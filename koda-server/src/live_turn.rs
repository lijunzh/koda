//! The session's latest turn as its clients have been told of it, kept in
//! memory for a client that joins while the session is open.
//!
//! The store has a turn's completed items only, and those a moment late.
//! A client that joins mid-turn needs the turn as it stands: the items
//! that completed, and the ones still open, a message with its text so
//! far and a tool call with its notices, as Codex's `thread/resume` merges
//! a thread's active turn into its answer. Built from the same messages
//! the clients get, so it can't say anything they weren't told.

use koda_protocol::ServerNotification;
use koda_protocol::item::{Item, TurnItem};
use koda_protocol::turn::Turn;

use crate::events::Outgoing;

/// The latest turn of this opening of the session: running, or ended and
/// not yet followed by another.
#[derive(Debug, Clone)]
pub(crate) struct LiveTurn {
    /// The turn as last announced, without its items.
    turn: Turn,
    /// Its completed items, in the order they completed.
    completed: Vec<TurnItem>,
    /// Its items that started and haven't completed, in the order they
    /// started, each as it stands.
    open: Vec<TurnItem>,
}

impl LiveTurn {
    /// Take note of one message sent to the session's clients. Returns the
    /// turn to keep: a `turn/started` begins a new one.
    pub(crate) fn observe(latest: &mut Option<Self>, message: &Outgoing) {
        let Outgoing::Notification(notification) = message else {
            return;
        };
        if let ServerNotification::TurnStarted(started) = notification {
            *latest = Some(Self {
                turn: Turn {
                    items: Vec::new(),
                    ..started.turn.clone()
                },
                completed: Vec::new(),
                open: Vec::new(),
            });
            return;
        }
        let Some(live) = latest.as_mut() else {
            return;
        };
        match notification {
            ServerNotification::TurnCompleted(ended) if ended.turn.id == live.turn.id => {
                live.turn = Turn {
                    items: Vec::new(),
                    ..ended.turn.clone()
                };
                // Whatever it left open ended with it.
                live.open.clear();
            }
            ServerNotification::ItemStarted(n) if n.turn_id == live.turn.id => {
                live.open.push(TurnItem {
                    parent_item_id: n.parent_item_id.clone(),
                    item: n.item.clone(),
                    started_at_ms: Some(n.started_at_ms),
                    completed_at_ms: None,
                });
            }
            ServerNotification::ItemCompleted(n) if n.turn_id == live.turn.id => {
                let started_at_ms = live
                    .open
                    .iter()
                    .position(|open| open.item.id() == n.item.id())
                    .and_then(|at| live.open.remove(at).started_at_ms);
                live.completed.push(TurnItem {
                    parent_item_id: n.parent_item_id.clone(),
                    item: n.item.clone(),
                    started_at_ms,
                    completed_at_ms: Some(n.completed_at_ms),
                });
            }
            ServerNotification::AgentMessageDelta(delta)
            | ServerNotification::ReasoningTextDelta(delta) => {
                if let Some(Item::AgentMessage { text, .. } | Item::Reasoning { text, .. }) =
                    live.open_item(&delta.item_id)
                {
                    text.push_str(&delta.delta);
                }
            }
            ServerNotification::ToolCallProgress(notice) => {
                if let Some(Item::ToolCall { progress, .. }) = live.open_item(&notice.item_id) {
                    progress.push(notice.message.clone());
                }
            }
            _ => {}
        }
    }

    fn open_item(&mut self, id: &str) -> Option<&mut Item> {
        self.open
            .iter_mut()
            .map(|open| &mut open.item)
            .find(|item| item.id() == id)
    }

    /// The turn as it stands: its completed items, then the open ones.
    pub(crate) fn turn(&self) -> Turn {
        Turn {
            items: self.completed.iter().chain(&self.open).cloned().collect(),
            ..self.turn.clone()
        }
    }

