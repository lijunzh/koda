//! Startup banner and pre-raw-mode messages.
//!
//! Builds ratatui `Line`s and prints them via `tui_output::write_line()`.
//! This replaces the hand-rolled ANSI escape codes that were in `repl.rs`.

use crate::tui_output::{self, BOLD, CYAN, DIM};
use koda_core::config::KodaConfig;
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

// ── Style constants (local) ────────────────────────────────
const BORDER: Style = Style::new().fg(Color::Cyan);
const INFO: Style = Style::new().fg(Color::Blue);

/// Print the two-column startup banner.
pub fn print_banner(config: &KodaConfig, recent_activity: &[String]) {
    let ver = env!("CARGO_PKG_VERSION");
    let cwd = pretty_cwd();

    // ── Column geometry ─────────────────────────────────────
    let left_w: usize = 34;
    let right_w: usize = 56;
    let total = left_w + 3 + right_w; // 3 = " │ "

    // ── Top border with embedded title ──────────────────────
    let title_text = format!(" \u{1f43b} Koda v{ver} ");
    let remaining = (total + 2).saturating_sub(title_text.chars().count() + 2);
    tui_output::write_line(&Line::from(vec![
        Span::styled("  ╭──", BORDER),
        Span::styled(
            title_text,
            Style::new()
                .fg(Color::Cyan)
                .add_modifier(ratatui::style::Modifier::BOLD),
        ),
        Span::styled(format!("{}╮", "─".repeat(remaining)), BORDER),
    ]));

    // ── Left column ─────────────────────────────────────────
    let left: Vec<Vec<Span>> = vec![
        vec![],
        vec![Span::styled("   Welcome back!", BOLD)],
        vec![],
        vec![Span::styled(format!("   {}", config.model), CYAN)],
        vec![Span::styled(format!("   {}", config.provider_type), CYAN)],
        vec![Span::styled(format!("   {}", cwd), INFO)],
    ];

    // ── Right column ────────────────────────────────────────
    let sep = "─".repeat(right_w);
    let mut right: Vec<Vec<Span>> = vec![
        vec![Span::styled(
            "Tips for getting started",
            Style::new()
                .fg(Color::Cyan)
                .add_modifier(ratatui::style::Modifier::BOLD),
        )],
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

    right.push(vec![Span::styled(
        "Recent activity",
        Style::new()
            .fg(Color::Cyan)
            .add_modifier(ratatui::style::Modifier::BOLD),
    )]);
    if recent_activity.is_empty() {
        right.push(vec![Span::styled("No recent activity", DIM)]);
    } else {
        for msg in recent_activity.iter().take(3) {
            let text = msg.lines().next().unwrap_or("");
            let truncated = truncate(text, 52);
            right.push(vec![Span::styled("• ", DIM), Span::raw(truncated)]);
        }
    }

    // ── Render rows ─────────────────────────────────────────
    let rows = left.len().max(right.len());
    let empty: Vec<Span> = vec![];

    tui_output::write_blank();
    for i in 0..rows {
        let l_spans = left.get(i).unwrap_or(&empty);
        let r_spans = right.get(i).unwrap_or(&empty);
        let l_len: usize = l_spans.iter().map(|s| span_width(s)).sum();
        let r_len: usize = r_spans.iter().map(|s| span_width(s)).sum();

        let mut spans = Vec::with_capacity(l_spans.len() + r_spans.len() + 5);
        spans.push(Span::styled("  │ ", BORDER));
        spans.extend(l_spans.iter().cloned());
        spans.push(Span::raw(" ".repeat(left_w.saturating_sub(l_len))));
        spans.push(Span::styled(" │ ", DIM));
        spans.extend(r_spans.iter().cloned());
        spans.push(Span::raw(" ".repeat(right_w.saturating_sub(r_len))));
        spans.push(Span::styled(" │", BORDER));

        tui_output::write_line(&Line::from(spans));
    }

    // ── Bottom border ───────────────────────────────────────
    tui_output::write_line(&Line::from(vec![Span::styled(
        format!("  ╰{}╯", "─".repeat(total + 2)),
        BORDER,
    )]));
    tui_output::write_blank();
}

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
fn span_width(span: &Span) -> usize {
    span.content
        .chars()
        .map(|c| if c > '\u{FFFF}' { 2 } else { 1 })
        .sum()
}

/// Truncate a string to `max` visible characters, appending "…" if needed.
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
