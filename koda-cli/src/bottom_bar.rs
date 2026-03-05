//! Fixed bottom bar using ANSI scroll regions.
//!
//! Sets up a scroll region for the main output (top N-2 rows) and
//! reserves the bottom 2 rows for a status bar and input prompt.
//! All existing `println!` output is confined to the scroll region
//! automatically — no changes needed to display code.
//!
//! During inference, captures raw keystrokes and renders them in the
//! input line so users can see what they're typing.

use crossterm::{
    cursor,
    event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{self, ClearType},
};
use futures_util::StreamExt;
use std::io::{Write, stdout};

/// Height of the fixed bottom area (status bar + input line).
const BOTTOM_HEIGHT: u16 = 2;

/// Manages the fixed bottom bar with ANSI scroll regions.
pub struct BottomBar {
    enabled: bool,
    rows: u16,
    cols: u16,
    status_text: String,
    /// Input buffer for type-ahead during inference.
    input_buf: String,
    /// Whether we're in raw mode (capturing keystrokes).
    raw_mode: bool,
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
            input_buf: String::new(),
            raw_mode: false,
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
        if self.raw_mode {
            let _ = terminal::disable_raw_mode();
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

    /// Enable raw mode for keystroke capture during inference.
    pub fn start_input_capture(&mut self) {
        if !self.raw_mode {
            let _ = terminal::enable_raw_mode();
            self.raw_mode = true;
            self.input_buf.clear();
            self.redraw_bar();
        }
    }

    /// Disable raw mode and return any buffered input.
    pub fn stop_input_capture(&mut self) -> Option<String> {
        if self.raw_mode {
            let _ = terminal::disable_raw_mode();
            self.raw_mode = false;
        }
        if self.input_buf.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.input_buf))
        }
    }

    /// Create an async event stream for reading keystrokes.
    /// Call this once and poll it in the tokio::select! loop.
    pub fn event_stream(&self) -> EventStream {
        EventStream::new()
    }

    /// Handle a crossterm key event. Returns Some(line) on Enter.
    pub fn handle_key(&mut self, event: KeyEvent) -> Option<String> {
        match event.code {
            KeyCode::Enter => {
                let line = std::mem::take(&mut self.input_buf);
                self.redraw_bar();
                if line.trim().is_empty() {
                    None
                } else {
                    Some(line)
                }
            }
            KeyCode::Backspace => {
                self.input_buf.pop();
                self.redraw_bar();
                None
            }
            KeyCode::Char('c') if event.modifiers.contains(KeyModifiers::CONTROL) => {
                // Ctrl+C: clear the input buffer
                self.input_buf.clear();
                self.redraw_bar();
                None
            }
            KeyCode::Char('u') if event.modifiers.contains(KeyModifiers::CONTROL) => {
                // Ctrl+U: clear line
                self.input_buf.clear();
                self.redraw_bar();
                None
            }
            KeyCode::Char('w') if event.modifiers.contains(KeyModifiers::CONTROL) => {
                // Ctrl+W: delete last word
                let trimmed = self.input_buf.trim_end().to_string();
                if let Some(pos) = trimmed.rfind(' ') {
                    self.input_buf = trimmed[..pos + 1].to_string();
                } else {
                    self.input_buf.clear();
                }
                self.redraw_bar();
                None
            }
            KeyCode::Char(c) => {
                self.input_buf.push(c);
                self.redraw_bar();
                None
            }
            _ => None,
        }
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

    /// Redraw the bottom bar (status line + input prompt).
    fn redraw_bar(&self) {
        let mut out = stdout();
        let status_row = self.rows - BOTTOM_HEIGHT;
        let input_row = self.rows - 1;
        let width = self.cols as usize;

        // Save cursor position
        let _ = execute!(out, cursor::SavePosition);

        // Draw status bar (inverted dim)
        let _ = execute!(out, cursor::MoveTo(0, status_row));
        let _ = execute!(out, terminal::Clear(ClearType::CurrentLine));
        let status = if self.status_text.is_empty() {
            String::new()
        } else {
            self.status_text.clone()
        };
        let padded = format!("{:<width$}", status, width = width);
        let _ = write!(out, "\x1b[90;7m{padded}\x1b[0m");

        // Draw input line
        let _ = execute!(out, cursor::MoveTo(0, input_row));
        let _ = execute!(out, terminal::Clear(ClearType::CurrentLine));
        if self.raw_mode {
            // Show the input buffer with a prompt
            let prompt = "\x1b[36m\u{276f}\x1b[0m ";
            let display = if self.input_buf.len() > width.saturating_sub(4) {
                // Truncate from the left if too long
                let start = self.input_buf.len() - (width - 4);
                &self.input_buf[start..]
            } else {
                &self.input_buf
            };
            let _ = write!(out, "{prompt}{display}\x1b[K");
        }

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
