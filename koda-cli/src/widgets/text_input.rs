//! Inline text input widget for the TUI.
//!
//! Renders a prompt above the viewport, reads character-by-character
//! input in raw mode using crossterm.

use crate::tui_output;
use crossterm::event::{self, Event, KeyCode};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    style::{Color, Style},
    text::{Line, Span},
};
use std::io::Write;

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

/// Read a line of text inline above the viewport.
///
/// Shows `prompt` via `insert_before()`, then reads input character by
/// character. Returns the trimmed input, or empty string on Esc.
///
/// If `mask` is true, input is shown as `*` characters (for API keys).
pub fn read_line(_terminal: &mut Term, prompt: &str, mask: bool) -> String {
    // Use write_line (crossterm direct) not emit_line (ratatui insert_before)
    // because this is called during slash commands where the ratatui viewport
    // is desynced after select_inline.
    tui_output::write_line(&Line::from(vec![
        Span::raw("  "),
        Span::styled(prompt.to_string(), Style::default().fg(Color::Cyan)),
    ]));

    let mut buf = String::new();
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, crossterm::style::Print("\r  ")).ok();
    stdout.flush().ok();

    loop {
        if let Ok(Event::Key(key)) = event::read() {
            match key.code {
                KeyCode::Enter => break,
                KeyCode::Esc => {
                    buf.clear();
                    break;
                }
                KeyCode::Backspace => {
                    if buf.pop().is_some() {
                        crossterm::execute!(
                            stdout,
                            crossterm::cursor::MoveLeft(1),
                            crossterm::style::Print(" "),
                            crossterm::cursor::MoveLeft(1),
                        )
                        .ok();
                    }
                }
                KeyCode::Char(c) => {
                    buf.push(c);
                    let display = if mask { "*".to_string() } else { c.to_string() };
                    crossterm::execute!(stdout, crossterm::style::Print(display)).ok();
                }
                _ => {}
            }
        }
    }
    crossterm::execute!(stdout, crossterm::style::Print("\r\n")).ok();
    stdout.flush().ok();
    buf.trim().to_string()
}
