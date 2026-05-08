//! Idle-mode UI event handling \u2014 #1349 Bugs 1+2.
//!
//! Mirror image of [`crate::inference::ui_handler::handle_inference_ui_inline`]
//! but for the idle event loop. The two paths exist because:
//!
//! - **During inference**, `select_loop` already drains `ui_rx` on
//!   every iteration of its inner select to feed the renderer +
//!   `ChildActivityTracker`. Bug 1 isn't triggered there.
//! - **During idle**, no one was draining `ui_rx`. Engine events from
//!   bg agents (`ChildAgentActivity`, `ChildTaskUpdate`, `Info`,
//!   `Warn`, `Error`) queued up indefinitely. The activity overlay
//!   froze (Bug 1) and completion mail was never surfaced (Bug 2).
//!
//! This module wires the missing idle drain. Kept separate from
//! `inference::ui_handler` because the policies differ:
//!
//! - Idle never has a streaming text turn, so `TextDelta` /
//!   `ToolCallStart` and friends shouldn't fire here \u2014 if they do,
//!   they're a misrouted event and we just let the renderer route
//!   them to the scroll buffer (defense in depth).
//! - Idle has the `auto_resume_pending` invariant to maintain (start
//!   a turn iff a terminal `ChildTaskUpdate` arrived AND the registry
//!   reports zero non-terminal bg agents).
//! - Idle never deals with approval / ask-user / loop-cap pickers \u2014
//!   those flows are inference-only and would be a bug if they
//!   arrived here.
//!
//! The two handlers staying split avoids piling idle-only branches
//! onto the inference hot path and vice versa. If they ever converge,
//! a single `handle_engine_event(&mut self, event, mode: IdleOrTurn)`
//! would be the natural refactor.

use super::TuiContext;
use crate::sink::UiEvent;
use koda_core::child_agent::{AgentStatus, ChildTaskSnapshot};
use koda_core::engine::EngineEvent;

impl TuiContext {
    /// Absorb one engine event that arrived while the TUI was idle.
    ///
    /// See module docs for rationale. The three live cases:
    ///
    /// 1. **`ChildAgentActivity`** \u2014 update the bg-task overlay
    ///    tracker so the pill keeps showing what the bg agent is
    ///    doing right now.
    /// 2. **`ChildTaskUpdate { status: terminal }`** \u2014 if the
    ///    registry has no other non-terminal entries, set
    ///    `auto_resume_pending` so the loop kicks off a synthetic
    ///    turn that drains the mailbox.
    /// 3. **everything else** \u2014 fall through the renderer so
    ///    `Info` / `Warn` / `Error` etc. land in the scroll buffer.
    ///    Approval / ask-user / loop-cap intentionally NOT handled
    ///    here \u2014 those are inference-loop semantics; arriving while
    ///    idle would be a routing bug, not a thing to gracefully
    ///    absorb.
    pub(super) fn handle_idle_ui_event(&mut self, ui_event: UiEvent) {
        match ui_event {
            UiEvent::Engine(EngineEvent::ChildAgentActivity { task_id, kind, .. }) => {
                self.child_activity.record_activity(task_id, &kind);
                // Frame redraw happens at top of next loop iteration.
            }
            UiEvent::Engine(EngineEvent::ChildTaskUpdate { status, .. }) => {
                // Only terminal statuses trigger the auto-resume gate.
                // `Pending` / `Running` heartbeats are pure UX signal
                // (rendered by the activity overlay's status palette).
                let snapshot = self.session.bg_agents.snapshot();
                if should_auto_resume(&status, &snapshot) {
                    self.auto_resume_pending = true;
                    tracing::debug!(
                        "auto_resume_pending=true (bg agent reached terminal status, no non-terminal entries remain)"
                    );
                }
            }
            UiEvent::Engine(event) => {
                // Defense-in-depth: route every other engine event
                // through the renderer so bg-agent narrative lines
                // (`Info` / `Warn` / `Error`) don't silently disappear.
                // The renderer knows to no-op on streaming-only
                // events that shouldn't fire while idle.
                self.renderer
                    .render_to_buffer(event, &mut self.scroll_buffer);
            }
        }
    }
}

