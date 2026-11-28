//! Startup banner and initial messages.
//!
//! All builder functions return `Vec<Line<'static>>` which get pushed
//! into the scroll buffer during `TuiContext::new()`.

use crate::tui_output::{DIM, WARM_ACCENT, WARM_INFO, WARM_MUTED, WARM_TITLE};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

// ── Banner ───────────────────────────────────────────

/// The banner: Koda's logo, the bear's paw print, beside the version, the
/// model and the folder. A terminal that can't draw the print's dots gets
/// the three lines alone.
pub fn build_banner_lines(model: &str, provider: &str, cwd: &str) -> Vec<Line<'static>> {
    banner_lines(model, provider, cwd, draws_braille())
}

/// Whether the terminal's font has Braille characters, which the print is
/// drawn with. The Linux console's doesn't; terminal emulators' do.
fn draws_braille() -> bool {
    std::env::var("TERM").map_or(true, |term| term != "linux")
}

fn banner_lines(model: &str, provider: &str, cwd: &str, print: bool) -> Vec<Line<'static>> {
    let ver = env!("CARGO_PKG_VERSION");

    // The logo's print (apps/desktop/assets/icon.svg) in Braille dots, as
    // Codex draws its logo in the terminal: five rows of ten, 20 dots by 20.
    const PAW: [&str; 5] = [
        "   \u{2880}\u{2844}\u{2880}\u{28e4}   ",
        " \u{28fe}\u{2846}\u{283f}\u{2807}\u{28bf}\u{281f}\u{28e0}\u{2846} ",
        " \u{28c9}\u{28e5}\u{28fe}\u{28ff}\u{28f7}\u{285c}\u{281b}\u{28c1}\u{2840}",
        "\u{28b0}\u{28ff}\u{28ff}\u{28ff}\u{28ff}\u{28ff}\u{28f7}\u{2818}\u{281f}\u{2801}",
        " \u{2809}\u{2809}\u{281b}\u{28bf}\u{28ff}\u{287f}   ",
    ];

    let info = [
        vec![Span::styled(format!("Koda v{ver}"), WARM_TITLE)],
        vec![
            Span::styled(model.to_string(), WARM_INFO),
            Span::styled(" \u{00b7} ", WARM_MUTED),
            Span::styled(provider.to_string(), WARM_MUTED),
        ],
        vec![
            Span::styled(cwd.to_string(), DIM),
            // The hint shares the folder's line, so the three lines sit
            // centred beside the print's five rows.
            Span::styled("  \u{00b7} ", WARM_MUTED),
            Span::styled("?", WARM_ACCENT),
            Span::styled(" for shortcuts", DIM),
        ],
    ];

    let mut lines = vec![Line::default()];
    if print {
        let mut info = info.into_iter();
        for (row, dots) in PAW.iter().enumerate() {
            let mut spans = vec![Span::styled(format!(" {dots}"), WARM_ACCENT)];
            // The middle three rows carry the three lines.
            if (1..=3).contains(&row)
                && let Some(text) = info.next()
            {
                spans.push(Span::raw("  "));
                spans.extend(text);
            }
            lines.push(Line::from(spans));
        }
    } else {
        for text in info {
            let mut spans = vec![Span::raw(" ")];
            spans.extend(text);
            lines.push(Line::from(spans));
        }
    }
    lines.push(Line::default());
    lines
}

/// Collect all startup lines (banner + warnings + notices), for the
/// session's settings as the host reports them.
pub fn collect_startup_lines(
    settings: &koda_client::protocol::session::SessionSettings,
    providers: &crate::providers::Providers,
) -> Vec<Line<'static>> {
    let cwd = pretty_cwd();
    let provider = providers.name(&settings.model_provider).to_string();
    let mut lines = build_banner_lines(&settings.model, &settings.model_provider, &cwd);

    // Model warnings
    if settings.model == "(no model loaded)" {
        lines.push(Line::from(vec![
            Span::styled("  \u{26a0} ", Style::new().fg(Color::Yellow)),
            Span::styled(
                format!("No model loaded in {provider}."),
                Style::new().fg(Color::Yellow),
            ),
        ]));
        lines.push(Line::styled(
            "  Load a model, then use /model to select it.",
            DIM,
        ));
    } else if settings.model == "(connection failed)" {
        lines.push(Line::from(vec![
            Span::styled("  \u{2717} ", Style::new().fg(Color::Red)),
            Span::styled(
                format!("Could not connect to {provider} at {}", settings.base_url),
                Style::new().fg(Color::Red),
            ),
        ]));
    }

    lines
}

/// Build update-available notice lines.
pub fn update_notice_lines(current: &str, latest: &str) -> Vec<Line<'static>> {
    let crate_name = crate::updates::crate_name();
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

