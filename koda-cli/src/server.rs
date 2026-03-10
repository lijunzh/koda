//! ACP server over stdio JSON-RPC.
//!
//! Reads newline-delimited JSON from stdin, writes JSON-RPC messages to stdout.
//! Implements the ACP lifecycle: Initialize → Authenticate → NewSession → Prompt → Cancel.

use acp::Side;
use agent_client_protocol_schema as acp;
use anyhow::Result;
use koda_cli::acp_adapter::{self, AcpOutgoing, PendingApproval};
use koda_core::agent::KodaAgent;
use koda_core::approval::ApprovalMode;
use koda_core::config::KodaConfig;
use koda_core::db::{Database, Role};
use koda_core::engine::EngineCommand;
use koda_core::persistence::Persistence;
use koda_core::session::KodaSession;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicI64;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// An active prompt session with its running task handle.
struct ActiveSession {
    session: KodaSession,
    cmd_tx: mpsc::Sender<EngineCommand>,
    cancel: CancellationToken,
}

/// Server state shared across the event loop.
struct ServerState {
    agent: Arc<KodaAgent>,
    config: KodaConfig,
    db: Database,
    project_root: PathBuf,
    active: Option<ActiveSession>,
    /// Maps outgoing JSON-RPC request IDs to engine approval IDs.
    pending_approvals: Arc<Mutex<HashMap<acp::RequestId, PendingApproval>>>,
    /// Counter for outgoing JSON-RPC request IDs.
    next_rpc_id: Arc<AtomicI64>,
}

/// Run the ACP server over stdio.
///
/// Reads newline-delimited JSON-RPC from stdin, dispatches to handlers,
/// and writes JSON-RPC responses/notifications to stdout.
pub async fn run_stdio_server(project_root: PathBuf, mut config: KodaConfig) -> Result<()> {
    // Initialize database
    let db = Database::init(&koda_core::db::config_dir()?).await?;

    // Query actual model capabilities before building agent
    let tmp_provider = koda_core::providers::create_provider(&config);
    config
        .query_and_apply_capabilities(tmp_provider.as_ref())
        .await;

    // Build agent (tools, MCP, system prompt)
    let agent = Arc::new(KodaAgent::new(&config, project_root.clone()).await?);

    let pending_approvals = Arc::new(Mutex::new(HashMap::new()));
    let next_rpc_id = Arc::new(AtomicI64::new(1));

    let mut state = ServerState {
        agent,
        config,
        db,
        project_root,
        active: None,
        pending_approvals,
        next_rpc_id,
    };

    // Channel for outgoing messages → stdout writer task
    let (out_tx, mut out_rx) = mpsc::channel::<String>(256);

    // Spawn stdout writer task
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let mut stdout = tokio::io::stdout();
        while let Some(line) = out_rx.recv().await {
            if stdout.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if stdout.write_all(b"\n").await.is_err() {
                break;
            }
            let _ = stdout.flush().await;
        }
    });

    // Read stdin line by line
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            // EOF — client disconnected
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Parse raw JSON to determine message type
        let raw: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let err =
                    make_error_response(acp::RequestId::Null, -32700, &format!("Parse error: {e}"));
                send_json(&out_tx, &err).await;
                continue;
            }
        };

        let has_method = raw.get("method").and_then(|m| m.as_str()).is_some();
        let has_id = raw.get("id").is_some();
        let has_result = raw.get("result").is_some();
        let has_error = raw.get("error").is_some();

        if has_method && has_id {
            // Request from client
            handle_request(&raw, &mut state, &out_tx).await;
        } else if has_method && !has_id {
            // Notification from client
            handle_notification(&raw, &mut state).await;
        } else if has_id && (has_result || has_error) {
            // Response to our outgoing request (permission response)
            handle_response(&raw, &mut state).await;
        } else {
            let err = make_error_response(acp::RequestId::Null, -32600, "Invalid JSON-RPC message");
            send_json(&out_tx, &err).await;
        }
    }

    Ok(())
}