/// `true` for `Cancelled`, `Completed { .. }`, `Errored { .. }`. False
/// for `Pending`, `Running { .. }`, and any forward-compat unknowns
/// (treated as non-terminal so we never auto-resume on a status
/// `koda-cli` doesn't recognise).
///
/// Pure free function so the auto-resume gating logic stays
/// unit-testable without spinning a real `ChildAgentRegistry`.
pub(crate) fn is_terminal(status: &AgentStatus) -> bool {
    match status {
        AgentStatus::Cancelled | AgentStatus::Completed { .. } | AgentStatus::Errored { .. } => {
            true
        }
        AgentStatus::Pending | AgentStatus::Running { .. } => false,
        // Forward-compat: if a future variant ships, treat it as
        // non-terminal so we don't auto-resume on something we can't
        // classify. Worse to wake the LLM with no mail than to wait
        // for a follow-up signal.
        _ => false,
    }
}

/// Pure decision function for the auto-resume gate (#1349 Bug 2).
///
/// Returns `true` when:
///
/// 1. The just-arrived status is **terminal** — i.e. a bg agent
///    actually finished (or errored / was cancelled), so it's the
///    moment when mail might be in the mailbox waiting for the
///    parent.
/// 2. The post-event registry snapshot has **no non-terminal
///    entries** — we don't auto-resume mid-fan-out, only when
///    the last bg agent has wrapped up. Mirrors codex's
///    `maybe_start_turn_for_pending_work` "only when idle" guard.
///
/// Extracted from [`TuiContext::handle_idle_ui_event`] so the
/// decision rules can be exercised by unit tests without spinning a
/// real `KodaSession` / `ChildAgentRegistry`. The handler stays a
/// thin glue layer around this fn + the side effect.
pub(crate) fn should_auto_resume(
    new_status: &AgentStatus,
    registry_snapshot: &[ChildTaskSnapshot],
) -> bool {
    is_terminal(new_status) && registry_snapshot.iter().all(|s| is_terminal(&s.status))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_and_running_are_not_terminal() {
        assert!(!is_terminal(&AgentStatus::Pending));
        assert!(!is_terminal(&AgentStatus::Running { iter: 0 }));
        assert!(!is_terminal(&AgentStatus::Running { iter: 42 }));
    }

    #[test]
    fn completed_errored_cancelled_are_terminal() {
        assert!(is_terminal(&AgentStatus::Cancelled));
        assert!(is_terminal(&AgentStatus::Completed {
            summary: "ok".into()
        }));
        assert!(is_terminal(&AgentStatus::Errored {
            error: "boom".into()
        }));
    }

    fn snap(task_id: u32, status: AgentStatus) -> ChildTaskSnapshot {
        ChildTaskSnapshot::for_testing(
            task_id,
            "t".into(),
            "p".into(),
            std::time::Duration::from_secs(1),
            status,
            None,
        )
    }

    #[test]
    fn no_resume_when_status_is_non_terminal() {
        // Heartbeat — the bg agent is still running, so even if the
        // snapshot were empty we should NOT auto-resume.
        assert!(!should_auto_resume(&AgentStatus::Running { iter: 3 }, &[]));
        assert!(!should_auto_resume(&AgentStatus::Pending, &[]));
    }

    #[test]
    fn no_resume_when_other_bg_agents_still_running() {
        // One bg agent finished, but a sibling is still in flight.
        // Mid-fan-out auto-resume would race the still-running one
        // and waste tokens — wait for the last one to wrap up.
        let snap = vec![
            snap(
                1,
                AgentStatus::Completed {
                    summary: "a".into(),
                },
            ),
            snap(2, AgentStatus::Running { iter: 5 }),
        ];
        assert!(!should_auto_resume(
            &AgentStatus::Completed {
                summary: "a".into()
            },
            &snap
        ));
    }

    #[test]
    fn resume_when_terminal_and_all_others_terminal() {
        // The headline case: last bg agent just finished.
        let snap = vec![
            snap(
                1,
                AgentStatus::Completed {
                    summary: "a".into(),
                },
            ),
            snap(
                2,
                AgentStatus::Completed {
                    summary: "b".into(),
                },
            ),
        ];
        assert!(should_auto_resume(
            &AgentStatus::Completed {
                summary: "b".into()
            },
            &snap
        ));
    }

    #[test]
    fn resume_when_terminal_and_snapshot_empty() {
        // The registry already pruned the just-finished entry by the
        // time we read the snapshot (race tolerance).
        assert!(should_auto_resume(
            &AgentStatus::Errored {
                error: "boom".into()
            },
            &[]
        ));
        assert!(should_auto_resume(&AgentStatus::Cancelled, &[]));
    }
}
