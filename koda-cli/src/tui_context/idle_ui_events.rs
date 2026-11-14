//! Engine events that arrive while the TUI is idle.
//!
//! During a turn, `inference::select_loop` reads them. Between turns,
//! sub-agent status can still arrive (#1349): a sub-agent's last status
//! lands after a stopped turn has returned. The overlay takes activity, and
//! anything else a person should see goes through the renderer.
//!
//! Nothing here starts a turn. The idle auto-resume of #1349 started one
//! when a background agent finished; background agents are gone since #1366
//! (every sub-agent runs inside its turn), so it could only start a turn
//! nobody asked for. The PTY tests `esc_during_a_sub_agent_starts_no_other_turn`
//! and `a_finished_sub_agent_starts_no_other_turn` hold that.

use super::TuiContext;
use crate::sink::UiEvent;
use koda_core::engine::EngineEvent;

impl TuiContext {
    /// Absorb one engine event that arrived while the TUI was idle.
    pub(super) fn handle_idle_ui_event(&mut self, ui_event: UiEvent) {
        match ui_event {
            UiEvent::Engine(EngineEvent::ChildAgentActivity { task_id, kind, .. }) => {
                self.child_activity.record_activity(task_id, &kind);
            }
            // The overlay reads status from the registry each frame.
            UiEvent::Engine(EngineEvent::ChildTaskUpdate { .. }) => {}
            UiEvent::Engine(event) => {
                self.renderer
                    .render_to_buffer(event, &mut self.scroll_buffer);
            }
        }
    }
}
