//! MCP compatibility tests: legacy protocol servers and HTTP redirects.
//!
//! rmcp 3.x speaks MCP 2026-07-28, which drops `initialize` and sessions,
//! but most servers Koda connects to speak an older version. These tests
//! run `McpClient` against hand-rolled servers that answer the way older
//! servers do (not rmcp, so the wire shapes are independent of the SDK):
//!
//! - **Legacy HTTP:** a session-ful Streamable HTTP server answering
//!   `initialize` with 2025-03-26 or 2025-06-18.
//! - **Legacy stdio:** a `sh` script answering with 2024-11-05.
//! - **Redirects:** a cross-origin redirect must not carry the bearer token
//!   or custom headers to the new host (GHSA-9g45-5xwm-f3wc); a same-origin
//!   307 must still work.
//!
//! HTTP servers bind to `127.0.0.1:0`, so connections go through
//! `connect_http_without_ssrf_guard_for_test`, which is `connect` minus the
//! loopback block.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use koda_core::mcp::client::McpClient;
use koda_core::mcp::config::{McpServerConfig, McpTransport};
use serde_json::{Value, json};
use tokio::net::TcpListener;

const TOKEN: &str = "super-secret-test-token";
const API_KEY: &str = "super-secret-api-key";
const SESSION_ID: &str = "legacy-session-1";

/// The version Koda offers in `initialize` (`LATEST_WITH_INITIALIZE`).
const OFFERED_VERSION: &str = "2025-11-25";

// ── Hand-rolled legacy Streamable HTTP server ────────────────────────────────

/// One HTTP request as the server saw it.
#[derive(Debug, Clone)]
struct Seen {
    http_method: Method,
    rpc_method: Option<String>,
    offered_version: Option<String>,
    authorization: Option<String>,
    api_key: Option<String>,
    session_id: Option<String>,
    protocol_header: Option<String>,
}

type SeenLog = Arc<Mutex<Vec<Seen>>>;

#[derive(Clone)]
struct LegacyServer {
    /// Version answered to `initialize`, whatever the client offers.
    protocol_version: &'static str,
    seen: SeenLog,
}

/// A pre-2026 server: issues `Mcp-Session-Id` on `initialize`, rejects
/// later requests without it, answers JSON (no SSE), and has no GET stream.
async fn legacy_handler(
    State(server): State<LegacyServer>,
    method: Method,
    headers: HeaderMap,
    body: String,
) -> Response {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let message: Option<Value> = serde_json::from_str(&body).ok();
    let rpc_method = message
        .as_ref()
        .and_then(|m| m["method"].as_str())
        .map(str::to_owned);
    server.seen.lock().unwrap().push(Seen {
        http_method: method.clone(),
        rpc_method: rpc_method.clone(),
        offered_version: message
            .as_ref()
            .and_then(|m| m["params"]["protocolVersion"].as_str())
            .map(str::to_owned),
        authorization: header("authorization"),
        api_key: header("x-api-key"),
        session_id: header("mcp-session-id"),
        protocol_header: header("mcp-protocol-version"),
    });

    if method != Method::POST {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let (Some(message), Some(rpc_method)) = (message, rpc_method) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let is_initialize = rpc_method == "initialize";
    if !is_initialize && header("mcp-session-id").as_deref() != Some(SESSION_ID) {
        return (StatusCode::BAD_REQUEST, "missing or unknown session").into_response();
    }
    let Some(id) = message.get("id").cloned() else {
        // Notification.
        return StatusCode::ACCEPTED.into_response();
    };

    let result = match rpc_method.as_str() {
        "initialize" => json!({
            "protocolVersion": server.protocol_version,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "legacy-http", "version": "0.1.0" },
            "instructions": "legacy HTTP server",
        }),
        "tools/list" => json!({
            "tools": [{
                "name": "echo",
                "description": "Echo the message back",
                "inputSchema": {
                    "type": "object",
                    "properties": { "message": { "type": "string" } },
                    "required": ["message"],
                },
                "annotations": { "readOnlyHint": true },
            }],
        }),
        "tools/call" => json!({
            "content": [{
                "type": "text",
                "text": message["params"]["arguments"]["message"],
            }],
            "isError": false,
        }),
        other => {
            let error = json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("method not found: {other}") },
            });
            return json_response(error, false);
        }
    };
    json_response(
        json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        is_initialize,
    )
}

