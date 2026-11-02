//! Approval flow and user interaction during tool execution.
//!
//! Extracted from `tool_dispatch.rs` — handles the async request/response
//! dance for tool approvals and the `AskUser` tool. Both functions emit
//! an event via [`EngineSink`] and `select!` on the command channel,
//! respecting cancellation tokens for graceful shutdown.

use crate::engine::{ApprovalDecision, EngineCommand, EngineEvent};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// The engine's end of the command channel for one turn.
///
/// Steering text can arrive at any moment, but it can only join the
/// conversation between model calls: a user message can't sit between a tool
/// call and its result. So every read of the channel goes through here and
/// keeps the text in `steers`, which outlives the turn. What the turn didn't
/// use reaches the client through
/// [`KodaSession::take_queued_steers`](crate::session::KodaSession::take_queued_steers).
pub(crate) struct CommandInbox<'a> {
    rx: &'a mut mpsc::Receiver<EngineCommand>,
    steers: &'a mut Vec<String>,
}

impl<'a> CommandInbox<'a> {
    pub(crate) fn new(
        rx: &'a mut mpsc::Receiver<EngineCommand>,
        steers: &'a mut Vec<String>,
    ) -> Self {
        Self { rx, steers }
    }

    /// Wait for the next command that isn't steering text.
    ///
    /// Cancel safe, like [`mpsc::Receiver::recv`], so waits can `select!` on it.
    pub(crate) async fn recv(&mut self) -> Option<EngineCommand> {
        loop {
            match self.rx.recv().await {
                Some(EngineCommand::QueueNext { text }) => self.steers.push(text),
                other => return other,
            }
        }
    }

    /// Whether any steering text is waiting.
    pub(crate) fn has_steers(&mut self) -> bool {
        self.collect();
        !self.steers.is_empty()
    }

    /// Take the waiting steering text, oldest first.
    pub(crate) fn take_steers(&mut self) -> Vec<String> {
        self.collect();
        std::mem::take(self.steers)
    }

    /// Read the channel without waiting.
    ///
    /// Responses are only sent while a wait is reading the channel, so one
    /// seen here is stale and is discarded.
    fn collect(&mut self) {
        while let Ok(cmd) = self.rx.try_recv() {
            match cmd {
                EngineCommand::QueueNext { text } => self.steers.push(text),
                other => {
                    // #1232 §6: name the variant instead of dumping
                    // `Discriminant(N)` (which forces devs to grep the source
                    // to figure out which `EngineCommand` integer N maps to)
                    // — and use `.kind()` rather than `{:?}` so
                    // potentially-sensitive payload fields (e.g.
                    // `AskUserResponse.answer`, `QueueNext.text`) stay out of
                    // logs.
                    tracing::warn!(
                        "unexpected command outside a wait (discarded): {}",
                        other.kind()
                    );
                }
            }
        }
    }
}

