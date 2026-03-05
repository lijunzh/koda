use agent_client_protocol_schema as acp;
use koda_core::engine::sink::EngineSink;
use koda_core::engine::{ApprovalDecision, EngineCommand, EngineEvent};
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Outgoing messages from the ACP adapter — either session notifications or
/// permission requests (which are JSON-RPC requests the *agent* sends to the *client*).
#[derive(Debug, Clone)]
pub enum AcpOutgoing {
    Notification(acp::SessionNotification),
    PermissionRequest {
        rpc_id: acp::RequestId,
        request: acp::RequestPermissionRequest,
    },
}

/// Maps a Koda tool name to the ACP `ToolKind` enum.
pub fn map_tool_kind(name: &str) -> acp::ToolKind {
    match name {
        "Read" => acp::ToolKind::Read,
        "Write" | "Edit" | "NotebookEdit" => acp::ToolKind::Edit,
        "Bash" | "Shell" => acp::ToolKind::Execute,
        "Grep" | "Glob" => acp::ToolKind::Search,
        "Delete" => acp::ToolKind::Delete,
        "WebFetch" => acp::ToolKind::Fetch,
        "Think" => acp::ToolKind::Think,
        _ => acp::ToolKind::Other,
    }
}

/// Translates an internal `EngineEvent` to an ACP `SessionNotification`.
///
/// Returns `None` for events that have no ACP equivalent (UI-only signals)
/// or that are handled specially (e.g. `ApprovalRequest`).
pub fn engine_event_to_acp(
    event: &EngineEvent,
    session_id: &str,
) -> Option<acp::SessionNotification> {
    match event {
        EngineEvent::TextDelta { text } => {
            let cb = acp::ContentBlock::Text(acp::TextContent::new(text.clone()));
            Some(acp::SessionNotification::new(
                session_id.to_string(),
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(cb)),
            ))
        }
        EngineEvent::TextDone => None,
        EngineEvent::ThinkingStart => None,
        EngineEvent::ThinkingDelta { text } => {
            let cb = acp::ContentBlock::Text(acp::TextContent::new(text.clone()));
            Some(acp::SessionNotification::new(
                session_id.to_string(),
                acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(cb)),
            ))
        }
        EngineEvent::ThinkingDone => None,
        EngineEvent::ResponseStart => None,

        EngineEvent::ToolCallStart { id, name, args, .. } => {
            let tc = acp::ToolCall::new(id.clone(), name.clone())
                .kind(map_tool_kind(name))
                .status(acp::ToolCallStatus::InProgress)
                .raw_input(Some(args.clone()));
            Some(acp::SessionNotification::new(
                session_id.to_string(),
                acp::SessionUpdate::ToolCall(tc),
            ))
        }

        EngineEvent::ToolCallResult {
            id,
            name: _,
            output,
        } => {
            let content = vec![acp::ToolCallContent::Content(acp::Content::new(
                acp::ContentBlock::Text(acp::TextContent::new(output.clone())),
            ))];
            let fields = acp::ToolCallUpdateFields::new()
                .status(acp::ToolCallStatus::Completed)
                .content(content);
            let update = acp::ToolCallUpdate::new(id.clone(), fields);
            Some(acp::SessionNotification::new(
                session_id.to_string(),
                acp::SessionUpdate::ToolCallUpdate(update),
            ))
        }

        EngineEvent::SubAgentStart { agent_name } => {
            let tc = acp::ToolCall::new(agent_name.clone(), format!("Sub-agent: {agent_name}"))
                .kind(acp::ToolKind::Other)
                .status(acp::ToolCallStatus::InProgress);
            Some(acp::SessionNotification::new(
                session_id.to_string(),
                acp::SessionUpdate::ToolCall(tc),
            ))
        }

        EngineEvent::SubAgentEnd { agent_name } => {
            let fields = acp::ToolCallUpdateFields::new().status(acp::ToolCallStatus::Completed);
            let update = acp::ToolCallUpdate::new(agent_name.clone(), fields);
            Some(acp::SessionNotification::new(
                session_id.to_string(),
                acp::SessionUpdate::ToolCallUpdate(update),
            ))
        }

        // Handled specially by AcpSink (bidirectional permission flow)
        EngineEvent::ApprovalRequest { .. } => None,

        EngineEvent::ActionBlocked {
            tool_name: _,
            detail,
            ..
        } => {
            let fields = acp::ToolCallUpdateFields::new()
                .status(acp::ToolCallStatus::Failed)
                .title(format!("Blocked: {detail}"));
            let update = acp::ToolCallUpdate::new("blocked".to_string(), fields);
            Some(acp::SessionNotification::new(
                session_id.to_string(),
                acp::SessionUpdate::ToolCallUpdate(update),
            ))
        }

        EngineEvent::StatusUpdate { .. } => None,
        EngineEvent::Footer { .. } => None,
        EngineEvent::SpinnerStart { .. } => None,
        EngineEvent::SpinnerStop => None,
        EngineEvent::TodoDisplay { .. } => None,

        EngineEvent::Info { message } => {
            let cb = acp::ContentBlock::Text(acp::TextContent::new(format!("[info] {message}")));
            Some(acp::SessionNotification::new(
                session_id.to_string(),
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(cb)),
            ))
        }
        EngineEvent::Warn { message } => {
            let cb = acp::ContentBlock::Text(acp::TextContent::new(format!("[warn] {message}")));
            Some(acp::SessionNotification::new(
                session_id.to_string(),
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(cb)),
            ))
        }
        EngineEvent::Error { message } => {
            let cb = acp::ContentBlock::Text(acp::TextContent::new(format!("[error] {message}")));
            Some(acp::SessionNotification::new(
                session_id.to_string(),
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(cb)),
            ))
        }
    }
}