    /// `turns`, the session's stored turns, with this one as it stands in
    /// place of what the store has of it, or after them if the store has
    /// none of it yet.
    pub(crate) fn over(&self, turns: &mut Vec<Turn>) {
        let turn = self.turn();
        match turns.iter_mut().find(|stored| stored.id == turn.id) {
            Some(stored) => *stored = turn,
            None => turns.push(turn),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use koda_core::engine::event::ToolOutcome;
    use koda_core::engine::{EngineEvent, TurnEndReason};
    use koda_protocol::item::ToolCallStatus;
    use koda_protocol::turn::TurnStatus;
    use serde_json::json;

    use crate::events::EventProjector;

    fn watch(events: Vec<EngineEvent>) -> Option<LiveTurn> {
        let mut projector = EventProjector::new("s1");
        let mut live = None;
        for event in events {
            for message in projector.project(None, event) {
                LiveTurn::observe(&mut live, &message);
            }
        }
        live
    }

    fn start() -> EngineEvent {
        EngineEvent::TurnStart {
            turn_id: "t1".into(),
        }
    }

    fn text(text: &str) -> EngineEvent {
        EngineEvent::TextDelta { text: text.into() }
    }

    fn call(id: &str) -> EngineEvent {
        EngineEvent::ToolCallStart {
            id: id.into(),
            name: "Bash".into(),
            args: json!({"command": "cargo test"}),
            is_sub_agent: false,
            preview: None,
        }
    }

    /// Mid-turn, the turn has what completed and what is open as it
    /// stands: a message with its text so far, a call in progress.
    #[test]
    fn a_running_turn_has_its_open_items_as_they_stand() {
        let live = watch(vec![
            start(),
            text("Let me "),
            text("check."),
            EngineEvent::TextDone { tools_follow: None },
            call("c1"),
            EngineEvent::ToolCallResult {
                id: "c1".into(),
                name: "Bash".into(),
                output: "ok".into(),
                outcome: ToolOutcome::Success,
                exit_code: Some(0),
                duration_ms: Some(5),
            },
            call("c2"),
            text("Still "),
            text("typing"),
        ])
        .unwrap();
        let turn = live.turn();
        assert_eq!(turn.status, TurnStatus::InProgress);
        let shown: Vec<(String, bool)> = turn
            .items
            .iter()
            .map(|item| {
                let what = match &item.item {
                    Item::AgentMessage { text, .. } => format!("message: {text}"),
                    Item::ToolCall { id, status, .. } => format!("{id}: {status:?}"),
                    other => format!("{other:?}"),
                };
                (what, item.completed_at_ms.is_some())
            })
            .collect();
        assert_eq!(
            shown,
            [
                ("message: Let me check.".to_string(), true),
                (format!("c1: {:?}", ToolCallStatus::Completed), true),
                (format!("c2: {:?}", ToolCallStatus::InProgress), false),
                ("message: Still typing".to_string(), false),
            ]
        );
        assert!(turn.items.iter().all(|item| item.started_at_ms.is_some()));
    }

    /// Once it ends, the turn has its end and nothing open; the next turn
    /// replaces it.
    #[test]
    fn an_ended_turn_keeps_its_items_until_the_next_starts() {
        let live = watch(vec![
            start(),
            text("Done."),
            EngineEvent::TextDone { tools_follow: None },
            EngineEvent::TurnEnd {
                turn_id: "t1".into(),
                reason: TurnEndReason::Complete,
            },
        ])
        .unwrap();
        let turn = live.turn();
        assert_eq!(turn.status, TurnStatus::Completed);
        assert_eq!(turn.items.len(), 1);

        let next = watch(vec![
            start(),
            text("Done."),
            EngineEvent::TextDone { tools_follow: None },
            EngineEvent::TurnEnd {
                turn_id: "t1".into(),
                reason: TurnEndReason::Complete,
            },
            EngineEvent::TurnStart {
                turn_id: "t2".into(),
            },
        ])
        .unwrap();
        assert_eq!(next.turn().id, "t2");
        assert!(next.turn().items.is_empty());
    }

    /// Laid over the stored turns, it replaces what the store has of it.
    #[test]
    fn it_replaces_the_stores_copy_of_the_turn() {
        let live = watch(vec![start(), text("so far")]).unwrap();
        let stored = |id: &str| Turn {
            id: id.into(),
            status: TurnStatus::Interrupted,
            error: None,
            usage: None,
            duration_ms: None,
            started_at: None,
            completed_at: None,
            items: Vec::new(),
        };
        let mut turns = vec![stored("t0"), stored("t1")];
        live.over(&mut turns);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[1].status, TurnStatus::InProgress);
        assert_eq!(turns[1].items.len(), 1);

        let mut earlier = vec![stored("t0")];
        live.over(&mut earlier);
        assert_eq!(earlier.len(), 2);
        assert_eq!(earlier[1].id, "t1");
    }
}
