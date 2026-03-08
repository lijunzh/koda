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

/// State for the active slash command menu.
#[derive(Clone)]
pub struct SlashMenuState {
    /// Currently matching commands: (command, description).
    pub filtered: Vec<(&'static str, &'static str)>,
    /// Index of the highlighted option.
    pub selected: usize,
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
            })
        }
    }

    /// Move selection up.
    pub fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Move selection down (wraps around).
    pub fn down(&mut self) {
        if self.selected + 1 < self.filtered.len() {
            self.selected += 1;
        } else {
            self.selected = 0;
        }
    }

    /// Get the currently selected command string.
    pub fn selected_command(&self) -> &'static str {
        self.filtered[self.selected].0
    }

    /// Number of lines this menu will occupy (title + options + hint).
    pub fn height(&self) -> u16 {
        (self.filtered.len() + 2) as u16 // title + items + hint
    }
}

/// Build the slash menu as `Vec<Line>` for rendering in the viewport.
pub fn build_menu_lines(state: &SlashMenuState) -> Vec<Line<'static>> {
    let mut lines = Vec::with_capacity(state.filtered.len() + 2);

    // Title
    lines.push(Line::from(Span::styled("  \u{1f43b} Commands", DIM)));

    // Options
    for (i, (cmd, desc)) in state.filtered.iter().enumerate() {
        let is_selected = i == state.selected;
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

    // Hint
    lines.push(Line::from(Span::styled(
        "  \u{2191}/\u{2193} navigate \u{00b7} enter select \u{00b7} esc cancel",
        HINT,
    )));

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_COMMANDS: &[(&str, &str)] = &[
        ("/help", "Show help"),
        ("/model", "Pick model"),
        ("/exit", "Quit"),
    ];

    #[test]
    fn from_input_all() {
        let state = SlashMenuState::from_input(TEST_COMMANDS, "/").unwrap();
        assert_eq!(state.filtered.len(), 3);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn from_input_filtered() {
        let state = SlashMenuState::from_input(TEST_COMMANDS, "/m").unwrap();
        assert_eq!(state.filtered.len(), 1);
        assert_eq!(state.filtered[0].0, "/model");
    }

    #[test]
    fn from_input_no_match() {
        assert!(SlashMenuState::from_input(TEST_COMMANDS, "/z").is_none());
    }

    #[test]
    fn navigation() {
        let mut state = SlashMenuState::from_input(TEST_COMMANDS, "/").unwrap();
        assert_eq!(state.selected_command(), "/help");
        state.down();
        assert_eq!(state.selected_command(), "/model");
        state.down();
        assert_eq!(state.selected_command(), "/exit");
        state.down(); // wraps
        assert_eq!(state.selected_command(), "/help");
        state.up(); // saturates at 0
        assert_eq!(state.selected_command(), "/help");
    }

    #[test]
    fn height_includes_chrome() {
        let state = SlashMenuState::from_input(TEST_COMMANDS, "/").unwrap();
        assert_eq!(state.height(), 5); // title + 3 items + hint
    }

    #[test]
    fn build_lines_count() {
        let state = SlashMenuState::from_input(TEST_COMMANDS, "/").unwrap();
        let lines = build_menu_lines(&state);
        assert_eq!(lines.len(), 5);
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