/// Pending approval context: maps an outgoing JSON-RPC request ID back to the
/// engine approval ID so we can route the client's response correctly.
pub struct PendingApproval {
    pub engine_approval_id: String,
}

/// ACP sink that translates EngineEvents to ACP messages and handles
/// the bidirectional approval flow.
pub struct AcpSink {
    session_id: String,
    tx: mpsc::Sender<AcpOutgoing>,
    /// Kept for future bidirectional approval flow where the server reads
    /// permission responses from stdin and routes them back to the engine.
    #[allow(dead_code)]
    cmd_tx: mpsc::Sender<EngineCommand>,
    pending_approvals: Arc<Mutex<HashMap<acp::RequestId, PendingApproval>>>,
    next_rpc_id: Arc<AtomicI64>,
}

impl AcpSink {
    pub fn new(
        session_id: String,
        tx: mpsc::Sender<AcpOutgoing>,
        cmd_tx: mpsc::Sender<EngineCommand>,
        pending_approvals: Arc<Mutex<HashMap<acp::RequestId, PendingApproval>>>,
        next_rpc_id: Arc<AtomicI64>,
    ) -> Self {
        Self {
            session_id,
            tx,
            cmd_tx,
            pending_approvals,
            next_rpc_id,
        }
    }
}

impl EngineSink for AcpSink {
    fn emit(&self, event: EngineEvent) {
        // Handle approval requests specially — they become outgoing JSON-RPC requests
        if let EngineEvent::ApprovalRequest {
            ref id,
            ref tool_name,
            ref detail,
            ..
        } = event
        {
            let rpc_id_num = self.next_rpc_id.fetch_add(1, Ordering::Relaxed);
            let rpc_id = acp::RequestId::Number(rpc_id_num);

            // Build the permission request
            let tc_fields = acp::ToolCallUpdateFields::new()
                .status(acp::ToolCallStatus::Pending)
                .title(detail.clone());
            let tc_update = acp::ToolCallUpdate::new(tool_name.clone(), tc_fields);

            let options = vec![
                acp::PermissionOption::new(
                    "approve",
                    "Approve",
                    acp::PermissionOptionKind::AllowOnce,
                ),
                acp::PermissionOption::new(
                    "reject",
                    "Reject",
                    acp::PermissionOptionKind::RejectOnce,
                ),
                acp::PermissionOption::new(
                    "always_allow",
                    "Always Allow",
                    acp::PermissionOptionKind::AllowAlways,
                ),
            ];

            let request =
                acp::RequestPermissionRequest::new(self.session_id.clone(), tc_update, options);

            // Store mapping so we can route the response back
            self.pending_approvals.lock().unwrap().insert(
                rpc_id.clone(),
                PendingApproval {
                    engine_approval_id: id.clone(),
                },
            );

            let _ = self
                .tx
                .try_send(AcpOutgoing::PermissionRequest { rpc_id, request });
            return;
        }

        // All other events go through the standard mapping
        if let Some(notification) = engine_event_to_acp(&event, &self.session_id) {
            let _ = self.tx.try_send(AcpOutgoing::Notification(notification));
        }
    }
}

