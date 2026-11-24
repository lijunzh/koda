//! Post-turn cleanup: unsent steering text, dialogs, the learned window.
//!
//! Runs once per turn after the inner select loop in
//! [`super::select_loop::TuiContext::run_inference_turn`] breaks. Compacting
//! after a turn is the host's (`model_post_turn_compact_threshold_percent`), as in
//! Codex, and runs inside the turn.

use crate::tui_context::TuiContext;
use crate::tui_types::TuiState;

use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

impl TuiContext {
    /// After `turn/completed`, with the turn's events all shown: back to
    /// idle, any dialog closed, unsent steering text back in the input box.
    /// A turn that didn't complete (stopped, failed, or Koda quitting) puts
    /// the queued messages back there too, as Codex does; a completed one
    /// leaves them for the event loop to send.
    pub(super) fn post_turn_cleanup(&mut self, unsent: Vec<String>, completed: bool) {
        self.tui_state = TuiState::Idle;
        self.inference_start = None;

        // A dialog that reached the screen as the turn stopped has no one
        // waiting for its answer, except a background agent's request: the
        // agent outlives the turn and still waits.
        if !matches!(
            self.menu,
            crate::tui_types::MenuContent::Approval {
                background: true,
                ..
            }
        ) {
            super::crossterm_handler::close_dialog(
                &mut self.menu,
                &mut self.prompt_mode,
                &mut self.pending_approval_id,
                &mut self.textarea,
            );
        }

        // Steering text the turn didn't deliver goes back in the composer, so
        // it isn't lost and doesn't go out behind the next prompt. Nothing
        // runs until the user presses Enter: they may have stopped the turn
        // on purpose, and a new turn can block on a single-slot local server
        // that is still busy with the cancelled one (#825).
        let returned = if completed {
            Vec::new()
        } else {
            self.queued.drain()
        };
        if !unsent.is_empty() || !returned.is_empty() {
            let mut unsent = unsent;
            for message in returned {
                unsent.push(message.text);
                self.pastes.list.extend(message.pastes);
                self.pasted_images.extend(message.images);
            }
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
        // the meter and the next turns use it. Not saved:
        // the server may be restarted with another size.
        if let Some(tokens) = self.learned_context_window.take() {
            self.settings.context_window = tokens as u64;
        }
    }
}

impl TuiContext {
    /// `Alt+Up` during a turn: the message queued last back into the empty
    /// input box, to change it or send it now with `Enter`, as Codex's
    /// Alt+Up edits the last queued message.
    pub(super) fn unqueue_last(&mut self) {
        let Some(message) = self.queued.take_last() else {
            crate::tui_output::dim_msg(&mut self.scroll_buffer, "Nothing is queued.".into());
            return;
        };
        let mut pastes = std::mem::take(&mut self.pastes.list);
        pastes.extend(message.pastes);
        self.pastes
            .restore(&mut self.textarea, &message.text, pastes);
        self.pasted_images.extend(message.images);
        self.scroll_buffer.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "\u{21a9} Taken off the queue. It's back in the input box.",
                Style::default().fg(Color::Yellow),
            ),
        ]));
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