fn json_response(body: Value, with_session: bool) -> Response {
    let mut response = (
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response();
    if with_session {
        response
            .headers_mut()
            .insert("mcp-session-id", SESSION_ID.parse().unwrap());
    }
    response
}

fn legacy_router(path: &str, protocol_version: &'static str) -> (Router, SeenLog) {
    let seen = SeenLog::default();
    let server = LegacyServer {
        protocol_version,
        seen: Arc::clone(&seen),
    };
    let router = Router::new()
        .route(path, any(legacy_handler))
        .with_state(server);
    (router, seen)
}

async fn bind() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    (listener, addr)
}

/// Serves until the test's runtime shuts down.
fn serve(listener: TcpListener, router: Router) {
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
}

/// Redirects every request to `location` with `status`.
fn redirect_router(status: StatusCode, location: String) -> Router {
    Router::new().fallback(any(move || {
        let location = location.clone();
        async move { (status, [(header::LOCATION, location)]) }
    }))
}

fn http_config(url: String) -> McpServerConfig {
    McpServerConfig {
        transport: McpTransport::Http {
            url,
            bearer_token: Some(TOKEN.into()),
            headers: HashMap::from([("x-api-key".to_string(), API_KEY.to_string())]),
        },
        startup_timeout_sec: 5,
        tool_timeout_sec: 5,
        enabled_tools: None,
        disabled_tools: None,
    }
}

async fn connect_http(client: &mut McpClient) -> anyhow::Result<()> {
    tokio::time::timeout(
        Duration::from_secs(10),
        client.connect_http_without_ssrf_guard_for_test(),
    )
    .await
    .expect("connect must not hang")
}

/// Call `echo` and return the text it sent back.
async fn echo(client: &McpClient, message: &str) -> String {
    let result = client
        .call_tool("echo", json!({ "message": message }))
        .await
        .expect("tools/call should succeed");
    result.content[0]
        .as_text()
        .expect("echo returns text")
        .text
        .clone()
}

// ── Legacy protocol servers ──────────────────────────────────────────────────

/// Koda offers 2025-11-25, the server answers with an older version and a
/// session, and the whole lifecycle works on the server's terms.
async fn assert_legacy_http_server_works(protocol_version: &'static str) {
    let (listener, addr) = bind().await;
    let (router, seen) = legacy_router("/mcp", protocol_version);
    serve(listener, router);

    let mut client = McpClient::new("legacy".into(), http_config(format!("http://{addr}/mcp")));
    connect_http(&mut client)
        .await
        .unwrap_or_else(|e| panic!("{protocol_version} server should connect: {e:#}"));

    assert_eq!(client.instructions(), Some("legacy HTTP server"));
    let tool = client
        .tools()
        .iter()
        .find(|t| t.original_name == "echo")
        .expect("echo tool discovered");
    assert_eq!(tool.annotations.read_only_hint, Some(true));
    assert_eq!(echo(&client, "hi").await, "hi");

    let seen = seen.lock().unwrap().clone();
    let posts: Vec<&Seen> = seen
        .iter()
        .filter(|s| s.http_method == Method::POST)
        .collect();
    let methods: Vec<_> = posts.iter().map(|s| s.rpc_method.as_deref()).collect();
    assert_eq!(
        methods,
        [
            Some("initialize"),
            Some("notifications/initialized"),
            Some("tools/list"),
            Some("tools/call"),
        ],
        "legacy lifecycle: initialize handshake, no server/discover"
    );
    assert_eq!(posts[0].offered_version.as_deref(), Some(OFFERED_VERSION));
    for s in &posts[1..] {
        assert_eq!(s.session_id.as_deref(), Some(SESSION_ID), "{s:?}");
        assert_eq!(
            s.protocol_header.as_deref(),
            Some(protocol_version),
            "{s:?}"
        );
    }
    for s in &seen {
        assert_eq!(
            s.authorization.as_deref(),
            Some(format!("Bearer {TOKEN}").as_str()),
            "{s:?}"
        );
        assert_eq!(s.api_key.as_deref(), Some(API_KEY), "{s:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_http_server_2025_03_26() {
    assert_legacy_http_server_works("2025-03-26").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_http_server_2025_06_18() {
    assert_legacy_http_server_works("2025-06-18").await;
}

/// A 2024-11-05 stdio server. It answers whatever the client offers with
/// 2024-11-05 and echoes the offered version back in `instructions`.
#[cfg(unix)]
const LEGACY_STDIO_SERVER: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      offered=$(printf '%s\n' "$line" | sed -n 's/.*"protocolVersion":"\([^"]*\)".*/\1/p')
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"legacy-stdio","version":"0.1.0"},"instructions":"offered %s"}}\n' "$id" "$offered" ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"Echo the message back","inputSchema":{"type":"object","properties":{"message":{"type":"string"}},"required":["message"]}}]}}\n' "$id" ;;
    *'"method":"tools/call"'*)
      message=$(printf '%s\n' "$line" | sed -n 's/.*"message":"\([^"]*\)".*/\1/p')
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}]}}\n' "$id" "$message" ;;
  esac
