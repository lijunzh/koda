//! What the TUI keeps about a turn the host is running.
//!
//! The TUI's dialogs read the engine's own events, which name a request by
//! the engine's id; the host asks with protocol requests, which name the
//! tool call (`itemId`). [`TurnTracker`] links the two, so an answer the
//! TUI gives as an [`EngineCommand`] goes back as the answer to the right
//! request. It also keeps the steering text sent during the turn until the
//! model receives it: the host drops text a turn never delivers, so the TUI
//! puts it back in the input box itself, as Codex's TUI does.

use std::collections::{HashMap, VecDeque};

use koda_client::events::Outgoing;
use koda_client::protocol::item::{self, ToolCallApprovalResponse, UserInputResponse};
use koda_client::protocol::rpc::RequestId;
use koda_client::protocol::turn::{LoopCapDecision, LoopCapResponse, Turn};
use koda_client::protocol::{ServerNotification, ServerRequest};
use koda_core::engine::{ApprovalDecision, EngineCommand, EngineEvent, TurnEndReason};
use koda_core::loop_guard::LoopContinuation;
use serde_json::Value;

/// One turn's links between engine requests and protocol requests, its
/// undelivered steering text, and how it ended.
#[derive(Debug, Default)]
pub(crate) struct TurnTracker {
    /// The turn's id, from `turn/start`.
    turn_id: String,
    /// Engine request id → the tool call it is about.
    engine_items: HashMap<String, String>,
    /// Tool call → the protocol request asking about it.
    item_requests: HashMap<String, RequestId>,
    /// The loop cap's engine id and protocol request; one at a time.
    loop_cap_engine: Option<String>,
    loop_cap_request: Option<RequestId>,
    /// Answers given before their protocol request was read.
    waiting: Vec<EngineCommand>,
    /// Steering text sent and not yet received by the model, oldest first.
    steers: VecDeque<String>,
    /// The engine ended the turn with an error, which it has shown already.
    error_shown: bool,
    /// `turn/completed`, once it has arrived.
    completed: Option<Turn>,
}

impl TurnTracker {
    pub(crate) fn new(turn_id: String) -> Self {
        Self {
            turn_id,
            ..Self::default()
        }
    }

    /// Take note of an engine event on its way to the renderer.
    pub(crate) fn observe_event(&mut self, event: &EngineEvent) {
        match event {
            EngineEvent::ApprovalRequest {
                id, tool_call_id, ..
            }
            | EngineEvent::AskUserRequest {
                id, tool_call_id, ..
            } => {
                self.engine_items.insert(id.clone(), tool_call_id.clone());
            }
            EngineEvent::LoopCapReached { id, .. } => self.loop_cap_engine = Some(id.clone()),
            EngineEvent::SteerDelivered { texts } => {
                for _ in texts {
                    self.steers.pop_front();
                }
            }
            EngineEvent::TurnEnd {
                reason: TurnEndReason::Error { .. },
                ..
            } => self.error_shown = true,
            _ => {}
        }
    }

    /// Take note of a protocol message. Messages about other turns or
    /// sessions don't touch this one.
    pub(crate) fn observe_message(&mut self, message: Outgoing) {
        match message {
            Outgoing::Request(ServerRequest::ToolCallApproval { id, params })
                if params.turn_id == self.turn_id =>
            {
                self.item_requests.insert(params.item_id, id);
            }
            Outgoing::Request(ServerRequest::UserInput { id, params })
                if params.turn_id == self.turn_id =>
            {
                self.item_requests.insert(params.item_id, id);
            }
            Outgoing::Request(ServerRequest::LoopCap { id, params })
                if params.turn_id == self.turn_id =>
            {
                self.loop_cap_request = Some(id);
            }
            Outgoing::Notification(ServerNotification::TurnCompleted(done))
                if done.turn.id == self.turn_id =>
            {
                self.completed = Some(done.turn);
            }
            _ => {}
        }
    }

    /// The turn's id.
    pub(crate) fn turn_id(&self) -> &str {
        &self.turn_id
    }

