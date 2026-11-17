//! Output bridge for the fullscreen TUI.
//!
//! All output flows through the `ScrollBuffer` render cache.
//! No more `insert_before()` or direct crossterm writes.
//!
//! Color tokens live in [`crate::theme`] — this module re-exports the
//! ones used by legacy call sites under their previous names so the
//! refactor stays diff-light.
//!
//! See #472 for the fullscreen migration.

use crate::scroll_buffer::ScrollBuffer;
use ratatui::text::{Line, Span};

// Re-export semantic styles so existing imports keep working. New call
// sites should reach for `crate::theme::*` directly.
pub use crate::theme::{
    BOLD, DIM, STATUS_ERROR as RED, STATUS_INFO as CYAN, STATUS_SUCCESS as GREEN,
    STATUS_WARNING as YELLOW, TOOL_PREFIX, WARM_ACCENT, WARM_INFO, WARM_MUTED, WARM_TITLE,
};
// Aliases for legacy ad-hoc names — prefer the semantic ones above.
pub use crate::theme::ACCENT_AMBER as AMBER;

/// Append a single `Line` to the scroll buffer.
pub fn emit_line(buffer: &mut ScrollBuffer, line: Line<'static>) {
    buffer.push(line);
}

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

/// Build a banner for an interrupted turn on session resume: the last
/// stored turn ended interrupted. `unanswered` is its prompt when nothing
/// came of it.
///
/// A banner, not an automatic resume (#594): resuming a destructive call
/// after a dropped connection would surprise the user, the user may have
/// fixed things by hand since, and looking at the history shouldn't cost a
/// model call. "continue" is one word away.
pub fn interrupted_turn_banner(unanswered: Option<&str>) -> Vec<Line<'static>> {
    let mut lines = vec![Line::default()];

    match unanswered {
        Some(preview) => {
            lines.push(Line::from(vec![
                Span::styled("  ↻ ", AMBER),
                Span::styled(
                    "Last turn was interrupted — your prompt was never answered:",
                    AMBER,
                ),
            ]));
            let preview: String = preview.chars().take(80).collect();
            let display = if preview.len() >= 77 {
                let cut = preview.floor_char_boundary(77);
                format!("  \u{201c}{}\u{2026}\u{201d}", &preview[..cut])
            } else {
                format!("  \u{201c}{}\u{201d}", preview)
            };
            lines.push(Line::styled(display, DIM));
        }
        None => {
            lines.push(Line::from(vec![
                Span::styled("  ↻ ", AMBER),
                Span::styled("Last turn was interrupted before it finished.", AMBER),
            ]));
        }
    }

    lines.push(Line::styled(
        "  Type \"continue\" to resume, or start a new message.",
        DIM,
    ));
    lines.push(Line::default());
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_banner_long_non_ascii_prompt_does_not_panic() {
        for padding in ["", "x", "xx"] {
            let prompt = format!("{padding}{}", "界".repeat(40));
            let lines = interrupted_turn_banner(Some(&prompt));
            assert!(!lines.is_empty());
        }
    }
}