done
"#;

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_stdio_server_2024_11_05() {
    let config = McpServerConfig {
        transport: McpTransport::Stdio {
            command: "sh".into(),
            args: vec!["-c".into(), LEGACY_STDIO_SERVER.into()],
            env: HashMap::new(),
            cwd: None,
        },
        startup_timeout_sec: 10,
        tool_timeout_sec: 10,
        enabled_tools: None,
        disabled_tools: None,
    };
    let mut client = McpClient::new("legacy".into(), config);
    client
        .connect()
        .await
        .expect("2024-11-05 stdio server should connect");

    assert_eq!(
        client.instructions(),
        Some(format!("offered {OFFERED_VERSION}").as_str())
    );
    assert!(client.tools().iter().any(|t| t.original_name == "echo"));
    assert_eq!(echo(&client, "hello stdio").await, "hello stdio");

    client.disconnect().await;
}

// ── Redirects (GHSA-9g45-5xwm-f3wc) ──────────────────────────────────────────

/// The configured origin redirects to a working MCP server on another
/// origin (another port). Koda must not follow, so that server never sees
/// a request, let alone the bearer token or `x-api-key`.
async fn assert_cross_origin_redirect_not_followed(status: StatusCode) {
    let (target_listener, target_addr) = bind().await;
    let (target_router, target_seen) = legacy_router("/mcp", "2025-06-18");
    serve(target_listener, target_router);

    let (listener, addr) = bind().await;
    serve(
        listener,
        redirect_router(status, format!("http://{target_addr}/mcp")),
    );

    let mut client = McpClient::new("redir".into(), http_config(format!("http://{addr}/mcp")));
    let result = connect_http(&mut client).await;

    assert!(
        result.is_err(),
        "{status} to another origin must fail the handshake"
    );
    let target_seen = target_seen.lock().unwrap().clone();
    assert!(
        target_seen.is_empty(),
        "{status}: redirect target must receive nothing, got {target_seen:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_origin_307_not_followed() {
    assert_cross_origin_redirect_not_followed(StatusCode::TEMPORARY_REDIRECT).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_origin_308_not_followed() {
    assert_cross_origin_redirect_not_followed(StatusCode::PERMANENT_REDIRECT).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_origin_302_not_followed() {
    assert_cross_origin_redirect_not_followed(StatusCode::FOUND).await;
}

/// Same-origin 307s still work (e.g. a server that moved `/mcp` to
/// `/v2/mcp`), with credentials intact on the new path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_origin_307_followed() {
    let (listener, addr) = bind().await;
    let (router, seen) = legacy_router("/v2/mcp", "2025-06-18");
    let router = router.route(
        "/mcp",
        any(move || async move {
            (
                StatusCode::TEMPORARY_REDIRECT,
                [(header::LOCATION, format!("http://{addr}/v2/mcp"))],
            )
        }),
    );
    serve(listener, router);

    let mut client = McpClient::new("moved".into(), http_config(format!("http://{addr}/mcp")));
    connect_http(&mut client)
        .await
        .expect("same-origin 307 should be followed");
    assert_eq!(echo(&client, "moved").await, "moved");

    let seen = seen.lock().unwrap().clone();
    assert!(!seen.is_empty());
    for s in &seen {
        assert_eq!(
            s.authorization.as_deref(),
            Some(format!("Bearer {TOKEN}").as_str()),
            "{s:?}"
        );
        assert_eq!(s.api_key.as_deref(), Some(API_KEY), "{s:?}");
    }
}