/// Resolve an ACP permission response to an engine approval command.
/// Returns the `EngineCommand::ApprovalResponse` if the RPC ID matches a pending approval.
pub fn resolve_permission_response(
    pending_approvals: &Arc<Mutex<HashMap<acp::RequestId, PendingApproval>>>,
    rpc_id: &acp::RequestId,
    outcome: &acp::RequestPermissionOutcome,
    cmd_tx: &mpsc::Sender<EngineCommand>,
) -> bool {
    let pending = pending_approvals.lock().unwrap().remove(rpc_id);
    if let Some(approval) = pending {
        let decision = match outcome {
            acp::RequestPermissionOutcome::Cancelled => ApprovalDecision::Reject,
            acp::RequestPermissionOutcome::Selected(selected) => {
                match selected.option_id.0.as_ref() {
                    "approve" => ApprovalDecision::Approve,
                    "always_allow" => ApprovalDecision::AlwaysAllow,
                    _ => ApprovalDecision::Reject,
                }
            }
            _ => ApprovalDecision::Reject,
        };
        let _ = cmd_tx.try_send(EngineCommand::ApprovalResponse {
            id: approval.engine_approval_id,
            decision,
        });
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_text_delta() {
        let event = EngineEvent::TextDelta {
            text: "hello".into(),
        };
        let acp = engine_event_to_acp(&event, "session-1").unwrap();

        assert_eq!(acp.session_id, "session-1".to_string().into());
        match acp.update {
            acp::SessionUpdate::AgentMessageChunk(chunk) => {
                let block = chunk.content;
                match block {
                    acp::ContentBlock::Text(text_content) => {
                        assert_eq!(text_content.text, "hello");
                    }
                    _ => panic!("Expected text block"),
                }
            }
            _ => panic!("Expected AgentMessageChunk"),
        }
    }

    #[test]
    fn test_thinking_delta() {
        let event = EngineEvent::ThinkingDelta {
            text: "reasoning...".into(),
        };
        let acp = engine_event_to_acp(&event, "s1").unwrap();
        match acp.update {
            acp::SessionUpdate::AgentThoughtChunk(chunk) => match chunk.content {
                acp::ContentBlock::Text(tc) => assert_eq!(tc.text, "reasoning..."),
                _ => panic!("Expected text block"),
            },
            _ => panic!("Expected AgentThoughtChunk"),
        }
    }

    #[test]
    fn test_tool_call_start() {
        let event = EngineEvent::ToolCallStart {
            id: "call_1".into(),
            name: "Bash".into(),
            args: serde_json::json!({"command": "ls"}),
            is_sub_agent: false,
        };
        let acp = engine_event_to_acp(&event, "s1").unwrap();
        match acp.update {
            acp::SessionUpdate::ToolCall(tc) => {
                assert_eq!(tc.tool_call_id.0.as_ref(), "call_1");
                assert_eq!(tc.title, "Bash");
                assert_eq!(tc.kind, acp::ToolKind::Execute);
                assert_eq!(tc.status, acp::ToolCallStatus::InProgress);
            }
            _ => panic!("Expected ToolCall"),
        }
    }

    #[test]
    fn test_tool_call_result() {
        let event = EngineEvent::ToolCallResult {
            id: "call_1".into(),
            name: "Read".into(),
            output: "file contents".into(),
        };
        let acp = engine_event_to_acp(&event, "s1").unwrap();
        match acp.update {
            acp::SessionUpdate::ToolCallUpdate(update) => {
                assert_eq!(update.tool_call_id.0.as_ref(), "call_1");
                assert_eq!(update.fields.status, Some(acp::ToolCallStatus::Completed));
            }
            _ => panic!("Expected ToolCallUpdate"),
        }
    }

    #[test]
    fn test_sub_agent_start() {
        let event = EngineEvent::SubAgentStart {
            agent_name: "reviewer".into(),
        };
        let acp = engine_event_to_acp(&event, "s1").unwrap();
        match acp.update {
            acp::SessionUpdate::ToolCall(tc) => {
                assert_eq!(tc.tool_call_id.0.as_ref(), "reviewer");
                assert_eq!(tc.kind, acp::ToolKind::Other);
            }
            _ => panic!("Expected ToolCall"),
        }
    }

    #[test]
    fn test_sub_agent_end() {
        let event = EngineEvent::SubAgentEnd {
            agent_name: "reviewer".into(),
        };
        let acp = engine_event_to_acp(&event, "s1").unwrap();
        match acp.update {
            acp::SessionUpdate::ToolCallUpdate(update) => {
                assert_eq!(update.fields.status, Some(acp::ToolCallStatus::Completed));
            }
            _ => panic!("Expected ToolCallUpdate"),
        }
    }

    #[test]
    fn test_action_blocked() {
        let event = EngineEvent::ActionBlocked {
            tool_name: "Bash".into(),
            detail: "rm -rf /".into(),
            preview: None,
        };
        let acp = engine_event_to_acp(&event, "s1").unwrap();
        match acp.update {
            acp::SessionUpdate::ToolCallUpdate(update) => {
                assert_eq!(update.fields.status, Some(acp::ToolCallStatus::Failed));
                assert_eq!(update.fields.title, Some("Blocked: rm -rf /".to_string()));
            }
            _ => panic!("Expected ToolCallUpdate"),
        }
    }

    #[test]
    fn test_info_warn_error() {
        for (event, prefix) in [
            (
                EngineEvent::Info {
                    message: "hello".into(),
                },
                "[info]",
            ),
            (
                EngineEvent::Warn {
                    message: "watch out".into(),
                },
                "[warn]",
            ),
            (
                EngineEvent::Error {
                    message: "oops".into(),
                },
                "[error]",
            ),
        ] {
            let acp = engine_event_to_acp(&event, "s1").unwrap();
            match acp.update {
                acp::SessionUpdate::AgentMessageChunk(chunk) => match chunk.content {
                    acp::ContentBlock::Text(tc) => assert!(tc.text.starts_with(prefix)),
                    _ => panic!("Expected text block"),
                },
                _ => panic!("Expected AgentMessageChunk"),
            }
        }
    }

    #[test]
    fn test_none_events() {
        let none_events = vec![
            EngineEvent::TextDone,
            EngineEvent::ThinkingStart,
            EngineEvent::ThinkingDone,
            EngineEvent::ResponseStart,
            EngineEvent::ApprovalRequest {
                id: "a".into(),
                tool_name: "Bash".into(),
                detail: "cmd".into(),
                preview: None,
                whitelist_hint: None,
            },
            EngineEvent::StatusUpdate {
                model: "m".into(),
                provider: "p".into(),
                context_pct: 0.5,
                approval_mode: "normal".into(),
                active_tools: 0,
            },
            EngineEvent::Footer {
                prompt_tokens: 0,
                completion_tokens: 0,
                cache_read_tokens: 0,
                thinking_tokens: 0,
                total_chars: 0,
                elapsed_ms: 0,
                rate: 0.0,
                context: String::new(),
            },
            EngineEvent::SpinnerStart {
                message: "x".into(),
            },
            EngineEvent::SpinnerStop,
        ];
        for event in none_events {
            assert!(
                engine_event_to_acp(&event, "s1").is_none(),
                "Expected None for {event:?}"
            );
        }
    }

    #[test]
    fn test_map_tool_kind() {
        assert_eq!(map_tool_kind("Read"), acp::ToolKind::Read);
        assert_eq!(map_tool_kind("Write"), acp::ToolKind::Edit);
        assert_eq!(map_tool_kind("Edit"), acp::ToolKind::Edit);
        assert_eq!(map_tool_kind("Bash"), acp::ToolKind::Execute);
        assert_eq!(map_tool_kind("Grep"), acp::ToolKind::Search);
        assert_eq!(map_tool_kind("Glob"), acp::ToolKind::Search);
        assert_eq!(map_tool_kind("Delete"), acp::ToolKind::Delete);
        assert_eq!(map_tool_kind("WebFetch"), acp::ToolKind::Fetch);
        assert_eq!(map_tool_kind("Think"), acp::ToolKind::Think);
        assert_eq!(map_tool_kind("Unknown"), acp::ToolKind::Other);
    }
}