/// Handle an incoming JSON-RPC request.
async fn handle_request(
    raw: &serde_json::Value,
    state: &mut ServerState,
    out_tx: &mpsc::Sender<String>,
) {
    let id = parse_request_id(raw);
    let method = raw["method"].as_str().unwrap_or("");

    // Extract params as RawValue for ACP decoder
    let params_raw = raw
        .get("params")
        .map(|p| serde_json::value::to_raw_value(p).unwrap());

    let decoded = acp::AgentSide::decode_request(method, params_raw.as_deref());

    let request = match decoded {
        Ok(r) => r,
        Err(e) => {
            let err = make_error_response(id, -32601, &format!("Unknown method '{method}': {e}"));
            send_json(out_tx, &err).await;
            return;
        }
    };

    match request {
        acp::ClientRequest::InitializeRequest(req) => {
            handle_initialize(id, req, out_tx).await;
        }
        acp::ClientRequest::AuthenticateRequest(_req) => {
            handle_authenticate(id, out_tx).await;
        }
        acp::ClientRequest::NewSessionRequest(req) => {
            handle_new_session(id, req, state, out_tx).await;
        }
        acp::ClientRequest::PromptRequest(req) => {
            handle_prompt(id, req, state, out_tx).await;
        }
        _ => {
            let err = make_error_response(
                id,
                -32601,
                &format!("Method '{method}' not yet implemented"),
            );
            send_json(out_tx, &err).await;
        }
    }
}

/// Handle an incoming JSON-RPC notification (no response expected).
async fn handle_notification(raw: &serde_json::Value, state: &mut ServerState) {
    let method = raw["method"].as_str().unwrap_or("");
    let params_raw = raw
        .get("params")
        .map(|p| serde_json::value::to_raw_value(p).unwrap());

    let decoded = acp::AgentSide::decode_notification(method, params_raw.as_deref());

    if let Ok(acp::ClientNotification::CancelNotification(_cancel)) = decoded
        && let Some(ref active) = state.active
    {
        active.cancel.cancel();
    }
}

/// Handle a JSON-RPC response (to our outgoing permission request).
async fn handle_response(raw: &serde_json::Value, state: &mut ServerState) {
    let rpc_id = parse_request_id(raw);

    // Check if this is a permission response
    if let Some(result) = raw.get("result")
        && let Ok(perm_resp) =
            serde_json::from_value::<acp::RequestPermissionResponse>(result.clone())
        && let Some(ref active) = state.active
    {
        acp_adapter::resolve_permission_response(
            &state.pending_approvals,
            &rpc_id,
            &perm_resp.outcome,
            &active.cmd_tx,
        );
    }
}

/// Handle `initialize` request.
async fn handle_initialize(
    id: acp::RequestId,
    req: acp::InitializeRequest,
    out_tx: &mpsc::Sender<String>,
) {
    let response = acp::InitializeResponse::new(req.protocol_version)
        .agent_info(acp::Implementation::new("koda", env!("CARGO_PKG_VERSION")));

    let resp = wrap_response(id, acp::AgentResponse::InitializeResponse(response));
    send_json(out_tx, &resp).await;
}

/// Handle `authenticate` request (no-op for local agent).
async fn handle_authenticate(id: acp::RequestId, out_tx: &mpsc::Sender<String>) {
    let response = acp::AuthenticateResponse::default();
    let resp = wrap_response(id, acp::AgentResponse::AuthenticateResponse(response));
    send_json(out_tx, &resp).await;
}

/// Handle `session/new` request.
async fn handle_new_session(
    id: acp::RequestId,
    _req: acp::NewSessionRequest,
    state: &mut ServerState,
    out_tx: &mpsc::Sender<String>,
) {
    let session_id = match state
        .db
        .create_session(&state.config.agent_name, &state.project_root)
        .await
    {
        Ok(sid) => sid,
        Err(e) => {
            let err = make_error_response(id, -32000, &format!("Failed to create session: {e}"));
            send_json(out_tx, &err).await;
            return;
        }
    };

    let (cmd_tx, _cmd_rx) = mpsc::channel::<EngineCommand>(32);
    let cancel = CancellationToken::new();

    let mut session = KodaSession::new(
        session_id.clone(),
        state.agent.clone(),
        state.db.clone(),
        &state.config,
        ApprovalMode::Auto,
    );
    session.skip_probe = true; // Server mode — probe at connection, not per-session

    state.active = Some(ActiveSession {
        session,
        cmd_tx,
        cancel,
    });

    let response = acp::NewSessionResponse::new(session_id);
    let resp = wrap_response(id, acp::AgentResponse::NewSessionResponse(response));
    send_json(out_tx, &resp).await;
}

