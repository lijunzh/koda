//! Startup banner and pre-raw-mode messages.
//!
//! Builds ratatui `Line`s and prints them via `tui_output::write_line()`.
//! All builder functions return `Vec<Line>` for testability; thin
//! `print_*` wrappers handle the actual output.

use crate::tui_output::{self, BOLD, CYAN, DIM};
use koda_core::config::KodaConfig;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

// ── Style constants (local) ────────────────────────────────
const BORDER: Style = Style::new().fg(Color::Cyan);
const INFO: Style = Style::new().fg(Color::Blue);
const TITLE: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);

// ── Column geometry ─────────────────────────────────────
const LEFT_W: usize = 34;
const RIGHT_W: usize = 56;
const TOTAL_W: usize = LEFT_W + 3 + RIGHT_W; // 3 = " │ "

// ── Banner ───────────────────────────────────────────────

/// Build the two-column banner as a `Vec<Line>` (testable).
pub fn build_banner_lines(
    model: &str,
    provider: &str,
    cwd: &str,
    recent_activity: &[String],
) -> Vec<Line<'static>> {
    let ver = env!("CARGO_PKG_VERSION");
    let mut lines = Vec::new();

    // Top border with embedded title
    let title_text = format!(" \u{1f43b} Koda v{ver} ");
    let remaining = (TOTAL_W + 2).saturating_sub(title_text.chars().count() + 2);
    lines.push(Line::from(vec![
        Span::styled("  ╭──", BORDER),
        Span::styled(title_text, TITLE),
        Span::styled(format!("{}╮", "─".repeat(remaining)), BORDER),
    ]));

    // Left column
    let left: Vec<Vec<Span>> = vec![
        vec![],
        vec![Span::styled("   Welcome back!", BOLD)],
        vec![],
        vec![Span::styled(format!("   {model}"), CYAN)],
        vec![Span::styled(format!("   {provider}"), CYAN)],
        vec![Span::styled(format!("   {cwd}"), INFO)],
    ];

    // Right column
    let sep = "─".repeat(RIGHT_W);
    let mut right: Vec<Vec<Span>> = vec![
        vec![Span::styled("Tips for getting started", TITLE)],
        vec![Span::styled("/model", DIM), Span::raw("      pick a model")],
        vec![
            Span::styled("/provider", DIM),
            Span::raw("   switch provider"),
        ],
        vec![Span::styled("/help", DIM), Span::raw("       all commands")],
        vec![
            Span::styled("Shift+Tab", DIM),
            Span::raw("  cycle mode: auto → strict → safe"),
        ],
        vec![Span::styled(sep, DIM)],
    ];

    right.push(vec![Span::styled("Recent activity", TITLE)]);
    if recent_activity.is_empty() {
        right.push(vec![Span::styled("No recent activity", DIM)]);
    } else {
        for msg in recent_activity.iter().take(3) {
            let text = msg.lines().next().unwrap_or("");
            let truncated = truncate(text, 52);
            right.push(vec![Span::styled("• ", DIM), Span::raw(truncated)]);
        }
    }

    // Render rows
    let rows = left.len().max(right.len());
    let empty: Vec<Span> = vec![];

    for i in 0..rows {
        let l_spans = left.get(i).unwrap_or(&empty);
        let r_spans = right.get(i).unwrap_or(&empty);
        let l_len: usize = l_spans.iter().map(|s| span_width(s)).sum();
        let r_len: usize = r_spans.iter().map(|s| span_width(s)).sum();

        let mut spans = Vec::with_capacity(l_spans.len() + r_spans.len() + 5);
        spans.push(Span::styled("  │ ", BORDER));
        spans.extend(l_spans.iter().cloned());
        spans.push(Span::raw(" ".repeat(LEFT_W.saturating_sub(l_len))));
        spans.push(Span::styled(" │ ", DIM));
        spans.extend(r_spans.iter().cloned());
        spans.push(Span::raw(" ".repeat(RIGHT_W.saturating_sub(r_len))));
        spans.push(Span::styled(" │", BORDER));

        lines.push(Line::from(spans));
    }

    // Bottom border
    lines.push(Line::from(vec![Span::styled(
        format!("  ╰{}╯", "─".repeat(TOTAL_W + 2)),
        BORDER,
    )]));

    lines
}

/// Print the two-column startup banner.
pub fn print_banner(config: &KodaConfig, recent_activity: &[String]) {
    let cwd = pretty_cwd();
    let lines = build_banner_lines(
        &config.model,
        &config.provider_type.to_string(),
        &cwd,
        recent_activity,
    );
    tui_output::write_blank();
    for line in &lines {
        tui_output::write_line(line);
    }
    tui_output::write_blank();
}

// ── Warnings & notices ──────────────────────────────────

/// Print model-related warnings (auto-detect failures).
pub fn print_model_warning(config: &KodaConfig) {
    if config.model == "(no model loaded)" {
        tui_output::warn_msg(format!("No model loaded in {}.", config.provider_type));
        tui_output::dim_msg("Load a model, then use /model to select it.".into());
    } else if config.model == "(connection failed)" {
        tui_output::write_line(&Line::from(vec![
            Span::styled("  \u{2717} ", Style::new().fg(Color::Red)),
            Span::styled(
                format!(
                    "Could not connect to {} at {}",
                    config.provider_type, config.base_url
                ),
                Style::new().fg(Color::Red),
            ),
        ]));
    }
}

/// Print update-available notice.
pub fn print_update_notice(current: &str, latest: &str) {
    let crate_name = koda_core::version::crate_name();
    tui_output::write_line(&Line::from(vec![
        Span::styled("  \u{2728} Update available: ", DIM),
        Span::styled(current, CYAN),
        Span::styled(" → ", DIM),
        Span::styled(latest, Style::new().fg(Color::Green)),
        Span::styled(format!("  (cargo install {crate_name})"), DIM),
    ]));
    tui_output::write_blank();
}

