//! Ephemeral `?` help overlay — multi-column shortcut reference.
//!
//! Press `?` when the input is empty to show shortcuts.
//! Any subsequent keypress dismisses the overlay.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

// Warm brown for the overlay (matches separator)
const HELP_DIM: Style = Style::new().fg(Color::Rgb(124, 111, 100));
const HELP_KEY: Style = Style::new()
    .fg(Color::Rgb(209, 154, 102)) // WARM_ACCENT
    .add_modifier(Modifier::BOLD);
const HELP_DESC: Style = Style::new().fg(Color::Rgb(198, 165, 106)); // WARM_INFO

/// Build the help overlay as `Vec<Line>` for rendering above the input.
///
/// Three columns of shortcut pairs, grouped by interaction type.
pub fn build_help_lines() -> Vec<Line<'static>> {
    // Each entry: (key, description)
    let col1 = [
        ("/command", "slash commands"),
        ("@file", "reference a file"),
        ("?", "show this help"),
    ];
    let col2 = [
        ("Shift+Tab", "cycle mode"),
        ("Ctrl+O", "toggle verbose"),
        ("Tab", "autocomplete"),
    ];
    let col3 = [
        ("Ctrl+C", "cancel / quit"),
        ("Ctrl+D", "exit"),
        ("Shift+Enter", "newline"),
    ];

    let mut lines = Vec::with_capacity(col1.len() + 1);
    lines.push(Line::from(Span::styled("  Keyboard shortcuts", HELP_DIM)));

    for i in 0..col1.len() {
        let mut spans = Vec::with_capacity(12);
        spans.push(Span::raw("  "));

        for (j, col) in [&col1, &col2, &col3].iter().enumerate() {
            if j > 0 {
                spans.push(Span::styled("  │  ", HELP_DIM));
            }
            let (key, desc) = col[i];
            spans.push(Span::styled(format!("{key:>12}"), HELP_KEY));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(desc, HELP_DESC));
        }

        lines.push(Line::from(spans));
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_lines_not_empty() {
        let lines = build_help_lines();
        assert!(!lines.is_empty());
    }

    #[test]
    fn help_has_title() {
        let lines = build_help_lines();
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("Keyboard shortcuts"));
    }

    #[test]
    fn help_contains_shortcuts() {
        let lines = build_help_lines();
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("Ctrl+C"));
        assert!(text.contains("Shift+Tab"));
        assert!(text.contains("/command"));
    }

    #[test]
    fn help_is_compact() {
        let lines = build_help_lines();
        // 1 title + 3 shortcut rows = 4 lines
        assert_eq!(lines.len(), 4);
    }
}
