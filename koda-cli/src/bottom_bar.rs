//! Fixed bottom bar using ANSI scroll regions.
//!
//! Sets up a scroll region for the main output (top N-2 rows) and
//! reserves the bottom 2 rows for a status bar and input prompt.
//! All existing `println!` output is confined to the scroll region
//! automatically — no changes needed to display code.
//!
//! Based on the same technique used by vim, less, and tmux.

use crossterm::{
    cursor, execute,
    terminal::{self, ClearType},
};
use std::io::{Write, stdout};

/// Height of the fixed bottom area (status bar + input line).
const BOTTOM_HEIGHT: u16 = 2;

/// Manages the fixed bottom bar with ANSI scroll regions.
pub struct BottomBar {
    enabled: bool,
    rows: u16,
    cols: u16,
    status_text: String,
}

impl BottomBar {
    /// Create and activate the bottom bar.
    /// Returns None if stdout is not a TTY (e.g., piped output).
    pub fn new() -> Option<Self> {
        if !std::io::IsTerminal::is_terminal(&stdout()) {
            return None;
        }

        let (cols, rows) = terminal::size().ok()?;
        if rows < 10 {
            return None; // terminal too small
        }

        let mut bar = Self {
            enabled: true,
            rows,
            cols,
            status_text: String::new(),
        };
        bar.setup_scroll_region();
        Some(bar)
    }

    /// Set up the ANSI scroll region, reserving bottom rows.
    fn setup_scroll_region(&mut self) {
        let scroll_end = self.rows - BOTTOM_HEIGHT;
        let mut out = stdout();
        // Clear screen and move to top
        let _ = execute!(out, terminal::Clear(ClearType::All));
        let _ = execute!(out, cursor::MoveTo(0, 0));
        // Set scroll region to top portion
        let _ = write!(out, "\x1b[1;{scroll_end}r");
        // Draw the bottom bar
        self.redraw_bar();
        // Move cursor to top of scroll region
        let _ = execute!(out, cursor::MoveTo(0, 0));
        let _ = out.flush();
    }

    /// Restore full-screen scroll region (cleanup on exit).
    pub fn restore(&self) {
        if !self.enabled {
            return;
        }
        let mut out = stdout();
        // Reset scroll region to full terminal
        let _ = write!(out, "\x1b[1;{}r", self.rows);
        let _ = execute!(out, cursor::MoveTo(0, self.rows - 1));
        let _ = out.flush();
    }

    /// Update the status bar text and redraw.
    pub fn set_status(&mut self, text: &str) {
        self.status_text = text.to_string();
        self.redraw_bar();
    }

    /// Handle terminal resize.
    #[allow(dead_code)]
    pub fn on_resize(&mut self) {
        if let Ok((cols, rows)) = terminal::size() {
            self.cols = cols;
            self.rows = rows;
            self.setup_scroll_region();
        }
    }

    /// Redraw the bottom bar (status line + input prompt placeholder).
    fn redraw_bar(&self) {
        let mut out = stdout();
        let status_row = self.rows - BOTTOM_HEIGHT;
        let _input_row = self.rows - 1;

        // Save cursor position
        let _ = execute!(out, cursor::SavePosition);

        // Draw status bar
        let _ = execute!(out, cursor::MoveTo(0, status_row));
        let _ = execute!(out, terminal::Clear(ClearType::CurrentLine));
        // Dim background for status bar
        let status = if self.status_text.is_empty() {
            String::new()
        } else {
            self.status_text.clone()
        };
        let padded = format!("{:<width$}", status, width = self.cols as usize);
        let _ = write!(out, "\x1b[90;7m{padded}\x1b[0m");

        // Restore cursor position (back to scroll region)
        let _ = execute!(out, cursor::RestorePosition);
        let _ = out.flush();
    }
}

impl Drop for BottomBar {
    fn drop(&mut self) {
        self.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bottom_height_is_reasonable() {
        assert!(BOTTOM_HEIGHT >= 1 && BOTTOM_HEIGHT <= 5);
    }

    #[test]
    fn test_non_tty_returns_none() {
        // In CI/test environments, stdout is usually not a TTY
        // This test just verifies it doesn't panic
        let _bar = BottomBar::new();
    }
}
