//! Approval flow and user interaction during tool execution.
//!
//! Extracted from `tool_dispatch.rs` — handles the async request/response
//! dance for tool approvals and the `AskUser` tool.

use crate::engine::{ApprovalDecision, EngineCommand, EngineEvent};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Emit an `AskUserRequest` and wait for the user's typed response.
///
/// Returns `None` if the session was interrupted or cancelled.
pub(crate) async fn handle_ask_user(
    sink: &dyn crate::engine::EngineSink,
    cmd_rx: &mut mpsc::Receiver<EngineCommand>,
    cancel: &CancellationToken,
    args: &serde_json::Value,
) -> Option<String> {
    let question = args["question"].as_str().unwrap_or("").to_string();
    let options: Vec<String> = args["options"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let request_id = uuid::Uuid::new_v4().to_string();
    sink.emit(EngineEvent::AskUserRequest {
        id: request_id.clone(),
        question,
        options,
    });

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(EngineCommand::AskUserResponse { id, answer }) if id == request_id => {
                    return Some(answer);
                }
                Some(EngineCommand::Interrupt) => {
                    cancel.cancel();
                    return None;
                }
                None => return None,
                _ => continue,
            },
            _ = cancel.cancelled() => return None,
        }
    }
}

/// Emit an `ApprovalRequest` and wait for the user's decision.
///
/// Returns `None` if the session was interrupted or cancelled.
pub(crate) async fn request_approval(
    sink: &dyn crate::engine::EngineSink,
    cmd_rx: &mut mpsc::Receiver<EngineCommand>,
    cancel: &CancellationToken,
    tool_name: &str,
    detail: &str,
    preview: Option<crate::preview::DiffPreview>,
) -> Option<ApprovalDecision> {
    let approval_id = uuid::Uuid::new_v4().to_string();
    sink.emit(EngineEvent::ApprovalRequest {
        id: approval_id.clone(),
        tool_name: tool_name.to_string(),
        detail: detail.to_string(),
        preview,
    });

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(EngineCommand::ApprovalResponse { id, decision }) if id == approval_id => {
                    return Some(decision);
                }
                Some(EngineCommand::Interrupt) => {
                    cancel.cancel();
                    return None;
                }
                None => return None,  // channel closed
                _ => continue,        // ignore unrelated commands
            },
            _ = cancel.cancelled() => return None,
        }
    }
}
