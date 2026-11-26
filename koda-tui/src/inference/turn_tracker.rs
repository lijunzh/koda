//! What the TUI keeps about a turn the host is running.
//!
//! The steering text sent during the turn, until the model receives it: the
//! host drops text a turn never delivers, so the TUI puts it back in the
//! input box itself, as Codex's TUI does. Each text goes with a client
//! message id, and the `userMessage` item the model received carries the
//! ids it delivered, which is how Codex's TUI matches them too. And the
//! turn's `turn/completed`, once it arrives.

use std::collections::VecDeque;

use koda_client::events::Outgoing;
use koda_client::protocol::ServerNotification;
use koda_client::protocol::item::Item;
use koda_client::protocol::turn::Turn;

/// One turn's undelivered steering text and how it ended.
#[derive(Debug, Default)]
pub(crate) struct TurnTracker {
    /// The turn's id, from `turn/start`.
    turn_id: String,
    /// Steering text sent and not yet received by the model, oldest first,
    /// with the client message id it went with.
    steers: VecDeque<(String, String)>,
    /// How many steers this turn has sent, for their ids.
    sent: usize,
    /// `turn/completed`, once it has arrived.
    completed: Option<Turn>,
    /// The model proposed a plan: one of its replies, not a sub-agent's,
    /// has a `<proposed_plan>` block.
    proposed_plan: bool,
    /// The model's last reply this turn, not a sub-agent's: the turn-ended
    /// notification shows its start.
    last_reply: Option<String>,
}

impl TurnTracker {
    pub(crate) fn new(turn_id: String) -> Self {
        Self {
            turn_id,
            ..Self::default()
        }
    }

    /// Take note of a protocol message. Messages about other turns don't
    /// touch this one.
    pub(crate) fn observe_message(&mut self, message: &Outgoing) {
        match message {
            Outgoing::Notification(ServerNotification::ItemCompleted(done))
                if done.turn_id == self.turn_id =>
            {
                if let Item::AgentMessage { text, .. } = &done.item
                    && done.parent_item_id.is_none()
                {
                    if crate::widgets::plan_menu::proposes_plan(text) {
                        self.proposed_plan = true;
                    }
                    self.last_reply = Some(text.clone());
                }
                if let Item::UserMessage {
                    client_message_ids, ..
                } = &done.item
                {
                    self.steers
                        .retain(|(id, _)| !client_message_ids.contains(id));
                }
            }
            Outgoing::Notification(ServerNotification::TurnCompleted(done))
                if done.turn.id == self.turn_id =>
            {
                self.completed = Some(done.turn.clone());
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

    /// The model's last reply this turn.
    pub(crate) fn last_reply(&self) -> Option<&str> {
        self.last_reply.as_deref()
    }

    /// Whether the model proposed a plan this turn.
    pub(crate) fn proposed_plan(&self) -> bool {
        self.proposed_plan
    }

    /// Keep `text`, about to go with `turn/steer`, until the model receives
    /// it. Returns the client message id to send it with.
    pub(crate) fn steer_sent(&mut self, text: String) -> String {
        self.sent += 1;
        let id = format!("{}-steer-{}", self.turn_id, self.sent);
        self.steers.push_back((id.clone(), text));
        id
    }

    /// Steering text the model never received, oldest first.
    pub(crate) fn take_unsent_steers(&mut self) -> Vec<String> {
        self.steers.drain(..).map(|(_, text)| text).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use koda_client::protocol::item::ItemCompletedNotification;
    use koda_client::protocol::turn::{TurnCompletedNotification, TurnStatus};

    fn delivered(turn: &str, ids: &[&str]) -> Outgoing {
        Outgoing::Notification(ServerNotification::ItemCompleted(
            ItemCompletedNotification {
                session_id: "s".into(),
                turn_id: turn.into(),
                parent_item_id: None,
                item: Item::UserMessage {
                    id: "user-2".into(),
                    text: "joined".into(),
                    client_message_ids: ids.iter().map(|id| id.to_string()).collect(),
                    mentions: Vec::new(),
                    images: Vec::new(),
                },
                completed_at_ms: 0,
            },
        ))
    }

    fn reply(turn: &str, parent: Option<&str>, text: &str) -> Outgoing {
        Outgoing::Notification(ServerNotification::ItemCompleted(
            ItemCompletedNotification {
                session_id: "s".into(),
                turn_id: turn.into(),
                parent_item_id: parent.map(str::to_string),
                item: Item::AgentMessage {
                    id: "m".into(),
                    text: text.into(),
                },
                completed_at_ms: 0,
            },
        ))
    }

    #[test]
    fn a_plan_is_the_model_s_own_in_this_turn() {
        let plan = "<proposed_plan>\n# Plan\n</proposed_plan>";
        let mut tracker = TurnTracker::new("t1".into());
        tracker.observe_message(&reply("t1", None, "Which database?"));
        tracker.observe_message(&reply("t1", Some("agent-call"), plan));
        tracker.observe_message(&reply("t0", None, plan));
        assert!(!tracker.proposed_plan());
        tracker.observe_message(&reply("t1", None, plan));
        assert!(tracker.proposed_plan());
    }

    #[test]
    fn steering_text_the_model_never_got_comes_back_in_order() {
        let mut tracker = TurnTracker::new("t1".into());
        let one = tracker.steer_sent("one".into());
        tracker.steer_sent("two".into());
        tracker.steer_sent("three".into());
        tracker.observe_message(&delivered("t1", &[&one]));
        assert_eq!(tracker.take_unsent_steers(), ["two", "three"]);
    }

    #[test]
    fn another_turns_delivery_is_not_this_ones() {
        let mut tracker = TurnTracker::new("t2".into());
        let one = tracker.steer_sent("one".into());
        tracker.observe_message(&delivered("t1", &[&one]));
        assert_eq!(tracker.take_unsent_steers(), ["one"]);
    }

    #[test]
    fn each_steer_has_its_own_id() {
        let mut tracker = TurnTracker::new("t1".into());
        let one = tracker.steer_sent("one".into());
        let two = tracker.steer_sent("two".into());
        assert_ne!(one, two);
        tracker.observe_message(&delivered("t1", &[&one, &two]));
        assert!(tracker.take_unsent_steers().is_empty());
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
                        started_at: None,
                        completed_at: None,
                        items: Vec::new(),
                    },
                },
            ))
        };
        tracker.observe_message(&done("t0"));
        assert!(tracker.completed().is_none());
        tracker.observe_message(&done("t1"));
        assert_eq!(tracker.completed().unwrap().id, "t1");
    }
}