/// A yellow `⚠` startup line.
pub fn warning_line(message: String) -> Line<'static> {
    Line::from(vec![
        Span::styled("  \u{26a0} ", Style::new().fg(Color::Yellow)),
        Span::styled(message, Style::new().fg(Color::Yellow)),
    ])
}

/// Build home-directory footgun warning lines.
///
/// Returns empty vec when project root is NOT the home directory.
pub fn home_dir_warning_lines(project_root: &std::path::Path) -> Vec<Line<'static>> {
    let home = match std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        Ok(h) => h,
        Err(_) => return vec![],
    };
    let home_path = match std::fs::canonicalize(&home) {
        Ok(p) => p,
        Err(_) => return vec![],
    };
    if project_root != home_path {
        return vec![];
    }
    vec![
        Line::from(vec![
            Span::styled("  \u{26a0}\u{fe0f}  ", Style::new().fg(Color::Yellow)),
            Span::styled(
                format!(
                    "Project root is your home directory ({}).",
                    project_root.display()
                ),
                Style::new().fg(Color::Yellow),
            ),
        ]),
        Line::styled(
            "     koda can modify any file in this tree. Consider running from a project subdirectory.",
            Style::new().fg(Color::Yellow),
        ),
        Line::default(),
    ]
}

/// Print session resume hint (after raw mode ends, to stdout).
///
/// NOTE: Session IDs are non-sensitive local identifiers (UUIDs stored in
/// a local SQLite database). Printing them to stdout is intentional UX —
/// the user needs the ID to resume their session. This is not a credential
/// leak. (Addresses CodeQL alert #7 / `rust/cleartext-logging`.)
pub fn print_resume_hint(session_id: &str) {
    println!("\nResume this session with:\n  koda --resume {session_id}");
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
    fn home_dir_warning_when_at_home() {
        // Use a path that definitely is NOT the home dir so we get empty.
        let lines = home_dir_warning_lines(std::path::Path::new("/tmp/definitely-not-home"));
        assert!(
            lines.is_empty(),
            "Should produce no warning for non-home dir"
        );
    }

    #[test]
    fn home_dir_warning_contains_text() {
        // When project_root == home, we should get warning lines.
        if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE"))
            && let Ok(home_path) = std::fs::canonicalize(&home)
        {
            let lines = home_dir_warning_lines(&home_path);
            let text = lines_to_text(&lines);
            assert!(
                text.contains("home directory"),
                "Warning should mention home directory"
            );
            assert!(
                text.contains("subdirectory"),
                "Warning should suggest subdirectory"
            );
        }
    }

    #[test]
    fn banner_contains_model_name() {
        let lines = build_banner_lines("gpt-4o", "openai", "~/projects/koda");
        let text = lines_to_text(&lines);
        assert!(text.contains("gpt-4o"));
    }

    #[test]
    fn banner_contains_provider() {
        let lines = build_banner_lines("claude-sonnet", "anthropic", "~/repo");
        let text = lines_to_text(&lines);
        assert!(text.contains("anthropic"));
    }

    #[test]
    fn banner_contains_cwd() {
        let lines = build_banner_lines("m", "p", "/tmp/test");
        let text = lines_to_text(&lines);
        assert!(text.contains("/tmp/test"));
    }

    #[test]
    fn banner_contains_version() {
        let lines = build_banner_lines("m", "p", "~");
        let text = lines_to_text(&lines);
        let ver = env!("CARGO_PKG_VERSION");
        assert!(text.contains(ver));
    }

    #[test]
    fn banner_is_the_print_beside_three_lines() {
        // A blank line, the print's five rows, a blank line. The version,
        // the model and the folder sit on the middle three.
        let lines = banner_lines("gpt-4o", "openai", "~/repo", true);
        assert_eq!(lines.len(), 7);
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(text[2].contains("Koda v"), "{text:?}");
        assert!(text[3].contains("gpt-4o"), "{text:?}");
        assert!(text[4].contains("~/repo"), "{text:?}");
        // Every row of the print is as wide as the others, so the text lines up.
        let widths: Vec<usize> = lines[1..6]
            .iter()
            .map(|l| l.spans[0].content.chars().count())
            .collect();
        assert!(widths.iter().all(|w| *w == widths[0]), "{widths:?}");
    }

    #[test]
    fn a_terminal_without_braille_gets_the_three_lines_alone() {
        let lines = banner_lines("gpt-4o", "openai", "~/repo", false);
        assert_eq!(lines.len(), 5);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("Koda v") && text.contains("gpt-4o") && text.contains("~/repo"));
        assert!(!text.chars().any(|c| ('\u{2800}'..='\u{28ff}').contains(&c)));
    }
}