    /// The turn's `turn/completed`, once it has arrived.
    pub(crate) fn completed(&self) -> Option<&Turn> {
        self.completed.as_ref()
    }

    /// Whether the engine already showed the error the turn ended with.
    pub(crate) fn error_shown(&self) -> bool {
        self.error_shown
    }

    /// Keep `text`, sent with `turn/steer`, until the model receives it.
    pub(crate) fn steer_sent(&mut self, text: String) {
        self.steers.push_back(text);
    }

    /// Steering text the model never received, oldest first.
    pub(crate) fn take_unsent_steers(&mut self) -> Vec<String> {
        self.steers.drain(..).collect()
    }

    /// Queue `answer` until its request is known; [`Self::ready_answers`]
    /// hands it out then.
    pub(crate) fn answer(&mut self, answer: EngineCommand) {
        self.waiting.push(answer);
    }

    /// The answers whose request is known, as the request's id and its
    /// response. The rest keep waiting.
    pub(crate) fn ready_answers(&mut self) -> Vec<(RequestId, Value)> {
        let mut ready = Vec::new();
        for answer in std::mem::take(&mut self.waiting) {
            match self.response(&answer) {
                Some(response) => ready.push(response),
                None => self.waiting.push(answer),
            }
        }
        ready
    }

    /// The protocol request `answer` answers, and the response.
    fn response(&mut self, answer: &EngineCommand) -> Option<(RequestId, Value)> {
        let by_item = |tracker: &mut Self, engine_id: &str| {
            let item = tracker.engine_items.get(engine_id)?;
            let request = tracker.item_requests.remove(item)?;
            tracker.engine_items.remove(engine_id);
            Some(request)
        };
        match answer {
            EngineCommand::ApprovalResponse { id, decision } => {
                let decision = match decision {
                    ApprovalDecision::Approve => item::ApprovalDecision::Approve,
                    ApprovalDecision::ApproveForSession => {
                        item::ApprovalDecision::ApproveForSession
                    }
                    ApprovalDecision::RejectWithFeedback { feedback } => {
                        item::ApprovalDecision::RejectWithFeedback {
                            feedback: feedback.clone(),
                        }
                    }
                    _ => item::ApprovalDecision::Reject,
                };
                let request = by_item(self, id)?;
                let response = serde_json::to_value(ToolCallApprovalResponse { decision }).ok()?;
                Some((request, response))
            }
            EngineCommand::AskUserResponse { id, answer } => {
                let request = by_item(self, id)?;
                let response = serde_json::to_value(UserInputResponse {
                    answer: answer.clone(),
                })
                .ok()?;
                Some((request, response))
            }
            EngineCommand::LoopDecision { id, action } => {
                if self.loop_cap_engine.as_deref() != Some(id) {
                    return None;
                }
                let request = self.loop_cap_request.take()?;
                self.loop_cap_engine = None;
                let decision = match action {
                    LoopContinuation::Continue50 => LoopCapDecision::Continue50,
                    LoopContinuation::Continue200 => LoopCapDecision::Continue200,
                    _ => LoopCapDecision::Stop,
                };
                let response = serde_json::to_value(LoopCapResponse { decision }).ok()?;
                Some((request, response))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use koda_client::protocol::item::{ToolCallApprovalParams, ToolEffect};
    use koda_client::protocol::turn::{LoopCapParams, TurnCompletedNotification, TurnStatus};

    fn approval_event(engine_id: &str, call: &str) -> EngineEvent {
        EngineEvent::ApprovalRequest {
            id: engine_id.into(),
            tool_call_id: call.into(),
            tool_name: "Write".into(),
            detail: "a.txt".into(),
            preview: None,
            effect: koda_core::tools::ToolEffect::LocalMutation,
            allow_for_session: true,
        }
    }

    fn approval_request(id: i64, turn: &str, call: &str) -> Outgoing {
        Outgoing::Request(ServerRequest::ToolCallApproval {
            id: RequestId::Integer(id),
            params: ToolCallApprovalParams {
                session_id: "s".into(),
                turn_id: turn.into(),
                item_id: call.into(),
                tool: "Write".into(),
                detail: "a.txt".into(),
                effect: ToolEffect::LocalMutation,
                allow_for_session: true,
            },
        })
    }

    #[test]
    fn an_answer_goes_to_the_request_for_its_tool_call() {
        let mut tracker = TurnTracker::new("t1".into());
        tracker.observe_event(&approval_event("e1", "call_1"));
        tracker.observe_message(approval_request(7, "t1", "call_1"));
        tracker.answer(EngineCommand::ApprovalResponse {
            id: "e1".into(),
            decision: ApprovalDecision::ApproveForSession,
        });
        let ready = tracker.ready_answers();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].0, RequestId::Integer(7));
        assert_eq!(ready[0].1["decision"]["type"], "approveForSession");
        assert!(tracker.ready_answers().is_empty(), "answered once");
    }

    #[test]
    fn an_answer_waits_for_a_request_not_read_yet() {
        let mut tracker = TurnTracker::new("t1".into());
        tracker.observe_event(&approval_event("e1", "call_1"));
        tracker.answer(EngineCommand::ApprovalResponse {
            id: "e1".into(),
            decision: ApprovalDecision::RejectWithFeedback {
                feedback: "not that file".into(),
            },
        });
        assert!(tracker.ready_answers().is_empty());
        tracker.observe_message(approval_request(3, "t1", "call_1"));
        let ready = tracker.ready_answers();
        assert_eq!(ready[0].0, RequestId::Integer(3));
        assert_eq!(ready[0].1["decision"]["type"], "rejectWithFeedback");
        assert_eq!(ready[0].1["decision"]["feedback"], "not that file");
    }

    #[test]
    fn another_turns_request_is_not_this_ones() {
        let mut tracker = TurnTracker::new("t2".into());
        tracker.observe_event(&approval_event("e1", "call_1"));
        tracker.observe_message(approval_request(1, "t1", "call_1"));
        tracker.answer(EngineCommand::ApprovalResponse {
            id: "e1".into(),
            decision: ApprovalDecision::Approve,
        });
        assert!(tracker.ready_answers().is_empty());
    }

    #[test]
    fn the_loop_cap_answer_goes_to_the_loop_cap_request() {
        let mut tracker = TurnTracker::new("t1".into());
        tracker.observe_event(&EngineEvent::LoopCapReached {
            id: "c1".into(),
            cap: 200,
            recent_tools: vec![],
        });
        tracker.observe_message(Outgoing::Request(ServerRequest::LoopCap {
            id: RequestId::Integer(9),
            params: LoopCapParams {
                session_id: "s".into(),
                turn_id: "t1".into(),
                cap: 200,
                recent_tools: vec![],
            },
        }));
        tracker.answer(EngineCommand::LoopDecision {
            id: "c1".into(),
            action: LoopContinuation::Stop,
        });
        let ready = tracker.ready_answers();
        assert_eq!(ready[0].0, RequestId::Integer(9));
        assert_eq!(ready[0].1["decision"], "stop");
    }

    #[test]
    fn steering_text_the_model_never_got_comes_back_in_order() {
        let mut tracker = TurnTracker::new("t1".into());
        tracker.steer_sent("one".into());
        tracker.steer_sent("two".into());
        tracker.steer_sent("three".into());
        tracker.observe_event(&EngineEvent::SteerDelivered {
            texts: vec!["one".into()],
        });
        assert_eq!(tracker.take_unsent_steers(), ["two", "three"]);
    }

    #[test]
    fn turn_completed_is_kept_for_this_turn_only() {
        let mut tracker = TurnTracker::new("t1".into());
        let done = |id: &str| {
            Outgoing::Notification(ServerNotification::TurnCompleted(
                TurnCompletedNotification {
                    session_id: "s".into(),
                    turn: Turn {
                        id: id.into(),
                        status: TurnStatus::Completed,
                        error: None,
                        usage: None,
                        duration_ms: None,
                    },
                },
            ))
        };
        tracker.observe_message(done("t0"));
        assert!(tracker.completed().is_none());
        tracker.observe_message(done("t1"));
        assert_eq!(tracker.completed().unwrap().id, "t1");
    }
}
