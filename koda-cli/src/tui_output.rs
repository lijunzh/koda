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

/// Build a banner for an interrupted turn on session resume.
///
/// Returns styled `Line`s ready to push into a `ScrollBuffer`.
/// The banner tells the user what was interrupted and how to continue.
pub fn interrupted_turn_banner(
    kind: &koda_core::persistence::InterruptionKind,
) -> Vec<Line<'static>> {
    use koda_core::persistence::InterruptionKind;

    let mut lines = vec![Line::default()];

    match kind {
        InterruptionKind::Prompt(preview) => {
            lines.push(Line::from(vec![
                Span::styled("  ↻ ", AMBER),
                Span::styled(
                    "Last turn was interrupted — your prompt was never answered:",
                    AMBER,
                ),
            ]));
            let display = if preview.len() >= 77 {
                format!("  \u{201c}{}\u{2026}\u{201d}", &preview[..77])
            } else {
                format!("  \u{201c}{}\u{201d}", preview)
            };
            lines.push(Line::styled(display, DIM));
        }
        InterruptionKind::Tool => {
            lines.push(Line::from(vec![
                Span::styled("  ↻ ", AMBER),
                Span::styled(
                    "Last turn was interrupted — tool result was never processed.",
                    AMBER,
                ),
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

/// Format seconds of idle time into a human-readable "away" string.
/// Returns `None` for trivially short gaps (< 5 min) to avoid noise.
fn format_idle(secs: i64) -> Option<String> {
    match secs {
        ..=299 => None, // < 5 minutes — not worth showing
        300..=3599 => Some(format!("{} min", secs / 60)),
        3600..=86399 => {
            let h = secs / 3600;
            Some(format!("{} h", h))
        }
        86400..=604799 => {
            let d = secs / 86400;
            Some(format!("{} day{}", d, if d == 1 { "" } else { "s" }))
        }
        _ => {
            let w = secs / 604800;
            Some(format!("{} week{}", w, if w == 1 { "" } else { "s" }))
        }
    }
}

/// One-line summary banner shown at the top of a resumed session.
///
/// Shows session title, message + tool-call counts, token usage, and
/// how long the session was idle. Returns an empty `Vec` if there are
/// no meaningful stats to display (e.g. brand-new session).
pub fn away_summary_banner(
    idle_secs: Option<i64>,
    title: Option<&str>,
    user_msg_count: usize,
    tool_call_count: usize,
    total_tokens: i64,
) -> Vec<Line<'static>> {
    let mut parts: Vec<String> = Vec::new();

    if user_msg_count > 0 {
        let msgs_label = match user_msg_count {
            1 => "1 msg".to_string(),
            n => format!("{n} msgs"),
        };
        parts.push(msgs_label);
    }

    if tool_call_count > 0 {
        parts.push(format!(
            "{tool_call_count} tool call{}",
            if tool_call_count == 1 { "" } else { "s" }
        ));
    }

    if total_tokens > 0 {
        parts.push(format!("{}k tok", total_tokens / 1000));
    }

    if let Some(secs) = idle_secs
        && let Some(away) = format_idle(secs)
    {
        parts.push(format!("away {away}"));
    }

    // Nothing useful to show—skip the banner entirely
    if parts.is_empty() {
        return Vec::new();
    }

    let stats: String = parts.join(" \u{00b7} "); // middot separator
    let title_span = title
        .map(|t| {
            let t: String = t.chars().take(50).collect();
            Span::styled(format!("“{t}”  "), WARM_ACCENT)
        })
        .unwrap_or_else(|| Span::raw(""));

    vec![
        Line::default(),
        Line::from(vec![
            Span::styled("  \u{1f4cb} ", CYAN),
            title_span,
            Span::styled(stats, WARM_MUTED),
        ]),
        Line::default(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_thresholds() {
        assert!(format_idle(0).is_none());
        assert!(format_idle(299).is_none()); // < 5 min — silent
        assert_eq!(format_idle(300).as_deref(), Some("5 min"));
        assert_eq!(format_idle(3600).as_deref(), Some("1 h"));
        assert_eq!(format_idle(86400).as_deref(), Some("1 day"));
        assert_eq!(format_idle(172800).as_deref(), Some("2 days"));
        assert_eq!(format_idle(604800).as_deref(), Some("1 week"));
        assert_eq!(format_idle(1209600).as_deref(), Some("2 weeks"));
    }

    #[test]
    fn banner_empty_for_zero_messages() {
        let lines = away_summary_banner(None, None, 0, 0, 0);
        // No stats → no banner
        assert!(lines.is_empty());
    }

    #[test]
    fn banner_shows_with_messages() {
        let lines = away_summary_banner(Some(7200), None, 5, 3, 15000);
        // blank + content + blank = 3 lines
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn banner_silent_for_short_idle() {
        // < 5 min away — the "away X" part is suppressed but msgs still show
        let lines = away_summary_banner(Some(30), None, 2, 0, 0);
        assert_eq!(lines.len(), 3); // banner still appears (has msg count)
    }
}
