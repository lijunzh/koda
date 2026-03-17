//! Startup banner and initial messages.
//!
//! All builder functions return `Vec<Line<'static>>` which get pushed
//! into the scroll buffer during `TuiContext::new()`.

use crate::tui_output::{DIM, WARM_ACCENT, WARM_INFO, WARM_MUTED, WARM_TITLE};
use koda_core::config::KodaConfig;
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

// ── Banner ───────────────────────────────────────────

pub fn build_banner_lines(
    model: &str,
    provider: &str,
    cwd: &str,
    _recent_activity: &[String],
) -> Vec<Line<'static>> {
    let ver = env!("CARGO_PKG_VERSION");

    const BEAR: [&str; 3] = [
        "\u{259e}\u{2580}\u{259a}\u{2584}\u{2584}\u{259e}\u{2580}\u{259a}",
        "\u{258c}\u{00b7}\u{2590}\u{2580}\u{258c}\u{00b7}\u{2590} ",
        "\u{2580}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2580}",
    ];

    vec![
        Line::default(),
        Line::from(vec![
            Span::styled(format!(" {}", BEAR[0]), WARM_ACCENT),
            Span::raw("  "),
            Span::styled(format!("Koda v{ver}"), WARM_TITLE),
        ]),
        Line::from(vec![
            Span::styled(format!(" {}", BEAR[1]), WARM_ACCENT),
            Span::raw("  "),
            Span::styled(model.to_string(), WARM_INFO),
            Span::styled(" \u{00b7} ", WARM_MUTED),
            Span::styled(provider.to_string(), WARM_MUTED),
        ]),
        Line::from(vec![
            Span::styled(format!(" {}", BEAR[2]), WARM_ACCENT),
            Span::raw("  "),
            Span::styled(cwd.to_string(), DIM),
        ]),
        Line::from(vec![
            Span::styled("  /", WARM_ACCENT),
            Span::styled("commands", DIM),
            Span::styled("  @", WARM_ACCENT),
            Span::styled("file", DIM),
            Span::styled("  Shift+Tab ", WARM_ACCENT),
            Span::styled("mode", DIM),
            Span::styled("  Ctrl+C ", WARM_ACCENT),
            Span::styled("cancel", DIM),
            Span::styled("  PgUp/PgDn ", WARM_ACCENT),
            Span::styled("scroll", DIM),
            Span::styled("  Ctrl+Y ", WARM_ACCENT),
            Span::styled("copy", DIM),
            Span::styled("  Ctrl+D ", WARM_ACCENT),
            Span::styled("quit", DIM),
        ]),
        Line::default(),
    ]
}

/// Collect all startup lines (banner + warnings + notices).
pub fn collect_startup_lines(
    config: &KodaConfig,
    recent_activity: &[String],
) -> Vec<Line<'static>> {
    let cwd = pretty_cwd();
    let mut lines = build_banner_lines(
        &config.model,
        &config.provider_type.to_string(),
        &cwd,
        recent_activity,
    );

    // Model warnings
    if config.model == "(no model loaded)" {
        lines.push(Line::from(vec![
            Span::styled("  \u{26a0} ", Style::new().fg(Color::Yellow)),
            Span::styled(
                format!("No model loaded in {}.", config.provider_type),
                Style::new().fg(Color::Yellow),
            ),
        ]));
        lines.push(Line::styled(
            "  Load a model, then use /model to select it.",
            DIM,
        ));
    } else if config.model == "(connection failed)" {
        lines.push(Line::from(vec![
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

    lines
}

/// Build update-available notice lines.
pub fn update_notice_lines(current: &str, latest: &str) -> Vec<Line<'static>> {
    let crate_name = koda_core::version::crate_name();
    vec![
        Line::from(vec![
            Span::styled("  \u{2728} Update available: ", DIM),
            Span::styled(current.to_string(), WARM_ACCENT),
            Span::styled(" \u{2192} ", DIM),
            Span::styled(latest.to_string(), Style::new().fg(Color::Green)),
            Span::styled(format!("  (cargo install {crate_name})"), DIM),
        ]),
        Line::default(),
    ]
}

/// Build purge nudge lines.
pub fn purge_nudge_lines(size_str: &str) -> Vec<Line<'static>> {
    vec![Line::from(vec![
        Span::styled("  \u{1f4a1} ", Style::default()),
        Span::styled(
            format!("{size_str} of archived history \u{2014} run /purge to clean up"),
            DIM,
        ),
    ])]
}

/// Print session resume hint (after raw mode ends, to stdout).
pub fn print_resume_hint(session_id: &str) {
    println!("\nResume this session with:\n  koda --resume {session_id}");
}

/// Nudge threshold: 500MB of compacted data.
pub const PURGE_NUDGE_BYTES: i64 = 500 * 1024 * 1024;

/// Append purge nudge lines if compacted data exceeds threshold.
pub async fn purge_nudge(db: &koda_core::db::Database, lines: &mut Vec<Line<'static>>) {
    use koda_core::persistence::Persistence;
    match db.compacted_stats().await {
        Ok(stats) if stats.size_bytes >= PURGE_NUDGE_BYTES => {
            let size = crate::tui_wizards::format_bytes(stats.size_bytes);
            lines.extend(purge_nudge_lines(&size));
        }
        _ => {}
    }
}

// ── Helpers ───────────────────────────────────────────

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
        assert!(text.contains("gpt-4o"));
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
        assert!(text.contains(ver));
    }

    #[test]
    fn banner_is_compact() {
        let lines = build_banner_lines("gpt-4o", "openai", "~/repo", &[]);
        // 3 bear lines + blank top + tips + blank bottom = 6
        assert_eq!(lines.len(), 6);
    }
}