/// Emit an `AskUserRequest` and wait for the user's typed response.
///
/// Returns `None` if the session was interrupted or cancelled.
pub(crate) async fn handle_ask_user(
    sink: &dyn crate::engine::EngineSink,
    cmd_rx: &mut CommandInbox<'_>,
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
                _ => continue, // ignore stale responses
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
    cmd_rx: &mut CommandInbox<'_>,
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
                _ => continue,        // ignore stale responses
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

    // ── CommandInbox ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn recv_keeps_steers_and_returns_the_next_command() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<EngineCommand>(8);
        let mut steers = Vec::new();
        let mut inbox = CommandInbox::new(&mut cmd_rx, &mut steers);
        for text in ["one", "two"] {
            let text = text.into();
            cmd_tx
                .send(EngineCommand::QueueNext { text })
                .await
                .unwrap();
        }
        cmd_tx.send(EngineCommand::Interrupt).await.unwrap();

        assert!(matches!(inbox.recv().await, Some(EngineCommand::Interrupt)));
        assert_eq!(inbox.take_steers(), ["one", "two"]);
    }

    #[tokio::test]
    async fn take_steers_keeps_arrival_order_across_recv_and_the_channel() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<EngineCommand>(8);
        let mut steers = Vec::new();
        let mut inbox = CommandInbox::new(&mut cmd_rx, &mut steers);
        let steer = |text: &str| EngineCommand::QueueNext { text: text.into() };
        cmd_tx.send(steer("waited")).await.unwrap();
        cmd_tx.send(EngineCommand::Interrupt).await.unwrap();
        cmd_tx.send(steer("not waited")).await.unwrap();

        inbox.recv().await;
        assert!(inbox.has_steers());
        assert_eq!(inbox.take_steers(), ["waited", "not waited"]);
        assert!(!inbox.has_steers());
    }

    #[tokio::test]
    async fn collecting_discards_a_stale_response() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<EngineCommand>(8);
        let mut steers = Vec::new();
        let mut inbox = CommandInbox::new(&mut cmd_rx, &mut steers);
        cmd_tx
            .send(EngineCommand::ApprovalResponse {
                id: "old".into(),
                decision: ApprovalDecision::Approve,
            })
            .await
            .unwrap();

        assert!(!inbox.has_steers());
        assert!(cmd_rx.is_empty());
    }

    #[tokio::test]
    async fn steers_outlive_the_inbox() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<EngineCommand>(8);
        let mut steers = Vec::new();
        cmd_tx
            .send(EngineCommand::QueueNext {
                text: "kept".into(),
            })
            .await
            .unwrap();
        cmd_tx.send(EngineCommand::Interrupt).await.unwrap();

        CommandInbox::new(&mut cmd_rx, &mut steers).recv().await;

        assert_eq!(steers, ["kept"]);
    }

    // ── handle_ask_user ────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ask_user_returns_answer() {
        let sink = Arc::new(TestSink::new());
        let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(8);
        let cancel = CancellationToken::new();
        let args = serde_json::json!({"question": "Pick one?", "options": ["a", "b"]});

        let sink2 = Arc::clone(&sink);
        let cancel2 = cancel.clone();
        let task = tokio::spawn(async move {
            let mut rx = cmd_rx;
            let mut steers = Vec::new();
            let mut rx = CommandInbox::new(&mut rx, &mut steers);
            handle_ask_user(&*sink2, &mut rx, &cancel2, &args).await
        });

        // Wait for AskUserRequest to be emitted, then reply with matching id.
        let id = sink
            .wait_for_map(Duration::from_secs(5), |e| match e {
                EngineEvent::AskUserRequest { id, .. } => Some(id.clone()),
                _ => None,
            })
            .await
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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
            let mut steers = Vec::new();
            let mut rx = CommandInbox::new(&mut rx, &mut steers);
            handle_ask_user(&*sink2, &mut rx, &cancel2, &args).await
        });

        let (q, opts) = sink
            .wait_for_map(Duration::from_secs(5), |e| match e {
                EngineEvent::AskUserRequest {
                    question, options, ..
                } => Some((question.clone(), options.clone())),
                _ => None,
            })
            .await
            .expect("no AskUserRequest emitted");
        assert_eq!(q, "Continue?");
        assert_eq!(opts, vec!["yes", "no"]);

        drop(cmd_tx);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ask_user_ignores_response_with_wrong_id() {
        let sink = Arc::new(TestSink::new());
        let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(8);
        let cancel = CancellationToken::new();
        let args = serde_json::json!({"question": "Q?", "options": []});

        let sink2 = Arc::clone(&sink);
        let cancel2 = cancel.clone();
        let task = tokio::spawn(async move {
            let mut rx = cmd_rx;
            let mut steers = Vec::new();
            let mut rx = CommandInbox::new(&mut rx, &mut steers);
            handle_ask_user(&*sink2, &mut rx, &cancel2, &args).await
        });

        let id = sink
            .wait_for_map(Duration::from_secs(5), |e| match e {
                EngineEvent::AskUserRequest { id, .. } => Some(id.clone()),
                _ => None,
            })
            .await
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ask_user_returns_none_on_interrupt() {
        let sink = Arc::new(TestSink::new());
        let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(8);
        let cancel = CancellationToken::new();
        let args = serde_json::json!({"question": "Q?", "options": []});

        let sink2 = Arc::clone(&sink);
        let cancel2 = cancel.clone();
        let task = tokio::spawn(async move {
            let mut rx = cmd_rx;
            let mut steers = Vec::new();
            let mut rx = CommandInbox::new(&mut rx, &mut steers);
            handle_ask_user(&*sink2, &mut rx, &cancel2, &args).await
        });

        // Wait for the spawned task to reach the command-receive loop
        // (proven by AskUserRequest emission) before sending the signal.
        sink.wait_for(Duration::from_secs(5), |e| {
            matches!(e, EngineEvent::AskUserRequest { .. })
        })
        .await
        .expect("task did not enter select loop");
        cmd_tx.send(EngineCommand::Interrupt).await.unwrap();
        assert_eq!(task.await.unwrap(), None);
        assert!(cancel.is_cancelled(), "interrupt should cancel the token");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ask_user_returns_none_when_channel_closes() {
        let sink = Arc::new(TestSink::new());
        let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(8);
        let cancel = CancellationToken::new();
        let args = serde_json::json!({"question": "Q?", "options": []});

        let sink2 = Arc::clone(&sink);
        let cancel2 = cancel.clone();
        let task = tokio::spawn(async move {
            let mut rx = cmd_rx;
            let mut steers = Vec::new();
            let mut rx = CommandInbox::new(&mut rx, &mut steers);
            handle_ask_user(&*sink2, &mut rx, &cancel2, &args).await
        });

        sink.wait_for(Duration::from_secs(5), |e| {
            matches!(e, EngineEvent::AskUserRequest { .. })
        })
        .await
        .expect("task did not enter select loop");
        drop(cmd_tx);
        assert_eq!(task.await.unwrap(), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ask_user_returns_none_on_cancellation() {
        let sink = Arc::new(TestSink::new());
        let (_cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(8);
        let cancel = CancellationToken::new();
        let args = serde_json::json!({"question": "Q?", "options": []});

        let sink2 = Arc::clone(&sink);
        let cancel2 = cancel.clone();
        let task = tokio::spawn(async move {
            let mut rx = cmd_rx;
            let mut steers = Vec::new();
            let mut rx = CommandInbox::new(&mut rx, &mut steers);
            handle_ask_user(&*sink2, &mut rx, &cancel2, &args).await
        });

        sink.wait_for(Duration::from_secs(5), |e| {
            matches!(e, EngineEvent::AskUserRequest { .. })
        })
        .await
        .expect("task did not enter select loop");
        cancel.cancel();
        assert_eq!(task.await.unwrap(), None);
    }

    // ── request_approval ───────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn request_approval_returns_approve() {
        let sink = Arc::new(TestSink::new());
        let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(8);
        let cancel = CancellationToken::new();

        let sink2 = Arc::clone(&sink);
        let cancel2 = cancel.clone();
        let task = tokio::spawn(async move {
            let mut rx = cmd_rx;
            let mut steers = Vec::new();
            let mut rx = CommandInbox::new(&mut rx, &mut steers);
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

        let id = sink
            .wait_for_map(Duration::from_secs(5), |e| match e {
                EngineEvent::ApprovalRequest { id, .. } => Some(id.clone()),
                _ => None,
            })
            .await
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn request_approval_returns_reject() {
        let sink = Arc::new(TestSink::new());
        let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(8);
        let cancel = CancellationToken::new();

        let sink2 = Arc::clone(&sink);
        let cancel2 = cancel.clone();
        let task = tokio::spawn(async move {
            let mut rx = cmd_rx;
            let mut steers = Vec::new();
            let mut rx = CommandInbox::new(&mut rx, &mut steers);
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

        let id = sink
            .wait_for_map(Duration::from_secs(5), |e| match e {
                EngineEvent::ApprovalRequest { id, .. } => Some(id.clone()),
                _ => None,
            })
            .await
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn request_approval_emits_event_with_tool_name_and_detail() {
        let sink = Arc::new(TestSink::new());
        let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(8);
        let cancel = CancellationToken::new();

        let sink2 = Arc::clone(&sink);
        let cancel2 = cancel.clone();
        let _task = tokio::spawn(async move {
            let mut rx = cmd_rx;
            let mut steers = Vec::new();
            let mut rx = CommandInbox::new(&mut rx, &mut steers);
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

        let (tool, detail) = sink
            .wait_for_map(Duration::from_secs(5), |e| match e {
                EngineEvent::ApprovalRequest {
                    tool_name, detail, ..
                } => Some((tool_name.clone(), detail.clone())),
                _ => None,
            })
            .await
            .expect("no ApprovalRequest emitted");
        assert_eq!(tool, "Edit");
        assert_eq!(detail, "replace line 42");

        drop(cmd_tx);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn request_approval_ignores_response_with_wrong_id() {
        let sink = Arc::new(TestSink::new());
        let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(8);
        let cancel = CancellationToken::new();

        let sink2 = Arc::clone(&sink);
        let cancel2 = cancel.clone();
        let task = tokio::spawn(async move {
            let mut rx = cmd_rx;
            let mut steers = Vec::new();
            let mut rx = CommandInbox::new(&mut rx, &mut steers);
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

        let id = sink
            .wait_for_map(Duration::from_secs(5), |e| match e {
                EngineEvent::ApprovalRequest { id, .. } => Some(id.clone()),
                _ => None,
            })
            .await
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn request_approval_returns_none_on_interrupt() {
        let sink = Arc::new(TestSink::new());
        let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(8);
        let cancel = CancellationToken::new();

        let sink2 = Arc::clone(&sink);
        let cancel2 = cancel.clone();
        let task = tokio::spawn(async move {
            let mut rx = cmd_rx;
            let mut steers = Vec::new();
            let mut rx = CommandInbox::new(&mut rx, &mut steers);
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

        sink.wait_for(Duration::from_secs(5), |e| {
            matches!(e, EngineEvent::ApprovalRequest { .. })
        })
        .await
        .expect("task did not enter select loop");
        cmd_tx.send(EngineCommand::Interrupt).await.unwrap();
        assert_eq!(task.await.unwrap(), None);
        assert!(cancel.is_cancelled());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn request_approval_returns_none_when_channel_closes() {
        let sink = Arc::new(TestSink::new());
        let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(8);
        let cancel = CancellationToken::new();

        let sink2 = Arc::clone(&sink);
        let cancel2 = cancel.clone();
        let task = tokio::spawn(async move {
            let mut rx = cmd_rx;
            let mut steers = Vec::new();
            let mut rx = CommandInbox::new(&mut rx, &mut steers);
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

        sink.wait_for(Duration::from_secs(5), |e| {
            matches!(e, EngineEvent::ApprovalRequest { .. })
        })
        .await
        .expect("task did not enter select loop");
        drop(cmd_tx);
        assert_eq!(task.await.unwrap(), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn request_approval_returns_none_on_cancellation() {
        let sink = Arc::new(TestSink::new());
        let (_cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(8);
        let cancel = CancellationToken::new();

        let sink2 = Arc::clone(&sink);
        let cancel2 = cancel.clone();
        let task = tokio::spawn(async move {
            let mut rx = cmd_rx;
            let mut steers = Vec::new();
            let mut rx = CommandInbox::new(&mut rx, &mut steers);
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

        sink.wait_for(Duration::from_secs(5), |e| {
            matches!(e, EngineEvent::ApprovalRequest { .. })
        })
        .await
        .expect("task did not enter select loop");
        cancel.cancel();
        assert_eq!(task.await.unwrap(), None);
    }
}
