//! Approval flow and user interaction during tool execution.
//!
//! Extracted from `tool_dispatch.rs` — handles the async request/response
//! dance for tool approvals and the `AskUser` tool. Both functions emit
//! an event via [`EngineSink`] and `select!` on the command channel,
//! respecting cancellation tokens for graceful shutdown.

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
    effect: crate::tools::ToolEffect,
) -> Option<ApprovalDecision> {
    let approval_id = uuid::Uuid::new_v4().to_string();
    sink.emit(EngineEvent::ApprovalRequest {
        id: approval_id.clone(),
        tool_name: tool_name.to_string(),
        detail: detail.to_string(),
        preview,
        effect,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::sink::TestSink;
    use crate::tools::ToolEffect;
    use std::sync::Arc;
    use std::time::Duration;

    // ── handle_ask_user ────────────────────────────────────────────────────

    #[tokio::test]
    async fn ask_user_returns_answer() {
        let sink = Arc::new(TestSink::new());
        let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(8);
        let cancel = CancellationToken::new();
        let args = serde_json::json!({"question": "Pick one?", "options": ["a", "b"]});

        let sink2 = Arc::clone(&sink);
        let cancel2 = cancel.clone();
        let task = tokio::spawn(async move {
            let mut rx = cmd_rx;
            handle_ask_user(&*sink2, &mut rx, &cancel2, &args).await
        });

        // Wait for AskUserRequest to be emitted, then reply with matching id.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let id = sink
            .events()
            .into_iter()
            .find_map(|e| {
                if let EngineEvent::AskUserRequest { id, .. } = e {
                    Some(id)
                } else {
                    None
                }
            })
            .expect("AskUserRequest not emitted");

        cmd_tx
            .send(EngineCommand::AskUserResponse {
                id,
                answer: "b".into(),
            })
            .await
            .unwrap();

        assert_eq!(task.await.unwrap(), Some("b".to_string()));
    }

    #[tokio::test]
    async fn ask_user_emits_request_event_with_question_and_options() {
        let sink = Arc::new(TestSink::new());
        let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(8);
        let cancel = CancellationToken::new();
        let args = serde_json::json!({
            "question": "Continue?",
            "options": ["yes", "no"]
        });

        let sink2 = Arc::clone(&sink);
        let cancel2 = cancel.clone();
        let _task = tokio::spawn(async move {
            let mut rx = cmd_rx;
            handle_ask_user(&*sink2, &mut rx, &cancel2, &args).await
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        let events = sink.events();
        let req = events.iter().find_map(|e| {
            if let EngineEvent::AskUserRequest {
                question, options, ..
            } = e
            {
                Some((question.clone(), options.clone()))
            } else {
                None
            }
        });
        let (q, opts) = req.expect("no AskUserRequest emitted");
        assert_eq!(q, "Continue?");
        assert_eq!(opts, vec!["yes", "no"]);

        drop(cmd_tx);
    }

    #[tokio::test]
    async fn ask_user_ignores_response_with_wrong_id() {
        let sink = Arc::new(TestSink::new());
        let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(8);
        let cancel = CancellationToken::new();
        let args = serde_json::json!({"question": "Q?", "options": []});

        let sink2 = Arc::clone(&sink);
        let cancel2 = cancel.clone();
        let task = tokio::spawn(async move {
            let mut rx = cmd_rx;
            handle_ask_user(&*sink2, &mut rx, &cancel2, &args).await
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        let id = sink
            .events()
            .into_iter()
            .find_map(|e| {
                if let EngineEvent::AskUserRequest { id, .. } = e {
                    Some(id)
                } else {
                    None
                }
            })
            .unwrap();

        // Wrong id first — should be ignored.
        cmd_tx
            .send(EngineCommand::AskUserResponse {
                id: "wrong-id".into(),
                answer: "nope".into(),
            })
            .await
            .unwrap();

        // Correct id — should be accepted.
        cmd_tx
            .send(EngineCommand::AskUserResponse {
                id,
                answer: "correct".into(),
            })
            .await
            .unwrap();

        assert_eq!(task.await.unwrap(), Some("correct".to_string()));
    }

    #[tokio::test]
    async fn ask_user_returns_none_on_interrupt() {
        let sink = Arc::new(TestSink::new());
        let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(8);
        let cancel = CancellationToken::new();
        let args = serde_json::json!({"question": "Q?", "options": []});

        let sink2 = Arc::clone(&sink);
        let cancel2 = cancel.clone();
        let task = tokio::spawn(async move {
            let mut rx = cmd_rx;
            handle_ask_user(&*sink2, &mut rx, &cancel2, &args).await
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        cmd_tx.send(EngineCommand::Interrupt).await.unwrap();
        assert_eq!(task.await.unwrap(), None);
        assert!(cancel.is_cancelled(), "interrupt should cancel the token");
    }

    #[tokio::test]
    async fn ask_user_returns_none_when_channel_closes() {
        let sink = Arc::new(TestSink::new());
        let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(8);
        let cancel = CancellationToken::new();
        let args = serde_json::json!({"question": "Q?", "options": []});

        let sink2 = Arc::clone(&sink);
        let cancel2 = cancel.clone();
        let task = tokio::spawn(async move {
            let mut rx = cmd_rx;
            handle_ask_user(&*sink2, &mut rx, &cancel2, &args).await
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(cmd_tx);
        assert_eq!(task.await.unwrap(), None);
    }

    #[tokio::test]
    async fn ask_user_returns_none_on_cancellation() {
        let sink = Arc::new(TestSink::new());
        let (_cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(8);
        let cancel = CancellationToken::new();
        let args = serde_json::json!({"question": "Q?", "options": []});

        let sink2 = Arc::clone(&sink);
        let cancel2 = cancel.clone();
        let task = tokio::spawn(async move {
            let mut rx = cmd_rx;
            handle_ask_user(&*sink2, &mut rx, &cancel2, &args).await
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();
        assert_eq!(task.await.unwrap(), None);
    }

    // ── request_approval ───────────────────────────────────────────────────

    #[tokio::test]
    async fn request_approval_returns_approve() {
        let sink = Arc::new(TestSink::new());
        let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(8);
        let cancel = CancellationToken::new();

        let sink2 = Arc::clone(&sink);
        let cancel2 = cancel.clone();
        let task = tokio::spawn(async move {
            let mut rx = cmd_rx;
            request_approval(
                &*sink2,
                &mut rx,
                &cancel2,
                "Write",
                "overwrite main.rs",
                None,
                ToolEffect::LocalMutation,
            )
            .await
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        let id = sink
            .events()
            .into_iter()
            .find_map(|e| {
                if let EngineEvent::ApprovalRequest { id, .. } = e {
                    Some(id)
                } else {
                    None
                }
            })
            .expect("ApprovalRequest not emitted");

        cmd_tx
            .send(EngineCommand::ApprovalResponse {
                id,
                decision: ApprovalDecision::Approve,
            })
            .await
            .unwrap();

        assert_eq!(task.await.unwrap(), Some(ApprovalDecision::Approve));
    }

    #[tokio::test]
    async fn request_approval_returns_reject() {
        let sink = Arc::new(TestSink::new());
        let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(8);
        let cancel = CancellationToken::new();

        let sink2 = Arc::clone(&sink);
        let cancel2 = cancel.clone();
        let task = tokio::spawn(async move {
            let mut rx = cmd_rx;
            request_approval(
                &*sink2,
                &mut rx,
                &cancel2,
                "Bash",
                "rm -rf .",
                None,
                ToolEffect::Destructive,
            )
            .await
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        let id = sink
            .events()
            .into_iter()
            .find_map(|e| {
                if let EngineEvent::ApprovalRequest { id, .. } = e {
                    Some(id)
                } else {
                    None
                }
            })
            .unwrap();

        cmd_tx
            .send(EngineCommand::ApprovalResponse {
                id,
                decision: ApprovalDecision::Reject,
            })
            .await
            .unwrap();

        assert_eq!(task.await.unwrap(), Some(ApprovalDecision::Reject));
    }

    #[tokio::test]
    async fn request_approval_emits_event_with_tool_name_and_detail() {
        let sink = Arc::new(TestSink::new());
        let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(8);
        let cancel = CancellationToken::new();

        let sink2 = Arc::clone(&sink);
        let cancel2 = cancel.clone();
        let _task = tokio::spawn(async move {
            let mut rx = cmd_rx;
            request_approval(
                &*sink2,
                &mut rx,
                &cancel2,
                "Edit",
                "replace line 42",
                None,
                ToolEffect::LocalMutation,
            )
            .await
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        let events = sink.events();
        let req = events.iter().find_map(|e| {
            if let EngineEvent::ApprovalRequest {
                tool_name, detail, ..
            } = e
            {
                Some((tool_name.clone(), detail.clone()))
            } else {
                None
            }
        });
        let (tool, detail) = req.expect("no ApprovalRequest emitted");
        assert_eq!(tool, "Edit");
        assert_eq!(detail, "replace line 42");

        drop(cmd_tx);
    }

    #[tokio::test]
    async fn request_approval_ignores_response_with_wrong_id() {
        let sink = Arc::new(TestSink::new());
        let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(8);
        let cancel = CancellationToken::new();

        let sink2 = Arc::clone(&sink);
        let cancel2 = cancel.clone();
        let task = tokio::spawn(async move {
            let mut rx = cmd_rx;
            request_approval(
                &*sink2,
                &mut rx,
                &cancel2,
                "Write",
                "detail",
                None,
                ToolEffect::LocalMutation,
            )
            .await
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        let id = sink
            .events()
            .into_iter()
            .find_map(|e| {
                if let EngineEvent::ApprovalRequest { id, .. } = e {
                    Some(id)
                } else {
                    None
                }
            })
            .unwrap();

        // Wrong id ignored.
        cmd_tx
            .send(EngineCommand::ApprovalResponse {
                id: "wrong".into(),
                decision: ApprovalDecision::Reject,
            })
            .await
            .unwrap();

        // Correct id accepted.
        cmd_tx
            .send(EngineCommand::ApprovalResponse {
                id,
                decision: ApprovalDecision::Approve,
            })
            .await
            .unwrap();

        assert_eq!(task.await.unwrap(), Some(ApprovalDecision::Approve));
    }

    #[tokio::test]
    async fn request_approval_returns_none_on_interrupt() {
        let sink = Arc::new(TestSink::new());
        let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(8);
        let cancel = CancellationToken::new();

        let sink2 = Arc::clone(&sink);
        let cancel2 = cancel.clone();
        let task = tokio::spawn(async move {
            let mut rx = cmd_rx;
            request_approval(
                &*sink2,
                &mut rx,
                &cancel2,
                "Bash",
                "detail",
                None,
                ToolEffect::Destructive,
            )
            .await
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        cmd_tx.send(EngineCommand::Interrupt).await.unwrap();
        assert_eq!(task.await.unwrap(), None);
        assert!(cancel.is_cancelled());
    }

    #[tokio::test]
    async fn request_approval_returns_none_when_channel_closes() {
        let sink = Arc::new(TestSink::new());
        let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(8);
        let cancel = CancellationToken::new();

        let sink2 = Arc::clone(&sink);
        let cancel2 = cancel.clone();
        let task = tokio::spawn(async move {
            let mut rx = cmd_rx;
            request_approval(
                &*sink2,
                &mut rx,
                &cancel2,
                "Write",
                "detail",
                None,
                ToolEffect::LocalMutation,
            )
            .await
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(cmd_tx);
        assert_eq!(task.await.unwrap(), None);
    }

    #[tokio::test]
    async fn request_approval_returns_none_on_cancellation() {
        let sink = Arc::new(TestSink::new());
        let (_cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(8);
        let cancel = CancellationToken::new();

        let sink2 = Arc::clone(&sink);
        let cancel2 = cancel.clone();
        let task = tokio::spawn(async move {
            let mut rx = cmd_rx;
            request_approval(
                &*sink2,
                &mut rx,
                &cancel2,
                "Write",
                "detail",
                None,
                ToolEffect::LocalMutation,
            )
            .await
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();
        assert_eq!(task.await.unwrap(), None);
    }
}
