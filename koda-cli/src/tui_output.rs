//! Output bridge: native ratatui types → `insert_before()`.
//!
//! All rendering produces `ratatui::text::Line` / `Text` directly.
//! This module provides helpers for writing styled content above
//! the persistent inline viewport.

use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

/// Write `Line`s above the inline viewport via `insert_before()`.
pub fn emit_lines(terminal: &mut Term, lines: &[Line<'_>]) {
    if lines.is_empty() {
        return;
    }
    let height = lines.len() as u16;
    let owned: Vec<Line<'static>> = lines
        .iter()
        .map(|l| {
            Line::from(
                l.spans
                    .iter()
                    .map(|s| Span::styled(s.content.to_string(), s.style))
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    let _ = terminal.insert_before(height, |buf| {
        Paragraph::new(owned).render(buf.area, buf);
    });
}

/// Write a single `Line` above the viewport.
pub fn emit_line(terminal: &mut Term, line: Line<'_>) {
    emit_lines(terminal, &[line]);
}

/// Write a blank line above the viewport.
pub fn emit_blank(terminal: &mut Term) {
    emit_line(terminal, Line::raw(""));
}

// ── Style constants ─────────────────────────────────────────────
// Centralized color palette for the TUI renderer.

pub const DIM: Style = Style::new().fg(Color::DarkGray);
pub const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);
pub const CYAN: Style = Style::new().fg(Color::Cyan);
pub const YELLOW: Style = Style::new().fg(Color::Yellow);
pub const RED: Style = Style::new().fg(Color::Red);
pub const GREEN: Style = Style::new().fg(Color::Green);
pub const MAGENTA: Style = Style::new().fg(Color::Magenta);
pub const ORANGE: Style = Style::new().fg(Color::Rgb(255, 165, 0));
pub const AMBER: Style = Style::new().fg(Color::Rgb(255, 191, 0));
