//! Output bridge: ANSI strings → ratatui `insert_before()`.
//!
//! Uses `ansi-to-tui` to convert existing ANSI-formatted output
//! into ratatui `Text` for rendering above the inline viewport.
//! This is a temporary bridge until rendering is migrated to native
//! ratatui `Line`/`Span` (tracked in #78 Step 2).

use ansi_to_tui::IntoText;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    widgets::{Paragraph, Widget},
};

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

/// Bridge for writing ANSI-formatted text above the inline viewport.
pub struct TuiOutput;

impl TuiOutput {
    /// Write an ANSI-formatted string above the viewport.
    ///
    /// Each call inserts content that scrolls into terminal scrollback.
    /// Empty strings are skipped.
    pub fn emit(terminal: &mut Term, text: &str) {
        if text.is_empty() {
            return;
        }
        let line_count = text.lines().count().max(1) as u16;
        let _ = terminal.insert_before(line_count, |buf| {
            match text.into_text() {
                Ok(styled) => {
                    Paragraph::new(styled).render(buf.area, buf);
                }
                Err(_) => {
                    // Fallback: render as plain text
                    Paragraph::new(text).render(buf.area, buf);
                }
            }
        });
    }

    /// Write multiple lines above the viewport.
    pub fn emit_lines(terminal: &mut Term, lines: &[String]) {
        for line in lines {
            Self::emit(terminal, line);
        }
    }
}
