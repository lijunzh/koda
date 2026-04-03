//! Output bridge for the fullscreen TUI.
//!
//! All output flows through the `ScrollBuffer` render cache.
//! No more `insert_before()` or direct crossterm writes.
//!
//! See #472 for the fullscreen migration.

use crate::scroll_buffer::ScrollBuffer;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

/// Append a single `Line` to the scroll buffer.
pub fn emit_line(buffer: &mut ScrollBuffer, line: Line<'static>) {
    buffer.push(line);
}

// ── Style constants ─────────────────────────────────────────
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

// Warm palette — earthy tones for koda's bear identity.
pub const WARM_TITLE: Style = Style::new()
    .fg(Color::Rgb(229, 192, 123)) // soft gold #e5c07b
    .add_modifier(Modifier::BOLD);
pub const WARM_ACCENT: Style = Style::new().fg(Color::Rgb(209, 154, 102)); // amber #d19a66
pub const WARM_MUTED: Style = Style::new().fg(Color::Rgb(124, 111, 100)); // brown #7c6f64
pub const WARM_INFO: Style = Style::new().fg(Color::Rgb(198, 165, 106)); // soft gold #c6a56a

// ── Message helpers ─────────────────────────────────────────
// Push styled status messages into the scroll buffer.

/// Push a success message: " ✓ {msg}"
pub fn ok_msg(buffer: &mut ScrollBuffer, msg: String) {
    buffer.push(Line::from(vec![
        Span::styled("  \u{2713} ", GREEN),
        Span::raw(msg),
    ]));
}

/// Push an error message: " ✗ {msg}"
pub fn err_msg(buffer: &mut ScrollBuffer, msg: String) {
    buffer.push(Line::from(vec![
        Span::styled("  \u{2717} ", RED),
        Span::styled(msg, RED),
    ]));
}

/// Push a dim message: "  {msg}"
pub fn dim_msg(buffer: &mut ScrollBuffer, msg: String) {
    buffer.push(Line::styled(format!("  {msg}"), DIM));
}

/// Push a warning message: " ⚠ {msg}"
pub fn warn_msg(buffer: &mut ScrollBuffer, msg: String) {
    buffer.push(Line::from(vec![
        Span::styled("  \u{26a0} ", YELLOW),
        Span::styled(msg, YELLOW),
    ]));
}

/// Push a blank line.
pub fn blank(buffer: &mut ScrollBuffer) {
    buffer.push(Line::default());
}