/// Print MCP server connection status.
pub fn print_mcp_status(statuses: &[(String, Result<usize, String>)]) {
    if statuses.is_empty() {
        return;
    }
    tui_output::write_line(&Line::from(vec![Span::styled(
        format!(
            "  \u{1f50c} Connecting to {} MCP server(s)...",
            statuses.len()
        ),
        CYAN,
    )]));
    for (name, result) in statuses {
        match result {
            Ok(tool_count) => {
                tui_output::write_line(&Line::from(vec![
                    Span::styled("  \u{2713} ", Style::new().fg(Color::Green)),
                    Span::raw(format!("{name} — {tool_count} tool(s)")),
                ]));
            }
            Err(msg) => {
                tui_output::write_line(&Line::from(vec![
                    Span::styled("  \u{2717} ", Style::new().fg(Color::Red)),
                    Span::raw(format!("{name} — {msg}")),
                ]));
            }
        }
    }
    tui_output::write_blank();
}

/// Print session resume hint (after raw mode ends).
pub fn print_resume_hint(session_id: &str) {
    tui_output::write_line(&Line::styled(
        format!("\nResume this session with:\n  koda --resume {session_id}"),
        DIM,
    ));
}

// ── Helpers ─────────────────────────────────────────────────

/// Visible character width of a Span (emoji = 2, ASCII = 1).
pub(crate) fn span_width(span: &Span) -> usize {
    span.content
        .chars()
        .map(|c| if c > '\u{FFFF}' { 2 } else { 1 })
        .sum()
}

/// Truncate a string to `max` visible characters, appending "…" if needed.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    let mut visible = 0;
    for (i, c) in s.char_indices() {
        let w = if c > '\u{FFFF}' { 2 } else { 1 };
        if visible + w > max.saturating_sub(1) {
            return format!("{}…", &s[..i]);
        }
        visible += w;
    }
    s.to_string()
}

/// Collapse $HOME to ~ in the current directory.
fn pretty_cwd() -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE"))
        && let Ok(rest) = cwd.strip_prefix(&home)
    {
        return format!("~/{}", rest.display())
            .trim_end_matches('/')
            .to_string();
    }
    cwd.display().to_string()
}

/// Extract all text content from a slice of Lines (used by tests).
#[cfg(test)]
pub(crate) fn lines_to_text(lines: &[Line]) -> String {
    lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_contains_model_name() {
        let lines = build_banner_lines("gpt-4o", "openai", "~/projects/koda", &[]);
        let text = lines_to_text(&lines);
        assert!(text.contains("gpt-4o"), "Banner should contain model name");
    }

    #[test]
    fn banner_contains_provider() {
        let lines = build_banner_lines("claude-sonnet", "anthropic", "~/repo", &[]);
        let text = lines_to_text(&lines);
        assert!(text.contains("anthropic"));
    }

    #[test]
    fn banner_contains_cwd() {
        let lines = build_banner_lines("m", "p", "/tmp/test", &[]);
        let text = lines_to_text(&lines);
        assert!(text.contains("/tmp/test"));
    }

    #[test]
    fn banner_contains_version() {
        let lines = build_banner_lines("m", "p", "~", &[]);
        let text = lines_to_text(&lines);
        let ver = env!("CARGO_PKG_VERSION");
        assert!(text.contains(ver), "Banner should contain version {ver}");
    }

    #[test]
    fn banner_shows_recent_activity() {
        let recent = vec!["Fixed bug in auth".into(), "Added tests".into()];
        let lines = build_banner_lines("m", "p", "~", &recent);
        let text = lines_to_text(&lines);
        assert!(text.contains("Fixed bug"));
        assert!(text.contains("Added tests"));
    }

    #[test]
    fn banner_no_activity_placeholder() {
        let lines = build_banner_lines("m", "p", "~", &[]);
        let text = lines_to_text(&lines);
        assert!(text.contains("No recent activity"));
    }

    #[test]
    fn banner_contains_tips() {
        let lines = build_banner_lines("m", "p", "~", &[]);
        let text = lines_to_text(&lines);
        assert!(text.contains("/model"));
        assert!(text.contains("/help"));
        assert!(text.contains("Shift+Tab"));
    }

    #[test]
    fn banner_has_box_borders() {
        let lines = build_banner_lines("m", "p", "~", &[]);
        let text = lines_to_text(&lines);
        assert!(text.contains('\u{256d}'), "Top-left corner");
        assert!(text.contains('\u{256e}'), "Top-right corner");
        assert!(text.contains('\u{2570}'), "Bottom-left corner");
        assert!(text.contains('\u{256f}'), "Bottom-right corner");
    }

    #[test]
    fn truncate_short_unchanged() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_adds_ellipsis() {
        let result = truncate("a very long string that exceeds", 10);
        assert!(result.ends_with('\u{2026}'));
    }

    #[test]
    fn span_width_ascii() {
        assert_eq!(span_width(&Span::raw("hello")), 5);
    }

    #[test]
    fn span_width_emoji() {
        assert_eq!(span_width(&Span::raw("\u{1f43b}")), 2); // bear
    }

    #[test]
    fn banner_recent_truncates_long_messages() {
        let long_msg = "x".repeat(200);
        let lines = build_banner_lines("m", "p", "~", &[long_msg]);
        let text = lines_to_text(&lines);
        // Should contain truncated version with ellipsis, not the full 200 chars
        assert!(
            text.contains('\u{2026}'),
            "Long messages should be truncated"
        );
    }
}
