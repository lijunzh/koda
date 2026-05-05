//! Post-turn cleanup: deferred-queue housekeeping, undo commit,
//! residual UI-event drain, auto-compact.
//!
//! Runs once per turn after the inner select loop in
//! [`super::select_loop::TuiContext::run_inference_turn`] breaks. Lives
//! in its own file so the auto-compact policy is easy to find and pin
//! independently of the (much larger) select-loop machinery.

use crate::sink::UiEvent;
use crate::tui_context::TuiContext;
use crate::tui_types::TuiState;

use koda_core::engine::EngineEvent;
use koda_core::persistence::Persistence;
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use tokio::sync::mpsc;

impl TuiContext {
    pub(super) async fn post_turn_cleanup(
        &mut self,
        ui_rx: &mut mpsc::UnboundedReceiver<UiEvent>,
        turn_cancel: &tokio_util::sync::CancellationToken,
    ) {
        // If the turn was cancelled, clear the later_queue so deferred
        // messages don’t immediately fire a new turn that may block on a
        // single-slot local server (LM Studio, ollama) (#825).
        //
        // #1200: check the per-turn child token, not `session.cancel`.
        // The session root is now stable across turn boundaries so bg
        // agents derived from it keep their cancel cascade live; the
        // child is the one that actually fires on Ctrl+C.
        if turn_cancel.is_cancelled() && !self.later_queue.is_empty() {
            let n = self.later_queue.len();
            self.later_queue.clear();
            self.scroll_buffer.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("\u{1f6ab} Cleared {n} deferred message(s)"),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }

        self.tui_state = TuiState::Idle;
        self.inference_start = None;
        // #1200: do NOT replace `session.cancel`. The session root is
        // session-lifetime stable now; per-turn cancellation is on a
        // child token that's dropped when the turn future ends. This
        // fixes the cascade-broken-across-turns bug where bg agents
        // reserved during turn N were orphaned the moment turn N
        // finished and `session.cancel` was swapped out from under them.

        // Commit undo snapshots for this turn
        if let Ok(mut undo) = self.agent.tools.undo.lock() {
            undo.commit_turn();
        }

        // Drain remaining UI events
        while let Ok(UiEvent::Engine(e)) = ui_rx.try_recv() {
            if let EngineEvent::ContextUsage { used, max } = &e {
                self.context_pct = if *max > 0 {
                    (used * 100 / max) as u32
                } else {
                    0
                };
            }
            // Tap activity events into the bg-activity tracker even
            // during post-turn drain — a fan-out agent emitting a
            // ToolStart milliseconds before the turn future resolves
            // would otherwise vanish from the overlay forever (#1210).
            if let EngineEvent::ChildAgentActivity { task_id, kind, .. } = &e {
                self.child_activity.record_activity(*task_id, kind);
            }
            self.renderer.render_to_buffer(e, &mut self.scroll_buffer);
        }

        // Auto-compact
        self.maybe_auto_compact().await;
    }

    async fn maybe_auto_compact(&mut self) {
        let ctx_pct = self.context_pct as usize;
        if ctx_pct < koda_core::inference_helpers::AUTO_COMPACT_THRESHOLD {
            return;
        }

        let pending = self
            .session
            .db
            .has_pending_tool_calls(&self.session.id)
            .await
            .unwrap_or(false);

        if pending {
            if !self.silent_compact_deferred {
                self.scroll_buffer.push(
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(
                            format!(
                                "\u{1f43b} Context at {ctx_pct}% \u{2014} deferring compact (tool calls pending)"
                            ),
                            Style::default().fg(Color::Yellow),
                        ),
                    ]),
                );
                self.silent_compact_deferred = true;
            }
            return;
        }

        self.silent_compact_deferred = false;
        self.scroll_buffer.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("\u{1f43b} Context at {ctx_pct}% \u{2014} auto-compacting..."),
                Style::default().fg(Color::Cyan),
            ),
        ]));

        match koda_core::compact::compact_session(
            &self.session.db,
            &self.session.id,
            self.config.max_context_tokens,
            &self.config.model_settings,
            &self.provider,
        )
        .await
        {
            Ok(Ok(result)) => {
                self.scroll_buffer.push(Line::styled(
                    format!(
                        "  \u{2713} Compacted {} messages \u{2192} ~{} tokens",
                        result.deleted, result.summary_tokens
                    ),
                    Style::default().fg(Color::Green),
                ));
            }
            Ok(Err(_skip)) => {} // silently skip
            Err(e) => {
                self.scroll_buffer.push(Line::styled(
                    format!("  \u{2717} Auto-compact failed: {e:#}"),
                    Style::default().fg(Color::Red),
                ));
            }
        }
    }
}
