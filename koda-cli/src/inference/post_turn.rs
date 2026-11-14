//! Post-turn cleanup: unsent steering text, dialogs, auto-compact.
//!
//! Runs once per turn after the inner select loop in
//! [`super::select_loop::TuiContext::run_inference_turn`] breaks. Lives
//! in its own file so the auto-compact policy is easy to find and pin
//! independently of the (much larger) select-loop machinery.

use crate::tui_context::TuiContext;
use crate::tui_types::TuiState;

use koda_core::persistence::Persistence;
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

impl TuiContext {
    /// After `turn/completed`, with the turn's events all shown: back to
    /// idle, any dialog closed, unsent steering text back in the input box,
    /// and auto-compaction if the context is full.
    pub(super) async fn post_turn_cleanup(&mut self, unsent: Vec<String>) {
        self.tui_state = TuiState::Idle;
        self.inference_start = None;

        // A dialog that reached the screen as the turn stopped has no one
        // waiting for its answer.
        super::crossterm_handler::close_dialog(
            &mut self.menu,
            &mut self.prompt_mode,
            &mut self.pending_approval_id,
            &mut self.textarea,
        );

        // Steering text the turn didn't deliver goes back in the composer, so
        // it isn't lost and doesn't go out behind the next prompt. Nothing
        // runs until the user presses Enter: they may have stopped the turn
        // on purpose, and a new turn can block on a single-slot local server
        // that is still busy with the cancelled one (#825).
        if !unsent.is_empty() {
            let has_draft = !self.textarea.text().is_empty();
            let draft = unsent_steers_as_draft(&unsent, has_draft);
            self.textarea.insert_str_at(0, &draft);
            self.scroll_buffer.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    "\u{21a9} Not sent to the model. It's back in the input box.",
                    Style::default().fg(Color::Yellow),
                ),
            ]));
        }

        // Keep the server's real window for the rest of the session, so
        // the meter, auto-compact and the next turns use it. Not saved:
        // the server may be restarted with another size.
        if let Some(tokens) = self.learned_context_window.take() {
            self.config.max_context_tokens = tokens;
            self.config.model_settings.max_context_tokens = tokens;
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

/// What to put in front of the composer when queued text comes back unsent.
///
/// `steers` is oldest first, as `TurnTracker::take_unsent_steers` gives them. A draft
/// already in the composer stays after it, one blank line down.
fn unsent_steers_as_draft(steers: &[String], has_draft: bool) -> String {
    let joined = steers.join("\n\n");
    if has_draft {
        format!("{joined}\n\n")
    } else {
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::unsent_steers_as_draft;

    #[test]
    fn unsent_steers_keep_their_order() {
        let steers = ["one".to_string(), "two".to_string()];
        assert_eq!(unsent_steers_as_draft(&steers, false), "one\n\ntwo");
    }

    #[test]
    fn unsent_steers_leave_room_for_the_draft() {
        let steers = ["one".to_string()];
        assert_eq!(unsent_steers_as_draft(&steers, true), "one\n\n");
    }
}