/// Handle `session/prompt` request.
async fn handle_prompt(
    id: acp::RequestId,
    req: acp::PromptRequest,
    state: &mut ServerState,
    out_tx: &mpsc::Sender<String>,
) {
    // Extract text from prompt content blocks
    let mut text_parts = Vec::new();
    for block in &req.prompt {
        if let acp::ContentBlock::Text(tc) = block {
            text_parts.push(tc.text.clone());
        }
    }
    let user_text = text_parts.join("\n");

    // Ensure we have an active session
    let active = match state.active.as_mut() {
        Some(a) => a,
        None => {
            let err = make_error_response(id, -32000, "No active session. Call session/new first.");
            send_json(out_tx, &err).await;
            return;
        }
    };

    let session_id = active.session.id.clone();

    // Insert user message into DB
    if let Err(e) = active
        .session
        .db
        .insert_message(&session_id, &Role::User, Some(&user_text), None, None, None)
        .await
    {
        let err = make_error_response(id, -32000, &format!("Failed to insert message: {e}"));
        send_json(out_tx, &err).await;
        return;
    }

    // Create a fresh cancel token for this prompt
    active.cancel = CancellationToken::new();
    active.session.cancel = active.cancel.clone();

    // Create new cmd channel for this prompt
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<EngineCommand>(32);
    active.cmd_tx = cmd_tx.clone();

    // Create AcpSink
    let (acp_tx, mut acp_rx) = mpsc::channel::<AcpOutgoing>(256);
    let sink = acp_adapter::AcpSink::new(
        session_id,
        acp_tx,
        cmd_tx,
        state.pending_approvals.clone(),
        state.next_rpc_id.clone(),
    );

    // Spawn background task to stream ACP events to stdout
    let out_tx_events = out_tx.clone();
    let streaming_task = tokio::spawn(async move {
        while let Some(outgoing) = acp_rx.recv().await {
            let json = match &outgoing {
                AcpOutgoing::Notification(notification) => {
                    let msg = acp::OutgoingMessage::<acp::AgentSide, acp::ClientSide>::Notification(
                        acp::Notification {
                            method: "session/update".into(),
                            params: Some(acp::AgentNotification::SessionNotification(
                                notification.clone(),
                            )),
                        },
                    );
                    let wrapped = acp::JsonRpcMessage::wrap(msg);
                    serde_json::to_string(&wrapped).ok()
                }
                AcpOutgoing::PermissionRequest { rpc_id, request } => {
                    let msg = acp::OutgoingMessage::<acp::AgentSide, acp::ClientSide>::Request(
                        acp::Request {
                            id: rpc_id.clone(),
                            method: "session/request_permission".into(),
                            params: Some(acp::AgentRequest::RequestPermissionRequest(
                                request.clone(),
                            )),
                        },
                    );
                    let wrapped = acp::JsonRpcMessage::wrap(msg);
                    serde_json::to_string(&wrapped).ok()
                }
            };
            if let Some(json) = json {
                let _ = out_tx_events.send(json).await;
            }
        }
    });

    // Run inference on the current task (blocks stdin reading, but that's fine
    // for the initial single-session implementation)
    let active = state.active.as_mut().unwrap();
    let config = state.config.clone();
    let result = active
        .session
        .run_turn(&config, None, &sink, &mut cmd_rx)
        .await;

    // Drop the sink so the streaming task finishes
    drop(sink);
    let _ = streaming_task.await;

    // Determine stop reason
    let stop_reason = match result {
        Ok(()) => acp::StopReason::EndTurn,
        Err(_) => acp::StopReason::EndTurn,
    };

    let response = acp::PromptResponse::new(stop_reason);
    let resp = wrap_response(id, acp::AgentResponse::PromptResponse(response));
    send_json(out_tx, &resp).await;
}

// ── Helpers ─────────────────────────────────────────────────

/// Parse a JSON-RPC request ID from a raw JSON value.
fn parse_request_id(raw: &serde_json::Value) -> acp::RequestId {
    match raw.get("id") {
        Some(serde_json::Value::Number(n)) => acp::RequestId::Number(n.as_i64().unwrap_or(0)),
        Some(serde_json::Value::String(s)) => acp::RequestId::Str(s.clone()),
        Some(serde_json::Value::Null) | None => acp::RequestId::Null,
        _ => acp::RequestId::Null,
    }
}

/// Send a JSON string over the output channel.
async fn send_json(out_tx: &mpsc::Sender<String>, value: &serde_json::Value) {
    if let Ok(json) = serde_json::to_string(value) {
        let _ = out_tx.send(json).await;
    }
}

/// Wrap an ACP agent response into a JSON-RPC response value.
fn wrap_response(id: acp::RequestId, response: acp::AgentResponse) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": response,
    })
}

/// Create a JSON-RPC error response.
fn make_error_response(id: acp::RequestId, code: i32, message: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
        },
    })
}
