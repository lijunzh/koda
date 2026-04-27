//! ACP (Agent Client Protocol) adapter — translates between Koda engine
//! events and the ACP JSON-RPC wire format.
//!
//! ## What it does
//!
//! - Maps `EngineEvent` → `SessionNotification` (outgoing to client)
//! - Maps ACP `EngineCommand` → internal `EngineCommand` (incoming from client)
//! - Maps Koda tool names → ACP `ToolKind` enum
//! - Handles ACP permission requests (tool approval over JSON-RPC)
//!
//! ## Why it's separate from `server.rs`
//!
//! `server.rs` owns the JSON-RPC transport (stdin/stdout framing).
//! This module owns the semantic translation between Koda's internal
//! event model and ACP's protocol schema. Neither knows about the other's
//! internals.

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
///
/// # Examples
///
/// ```ignore
/// // pub(crate): acp_adapter is not importable from a doc-test binary; illustrative only.
/// use agent_client_protocol_schema::ToolKind;
/// use koda_cli::acp_adapter::map_tool_kind;
///
/// assert_eq!(map_tool_kind("Read"),    ToolKind::Read);
/// assert_eq!(map_tool_kind("Write"),   ToolKind::Edit);
/// assert_eq!(map_tool_kind("Bash"),    ToolKind::Execute);
/// assert_eq!(map_tool_kind("Grep"),    ToolKind::Search);
/// assert_eq!(map_tool_kind("Delete"),  ToolKind::Delete);
/// assert_eq!(map_tool_kind("WebFetch"),ToolKind::Fetch);
/// assert_eq!(map_tool_kind("Think"),   ToolKind::Think);
/// // Unknown tools fall back to Other
/// assert_eq!(map_tool_kind("InvokeAgent"), ToolKind::Other);
/// ```
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

        // Streaming output lines — not mapped to ACP events (yet).
        EngineEvent::ToolOutputLine { .. } => None,

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

        // #1076: surface bg-task lifecycle as plain text chunks. ACP
        // doesn't have a typed bg-task notification today, so we render
        // a short `[bg task N] kind` line that ACP-aware IDEs can
        // either show inline or filter on the `[bg task N]` prefix.
        // The full structured payload is still on the wire for any
        // future typed mapping — see `EngineEvent::BgTaskUpdate`.
        EngineEvent::BgTaskUpdate {
            task_id, status, ..
        } => {
            let summary = match status {
                koda_core::bg_agent::AgentStatus::Pending => "pending".to_string(),
                koda_core::bg_agent::AgentStatus::Running { iter } => {
                    if *iter == 0 {
                        "running (starting)".to_string()
                    } else {
                        format!("running (iter {iter})")
                    }
                }
                koda_core::bg_agent::AgentStatus::Cancelled => "cancelled".to_string(),
                koda_core::bg_agent::AgentStatus::Completed { .. } => "completed".to_string(),
                koda_core::bg_agent::AgentStatus::Errored { error } => {
                    let snippet: String = error.chars().take(80).collect();
                    format!("errored: {snippet}")
                }
            };
            let cb = acp::ContentBlock::Text(acp::TextContent::new(format!(
                "[bg task {task_id}] {summary}"
            )));
            Some(acp::SessionNotification::new(
                session_id.to_string(),
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(cb)),
            ))
        }

        // (#1077 Phase A) TodoWrite lifecycle. Surfaces every accepted
        // change so ACP IDEs can render a checklist (with diff-driven
        // animation if they care). The diff (added / changed /
        // removed) and the full new list are both in the structured
        // payload, but ACP's `session/update` notifications carry text
        // chunks today — so we render a compact human-readable line
        // here. IDEs that want richer rendering can scan the
        // tool-result stream for `TodoWrite` outputs (the formatted
        // list lives there); the goal of this notification is parity
        // with `BgTaskUpdate` ("a transition happened, here's the
        // gist"), not to be the sole source of render data.
        EngineEvent::TodoUpdate { items, diff } => {
            let cb = acp::ContentBlock::Text(acp::TextContent::new(format!(
                "[todos] +{} ~{} -{} ({} total)",
                diff.added.len(),
                diff.changed.len(),
                diff.removed.len(),
                items.len(),
            )));
            Some(acp::SessionNotification::new(
                session_id.to_string(),
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(cb)),
            ))
        }

        // Handled specially by AcpSink (bidirectional permission flow)
        EngineEvent::ApprovalRequest { .. } => None,
        // AskUser not yet implemented in ACP protocol; filtered here.
        // AcpSink::emit auto-responds with an empty string (fallback).
        EngineEvent::AskUserRequest { .. } => None,

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
        EngineEvent::ContextUsage { .. } => None,
        EngineEvent::Footer { .. } => None,
        EngineEvent::SpinnerStart { .. } => None,
        EngineEvent::SpinnerStop => None,
        EngineEvent::TurnStart { .. } => None,
        EngineEvent::TurnEnd { .. } => None,
        EngineEvent::LoopCapReached { .. } => None,

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

        // Handle loop cap — server always auto-continues
        if matches!(event, EngineEvent::LoopCapReached { .. }) {
            let _ = self.cmd_tx.try_send(EngineCommand::LoopDecision {
                action: koda_core::loop_guard::LoopContinuation::Continue200,
            });
            return;
        }

        // AskUser: no ACP protocol support yet — auto-respond with empty string.
        if let EngineEvent::AskUserRequest { ref id, .. } = event {
            let _ = self.cmd_tx.try_send(EngineCommand::AskUserResponse {
                id: id.clone(),
                answer: String::new(),
            });
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

    // #1076: bg-task lifecycle must reach ACP clients as session
    // notifications.  Pre-fix the TUI was the only client that saw
    // bg status because it polled the registry directly; now every
    // transition flows through `EngineEvent::BgTaskUpdate` and lands
    // here as a text chunk an ACP-aware IDE can render or filter.
    #[test]
    fn test_bg_task_update_running_iter_zero_renders_starting() {
        let event = EngineEvent::BgTaskUpdate {
            task_id: 5,
            spawner: None,
            status: koda_core::bg_agent::AgentStatus::Running { iter: 0 },
        };
        let notif = engine_event_to_acp(&event, "s1").expect("must produce notification");
        match notif.update {
            acp::SessionUpdate::AgentMessageChunk(chunk) => match chunk.content {
                acp::ContentBlock::Text(t) => {
                    assert!(t.text.contains("[bg task 5]"), "got: {}", t.text);
                    assert!(t.text.contains("running (starting)"), "got: {}", t.text);
                }
                other => panic!("expected text content, got {other:?}"),
            },
            other => panic!("expected AgentMessageChunk, got {other:?}"),
        }
    }

    #[test]
    fn test_bg_task_update_renders_iter_count() {
        let event = EngineEvent::BgTaskUpdate {
            task_id: 12,
            spawner: Some(7),
            status: koda_core::bg_agent::AgentStatus::Running { iter: 4 },
        };
        let notif = engine_event_to_acp(&event, "s1").unwrap();
        let acp::SessionUpdate::AgentMessageChunk(chunk) = notif.update else {
            panic!("expected chunk");
        };
        let acp::ContentBlock::Text(t) = chunk.content else {
            panic!("expected text");
        };
        assert!(t.text.contains("running (iter 4)"), "got: {}", t.text);
    }

    #[test]
    fn test_bg_task_update_terminal_states_are_distinguishable() {
        // The four terminal-ish kinds must each render to a unique
        // string so an ACP client (or grep) can distinguish them.
        // Pending is included because it's the initial reservation
        // state — some clients may want to show "queued".
        let cases = [
            (koda_core::bg_agent::AgentStatus::Pending, "pending"),
            (koda_core::bg_agent::AgentStatus::Cancelled, "cancelled"),
            (
                koda_core::bg_agent::AgentStatus::Completed {
                    summary: "all good".into(),
                },
                "completed",
            ),
            (
                koda_core::bg_agent::AgentStatus::Errored {
                    error: "boom".into(),
                },
                "errored",
            ),
        ];
        for (status, marker) in cases {
            let event = EngineEvent::BgTaskUpdate {
                task_id: 1,
                spawner: None,
                status,
            };
            let notif = engine_event_to_acp(&event, "s1").unwrap();
            let acp::SessionUpdate::AgentMessageChunk(chunk) = notif.update else {
                panic!("expected chunk");
            };
            let acp::ContentBlock::Text(t) = chunk.content else {
                panic!("expected text");
            };
            assert!(
                t.text.contains(marker),
                "status missing {marker:?} marker, got: {}",
                t.text
            );
        }
    }

    // #1077 Phase A: TodoWrite lifecycle reaches ACP clients as a
    // session/update notification with a compact diff summary. Three
    // tests covering the dimensions a future ACP IDE renderer cares
    // about: counts, diff direction (added vs. removed vs. changed),
    // and the empty-diff suppression contract (which is enforced at
    // the dispatch layer in tools/mod.rs — see corresponding test
    // there). The tests below verify the *mapping* assuming the
    // dispatch layer has already chosen to emit.

    fn sample_todo(content: &str, status_str: &str) -> koda_core::tools::todo::TodoItem {
        use koda_core::tools::todo::{TodoItem, TodoPriority, TodoStatus};
        let status = match status_str {
            "pending" => TodoStatus::Pending,
            "in_progress" => TodoStatus::InProgress,
            "completed" => TodoStatus::Completed,
            other => panic!("unknown status {other}"),
        };
        TodoItem {
            content: content.into(),
            status,
            priority: TodoPriority::Medium,
        }
    }

    #[test]
    fn test_todo_update_first_write_renders_added_count() {
        use koda_core::tools::todo::TodoDiff;
        let items = vec![sample_todo("A", "pending"), sample_todo("B", "in_progress")];
        let diff = TodoDiff {
            added: items.clone(),
            ..Default::default()
        };
        let event = EngineEvent::TodoUpdate { items, diff };
        let notif = engine_event_to_acp(&event, "s1").expect("must produce notification");
        match notif.update {
            acp::SessionUpdate::AgentMessageChunk(chunk) => match chunk.content {
                acp::ContentBlock::Text(t) => {
                    assert!(t.text.contains("[todos]"), "got: {}", t.text);
                    assert!(t.text.contains("+2"), "expected +2 added, got: {}", t.text);
                    assert!(
                        t.text.contains("~0"),
                        "expected ~0 changed, got: {}",
                        t.text
                    );
                    assert!(
                        t.text.contains("-0"),
                        "expected -0 removed, got: {}",
                        t.text
                    );
                    assert!(t.text.contains("(2 total)"), "got: {}", t.text);
                }
                other => panic!("expected text content, got {other:?}"),
            },
            other => panic!("expected AgentMessageChunk, got {other:?}"),
        }
    }

    #[test]
    fn test_todo_update_status_change_renders_changed_count() {
        use koda_core::tools::todo::{TodoChange, TodoDiff};
        let before = sample_todo("A", "pending");
        let after = sample_todo("A", "in_progress");
        let items = vec![after.clone()];
        let diff = TodoDiff {
            changed: vec![TodoChange { before, after }],
            ..Default::default()
        };
        let event = EngineEvent::TodoUpdate { items, diff };
        let notif = engine_event_to_acp(&event, "s1").unwrap();
        let acp::SessionUpdate::AgentMessageChunk(chunk) = notif.update else {
            panic!("expected chunk");
        };
        let acp::ContentBlock::Text(t) = chunk.content else {
            panic!("expected text");
        };
        assert!(t.text.contains("+0"), "expected +0 added, got: {}", t.text);
        assert!(
            t.text.contains("~1"),
            "expected ~1 changed, got: {}",
            t.text
        );
        assert!(
            t.text.contains("-0"),
            "expected -0 removed, got: {}",
            t.text
        );
        assert!(t.text.contains("(1 total)"), "got: {}", t.text);
    }

    #[test]
    fn test_todo_update_clear_renders_removed_count() {
        use koda_core::tools::todo::TodoDiff;
        let removed = vec![sample_todo("A", "completed"), sample_todo("B", "completed")];
        let diff = TodoDiff {
            removed,
            ..Default::default()
        };
        let event = EngineEvent::TodoUpdate {
            items: Vec::new(),
            diff,
        };
        let notif = engine_event_to_acp(&event, "s1").unwrap();
        let acp::SessionUpdate::AgentMessageChunk(chunk) = notif.update else {
            panic!("expected chunk");
        };
        let acp::ContentBlock::Text(t) = chunk.content else {
            panic!("expected text");
        };
        assert!(
            t.text.contains("-2"),
            "expected -2 removed, got: {}",
            t.text
        );
        assert!(t.text.contains("(0 total)"), "got: {}", t.text);
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
                effect: koda_core::tools::ToolEffect::LocalMutation,
            },
            EngineEvent::AskUserRequest {
                id: "b".into(),
                question: "Which db?".into(),
                options: vec![],
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
            EngineEvent::TurnStart {
                turn_id: "t1".into(),
            },
            EngineEvent::TurnEnd {
                turn_id: "t1".into(),
                reason: koda_core::engine::event::TurnEndReason::Complete,
            },
            EngineEvent::LoopCapReached {
                cap: 200,
                recent_tools: vec![],
            },
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
