//! Fixed bottom bar using ANSI scroll regions.
//!
//! Sets up a scroll region for the main output (top N-2 rows) and
//! reserves the bottom 2 rows for an input line and a status bar.
//! All existing `println!` output is confined to the scroll region
//! automatically — no changes needed to display code.
//!
//! During inference, captures raw keystrokes and renders them in the
//! input line so users can see what they're typing.
//!
//! ## Keyboard shortcuts (during inference)
//!
//! | Key | Action |
//! |-----|--------|
//! | **Enter** | Queue typed text as the next prompt |
//! | **Ctrl+C** | Cancel the current inference turn |
//! | **Ctrl+C ×2** | Force quit Koda |
//! | **Ctrl+U** | Clear the input line |
//! | **Ctrl+W** | Delete the last word |
//! | **Backspace** | Delete the last character |

use crossterm::{
    cursor,
    event::{EventStream, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{self, ClearType},
};
use std::io::{Write, stdout};

/// Height of the fixed bottom area (input line only during inference).
const BOTTOM_HEIGHT: u16 = 1;

/// Action returned by keystroke handling.
pub enum KeyAction {
    /// No action (regular typing, backspace, etc.)
    None,
    /// User pressed Enter — submit this text.
    Submit(String),
    /// User pressed Ctrl+C — interrupt current turn.
    Interrupt,
}

/// Manages the fixed bottom bar with ANSI scroll regions.
pub struct BottomBar {
    enabled: bool,
    rows: u16,
    cols: u16,
    /// Input buffer for type-ahead during inference.
    input_buf: String,
    /// Queued message (after Enter, waiting for current turn to finish).
    queued_msg: Option<String>,
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
            input_buf: String::new(),
            queued_msg: None,
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

    /// Enable raw mode for keystroke capture during inference.
    /// Re-enables output post-processing (OPOST) so println! still works.
    pub fn start_input_capture(&mut self) {
        if !self.raw_mode {
            let _ = terminal::enable_raw_mode();
            // Raw mode disables OPOST (output processing), which breaks println!
            // Re-enable it so \n → \r\n translation works in the scroll area.
            #[cfg(unix)]
            {
                use std::os::fd::AsFd;
                let out = std::io::stdout();
                let stdout_fd = out.as_fd();
                if let Ok(mut termios) = nix::sys::termios::tcgetattr(stdout_fd) {
                    termios.output_flags |= nix::sys::termios::OutputFlags::OPOST;
                    let _ = nix::sys::termios::tcsetattr(
                        stdout_fd,
                        nix::sys::termios::SetArg::TCSANOW,
                        &termios,
                    );
                }
            }
            self.raw_mode = true;
            self.input_buf.clear();
            self.queued_msg = None;
            // Don't redraw yet — readline's prompt is still visible.
            // The bar appears when the user starts typing or on first status update.
        }
    }

    /// Disable raw mode and return any buffered/queued input.
    pub fn stop_input_capture(&mut self) -> Option<String> {
        if self.raw_mode {
            let _ = terminal::disable_raw_mode();
            self.raw_mode = false;
        }
        // Prefer queued (Enter was pressed) over partial buffer
        let result = self.queued_msg.take().or_else(|| {
            if self.input_buf.is_empty() {
                None
            } else {
                Some(std::mem::take(&mut self.input_buf))
            }
        });
        self.input_buf.clear();
        result
    }

    /// Create an async event stream for reading keystrokes.
    /// Call this once and poll it in the tokio::select! loop.
    pub fn event_stream(&self) -> EventStream {
        EventStream::new()
    }

    /// Handle a crossterm key event.
    /// Returns `KeyAction` indicating what happened.
    pub fn handle_key(&mut self, event: KeyEvent) -> KeyAction {
        // Only handle key press events (not release/repeat)
        if event.kind != crossterm::event::KeyEventKind::Press {
            return KeyAction::None;
        }

        // Ignore keys if already queued
        if self.queued_msg.is_some() {
            return KeyAction::None;
        }

        match event.code {
            KeyCode::Enter => {
                let line = std::mem::take(&mut self.input_buf);
                if line.trim().is_empty() {
                    return KeyAction::None;
                }
                // Show "Queued" state in the bar
                self.queued_msg = Some(line.clone());
                self.redraw_bar();
                KeyAction::Submit(line)
            }
            KeyCode::Char('c') if event.modifiers.contains(KeyModifiers::CONTROL) => {
                // Ctrl+C: interrupt (raw mode swallows SIGINT)
                self.input_buf.clear();
                self.redraw_bar();
                KeyAction::Interrupt
            }
            KeyCode::Char('u') if event.modifiers.contains(KeyModifiers::CONTROL) => {
                // Ctrl+U: clear line
                self.input_buf.clear();
                self.redraw_bar();
                KeyAction::None
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
                KeyAction::None
            }
            KeyCode::Backspace => {
                self.input_buf.pop();
                self.redraw_bar();
                KeyAction::None
            }
            KeyCode::Char(c) => {
                self.input_buf.push(c);
                self.redraw_bar();
                KeyAction::None
            }
            _ => KeyAction::None,
        }
    }

    /// Handle terminal resize.
    /// Currently a no-op during inference — reliable resize with ANSI scroll
    /// regions during rapid events is unsolved. See issue #45.
    /// Use `refresh_if_resized()` between turns instead.
    #[allow(dead_code)]
    pub fn on_resize(&mut self) {
        // Intentionally empty during inference.
    }

    /// Check if terminal was resized and re-setup scroll region if needed.
    /// Call this between turns (when readline is idle, no raw mode).
    pub fn refresh_if_resized(&mut self) {
        if let Ok((cols, rows)) = terminal::size() {
            if rows < 10 {
                return;
            }
            if cols != self.cols || rows != self.rows {
                self.cols = cols;
                self.rows = rows;
                // Update scroll region to new size (no bar to draw between turns)
                let scroll_end = self.rows - BOTTOM_HEIGHT;
                let mut out = stdout();
                let _ = write!(out, "\x1b[1;{scroll_end}r");
                let _ = out.flush();
            }
        }
    }

    /// Redraw the bottom input line.
    fn redraw_bar(&self) {
        let mut out = stdout();
        let input_row = self.rows - BOTTOM_HEIGHT;

        let _ = execute!(out, cursor::SavePosition);
        let _ = execute!(out, cursor::MoveTo(0, input_row));
        let _ = execute!(out, terminal::Clear(ClearType::CurrentLine));

        if self.raw_mode {
            if let Some(ref queued) = self.queued_msg {
                let _ = write!(
                    out,
                    "\x1b[33m\u{23f3} Queued:\x1b[0m \x1b[90m{queued}\x1b[0m\x1b[K"
                );
            } else {
                let _ = write!(out, "\x1b[36m\u{276f}\x1b[0m {}\x1b[K", self.input_buf);
            }
        }

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
        const { assert!(BOTTOM_HEIGHT >= 1 && BOTTOM_HEIGHT <= 5) };
    }

    #[test]
    fn test_non_tty_returns_none() {
        // In CI/test environments, stdout is usually not a TTY
        // This test just verifies it doesn't panic
        let _bar = BottomBar::new();
    }
}
