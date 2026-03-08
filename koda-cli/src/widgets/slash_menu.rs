//! Inline slash command menu — rendered inside the ratatui viewport.
//!
//! Appears when the user types `/` in an empty input. Filters live
//! as the user continues typing. Fully reactive — no crossterm
//! direct writes, no terminal reinit needed.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

// Warm palette (matches help_overlay and separator)
const DIM: Style = Style::new().fg(Color::Rgb(124, 111, 100));
const SELECTED_CMD: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
const UNSELECTED_CMD: Style = Style::new().fg(Color::Rgb(124, 111, 100));
const DESC: Style = Style::new().fg(Color::Rgb(198, 165, 106));
const HINT: Style = Style::new().fg(Color::Rgb(124, 111, 100));

/// Max visible items in the dropdown (scroll for more).
const MAX_VISIBLE: usize = 6;

/// State for the active slash command menu.
#[derive(Clone)]
pub struct SlashMenuState {
    /// Currently matching commands: (command, description).
    pub filtered: Vec<(&'static str, &'static str)>,
    /// Index of the highlighted option.
    pub selected: usize,
    /// Scroll offset for the visible window.
    pub scroll_offset: usize,
}

impl SlashMenuState {
    /// Create a new menu state by filtering commands against input.
    /// Returns `None` if no commands match.
    pub fn from_input(
        commands: &'static [(&'static str, &'static str)],
        input: &str,
    ) -> Option<Self> {
        let filtered: Vec<_> = commands
            .iter()
            .filter(|(cmd, _)| cmd.starts_with(input))
            .copied()
            .collect();
        if filtered.is_empty() {
            None
        } else {
            Some(Self {
                filtered,
                selected: 0,
                scroll_offset: 0,
            })
        }
    }

    /// Move selection up.
    pub fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
        // Keep selected in visible window
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        }
    }

    /// Move selection down (wraps around).
    pub fn down(&mut self) {
        if self.selected + 1 < self.filtered.len() {
            self.selected += 1;
        } else {
            self.selected = 0;
            self.scroll_offset = 0;
        }
        // Keep selected in visible window
        let visible = MAX_VISIBLE.min(self.filtered.len());
        if self.selected >= self.scroll_offset + visible {
            self.scroll_offset = self.selected + 1 - visible;
        }
    }

    /// Get the currently selected command string.
    pub fn selected_command(&self) -> &'static str {
        self.filtered[self.selected].0
    }
}

/// Build the slash menu as `Vec<Line>` for rendering in the viewport.
/// Always returns exactly `MAX_VISIBLE + 2` lines (fixed height).
pub fn build_menu_lines(state: &SlashMenuState) -> Vec<Line<'static>> {
    let visible = MAX_VISIBLE.min(state.filtered.len());
    let end = (state.scroll_offset + visible).min(state.filtered.len());
    let window = &state.filtered[state.scroll_offset..end];
    let has_above = state.scroll_offset > 0;
    let has_below = end < state.filtered.len();

    let mut lines = Vec::with_capacity(MAX_VISIBLE + 2);

    // Title with scroll indicator
    let title = if has_above {
        "  \u{1f43b} Commands  \u{25b2} more"
    } else {
        "  \u{1f43b} Commands"
    };
    lines.push(Line::from(Span::styled(title, DIM)));

    // Visible options
    for (i, (cmd, desc)) in window.iter().enumerate() {
        let absolute_idx = state.scroll_offset + i;
        let is_selected = absolute_idx == state.selected;
        let mut spans = Vec::with_capacity(4);

        if is_selected {
            spans.push(Span::styled(
                "  \u{203a} ",
                Style::default().fg(Color::Cyan),
            ));
            spans.push(Span::styled(*cmd, SELECTED_CMD));
        } else {
            spans.push(Span::raw("    "));
            spans.push(Span::styled(*cmd, UNSELECTED_CMD));
        }
        spans.push(Span::styled(format!("  {desc}"), DESC));

        lines.push(Line::from(spans));
    }

    // Pad empty slots to maintain fixed height
    for _ in visible..MAX_VISIBLE {
        lines.push(Line::from(""));
    }

    // Hint with scroll indicator
    let hint = if has_below {
        "  \u{2191}/\u{2193} navigate \u{00b7} enter select \u{00b7} esc cancel  \u{25bc} more"
    } else {
        "  \u{2191}/\u{2193} navigate \u{00b7} enter select \u{00b7} esc cancel"
    };
    lines.push(Line::from(Span::styled(hint, HINT)));

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_COMMANDS: &[(&str, &str)] = &[
        ("/agent", "Agents"),
        ("/compact", "Compact"),
        ("/cost", "Cost"),
        ("/diff", "Diff"),
        ("/exit", "Quit"),
        ("/expand", "Expand"),
        ("/mcp", "MCP"),
        ("/model", "Pick model"),
    ];

    #[test]
    fn from_input_all() {
        let state = SlashMenuState::from_input(TEST_COMMANDS, "/").unwrap();
        assert_eq!(state.filtered.len(), 8);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn from_input_filtered() {
        let state = SlashMenuState::from_input(TEST_COMMANDS, "/m").unwrap();
        assert_eq!(state.filtered.len(), 2); // /mcp, /model
        assert_eq!(state.filtered[0].0, "/mcp");
    }

    #[test]
    fn from_input_no_match() {
        assert!(SlashMenuState::from_input(TEST_COMMANDS, "/z").is_none());
    }

    #[test]
    fn navigation() {
        let mut state = SlashMenuState::from_input(TEST_COMMANDS, "/").unwrap();
        assert_eq!(state.selected_command(), "/agent");
        state.down();
        assert_eq!(state.selected_command(), "/compact");
        // Go to end
        for _ in 0..6 {
            state.down();
        }
        assert_eq!(state.selected_command(), "/model");
        state.down(); // wraps
        assert_eq!(state.selected_command(), "/agent");
        state.up(); // saturates at 0
        assert_eq!(state.selected_command(), "/agent");
    }

    #[test]
    fn scroll_indicator() {
        let state = SlashMenuState::from_input(TEST_COMMANDS, "/").unwrap();
        let lines = build_menu_lines(&state);
        // 8 items, 6 visible → should have ▼ more on hint
        let hint: String = lines
            .last()
            .unwrap()
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            hint.contains('\u{25bc}'),
            "should show scroll-down indicator: {hint}"
        );
    }

    #[test]
    fn build_lines_count_always_fixed() {
        // Full list
        let state = SlashMenuState::from_input(TEST_COMMANDS, "/").unwrap();
        let lines = build_menu_lines(&state);
        assert_eq!(lines.len(), 8); // title + 6 slots + hint
        // Filtered to 2 items — still 8 lines (4 blank padding)
        let state = SlashMenuState::from_input(TEST_COMMANDS, "/e").unwrap();
        let lines = build_menu_lines(&state);
        assert_eq!(lines.len(), 8);
    }

    #[test]
    fn build_lines_selected_marker() {
        let state = SlashMenuState::from_input(TEST_COMMANDS, "/").unwrap();
        let lines = build_menu_lines(&state);
        // First option should have the › marker
        let first_opt: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(first_opt.contains('\u{203a}'), "got: {first_opt}");
        // Second option should NOT
        let second_opt: String = lines[2].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!second_opt.contains('\u{203a}'), "got: {second_opt}");
    }
}
