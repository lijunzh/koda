//! Startup banner and pre-raw-mode messages.
//!
//! Builds ratatui `Line`s and prints them via `tui_output::write_line()`.
//! All builder functions return `Vec<Line>` for testability; thin
//! `print_*` wrappers handle the actual output.

use crate::tui_output::{self, DIM, WARM_ACCENT, WARM_INFO, WARM_MUTED, WARM_TITLE};
use koda_core::config::KodaConfig;
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

// ── Banner ───────────────────────────────────────────────

/// Build the compact 3-line header with block-art bear.
///
/// ```text
///  ▞▀▚▄▄▞▀▚  Koda v0.1.3
///  ▌·▐▀▌·▐   gpt-4o · openai
///  ▀▄▄▄▄▄▄▀  ~/repo/koda
/// ```
pub fn build_banner_lines(
    model: &str,
    provider: &str,
    cwd: &str,
    _recent_activity: &[String],
) -> Vec<Line<'static>> {
    let ver = env!("CARGO_PKG_VERSION");

    // 3-line block-art bear (quadrant style).
    // Each line is 8 visual columns wide.
    const BEAR: [&str; 3] = ["▞▀▚▄▄▞▀▚", "▌·▐▀▌·▐ ", "▀▄▄▄▄▄▄▀"];

    vec![
        // Line 1: bear ears + name + version
        Line::from(vec![
            Span::styled(format!(" {}", BEAR[0]), WARM_ACCENT),
            Span::raw("  "),
            Span::styled(format!("Koda v{ver}"), WARM_TITLE),
        ]),
        // Line 2: bear face + model · provider
        Line::from(vec![
            Span::styled(format!(" {}", BEAR[1]), WARM_ACCENT),
            Span::raw("  "),
            Span::styled(model.to_string(), WARM_INFO),
            Span::styled(" · ", WARM_MUTED),
            Span::styled(provider.to_string(), WARM_MUTED),
        ]),
        // Line 3: bear chin + cwd
        Line::from(vec![
            Span::styled(format!(" {}", BEAR[2]), WARM_ACCENT),
            Span::raw("  "),
            Span::styled(cwd.to_string(), DIM),
        ]),
    ]
}

/// Print the compact startup header.
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
    // Keyboard tips (replaces the old ? overlay)
    tui_output::write_line(&Line::from(vec![
        Span::styled("  /", WARM_ACCENT),
        Span::styled("commands", DIM),
        Span::styled("  @", WARM_ACCENT),
        Span::styled("file", DIM),
        Span::styled("  Shift+Tab ", WARM_ACCENT),
        Span::styled("mode", DIM),
        Span::styled("  Ctrl+C ", WARM_ACCENT),
        Span::styled("cancel", DIM),
        Span::styled("  Ctrl+L ", WARM_ACCENT),
        Span::styled("refresh", DIM),
        Span::styled("  Ctrl+D ", WARM_ACCENT),
        Span::styled("quit", DIM),
    ]));
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
        Span::styled(current, WARM_ACCENT),
        Span::styled(" → ", DIM),
        Span::styled(latest, Style::new().fg(Color::Green)),
        Span::styled(format!("  (cargo install {crate_name})"), DIM),
    ]));
    tui_output::write_blank();
}

/// Print session resume hint (after raw mode ends).
pub fn print_resume_hint(session_id: &str) {
    tui_output::write_line(&Line::styled(
        format!("\nResume this session with:\n  koda --resume {session_id}"),
        DIM,
    ));
}

/// Nudge threshold: 500MB of compacted data.
const PURGE_NUDGE_BYTES: i64 = 500 * 1024 * 1024;

/// Print a one-line nudge if compacted data exceeds 500MB.
pub async fn print_purge_nudge_if_needed(db: &koda_core::db::Database) {
    match <koda_core::db::Database as koda_core::persistence::Persistence>::compacted_stats(db)
        .await
    {
        Ok(stats) if stats.size_bytes >= PURGE_NUDGE_BYTES => {
            let size = crate::tui_wizards::format_bytes(stats.size_bytes);
            tui_output::write_line(&Line::from(vec![
                Span::styled("  \u{1f4a1} ", Style::default()),
                Span::styled(
                    format!("{size} of archived history — run /purge to clean up"),
                    DIM,
                ),
            ]));
        }
        _ => {} // No stats, no nudge
    }
}

// ── Helpers ─────────────────────────────────────────────────

/// Visible character width of a Span (emoji = 2, ASCII = 1).
#[cfg(test)]
fn span_width(span: &Span) -> usize {
    span.content
        .chars()
        .map(|c| if c > '\u{FFFF}' { 2 } else { 1 })
        .sum()
}

/// Truncate a string to `max` visible characters, appending "…" if needed.
#[cfg(test)]
fn truncate(s: &str, max: usize) -> String {
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
    fn banner_contains_bear_face() {
        let lines = build_banner_lines("m", "p", "~", &[]);
        let text = lines_to_text(&lines);
        assert!(
            text.contains("▞▀▚"),
            "Banner should contain block-art bear ears"
        );
        // Block-art bear line 3: chin
        assert!(
            text.contains("▀▄▄▄▄▄▄▀"),
            "Banner should contain block-art bear chin"
        );
    }

    #[test]
    fn banner_is_compact() {
        let lines = build_banner_lines("gpt-4o", "openai", "~/repo", &[]);
        assert_eq!(lines.len(), 3, "Compact banner should be exactly 3 lines");
    }

    #[test]
    fn banner_model_dot_provider_format() {
        let lines = build_banner_lines("gpt-4o", "openai", "~", &[]);
        let text = lines_to_text(&lines);
        assert!(text.contains("gpt-4o"));
        assert!(text.contains(" · "));
        assert!(text.contains("openai"));
    }

    #[test]
    fn banner_no_box_borders() {
        let lines = build_banner_lines("m", "p", "~", &[]);
        let text = lines_to_text(&lines);
        assert!(!text.contains('╭'), "No top-left corner");
        assert!(!text.contains('╮'), "No top-right corner");
        assert!(!text.contains('╰'), "No bottom-left corner");
        assert!(!text.contains('╯'), "No bottom-right corner");
        assert!(!text.contains('│'), "No vertical borders");
    }

    #[test]
    fn truncate_short_unchanged() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_adds_ellipsis() {
        let result = truncate("a very long string that exceeds", 10);
        assert!(result.ends_with('…'));
    }

    #[test]
    fn span_width_ascii() {
        assert_eq!(span_width(&Span::raw("hello")), 5);
    }

    #[test]
    fn span_width_emoji() {
        assert_eq!(span_width(&Span::raw("\u{1f43b}")), 2); // bear
    }
}
