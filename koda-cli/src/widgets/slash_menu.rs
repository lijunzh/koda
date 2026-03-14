//! Slash command dropdown — thin wrapper around the generic dropdown.
//!
//! Appears when the user types `/` in an empty input. Filters live
//! as the user continues typing.

use super::dropdown::{self, DropdownItem, DropdownState};
use ratatui::text::Line;

/// A slash command item.
#[derive(Clone, Debug)]
pub struct SlashCommand {
    pub command: &'static str,
    pub description: &'static str,
}

impl DropdownItem for SlashCommand {
    fn label(&self) -> &str {
        self.command
    }
    fn description(&self) -> String {
        self.description.to_string()
    }
    fn matches_filter(&self, filter: &str) -> bool {
        self.command.starts_with(filter)
    }
}

/// Create a slash menu dropdown from the command list and current input.
/// Returns `None` if no commands match.
pub fn from_input(
    commands: &'static [(&'static str, &'static str)],
    input: &str,
) -> Option<DropdownState<SlashCommand>> {
    let items: Vec<SlashCommand> = commands
        .iter()
        .map(|(cmd, desc)| SlashCommand {
            command: cmd,
            description: desc,
        })
        .collect();
    let mut dd = DropdownState::new(items, "\u{1f43b} Commands");
    if dd.apply_filter(input) {
        Some(dd)
    } else {
        None
    }
}

/// Build lines for rendering. Delegates to the generic dropdown renderer.
pub fn build_menu_lines(state: &DropdownState<SlashCommand>) -> Vec<Line<'static>> {
    dropdown::build_dropdown_lines(state)
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
        ("/model", "Pick model"),
    ];

    #[test]
    fn from_input_all() {
        let state = from_input(TEST_COMMANDS, "/").unwrap();
        assert_eq!(state.filtered.len(), 7);
    }

    #[test]
    fn from_input_filtered() {
        let state = from_input(TEST_COMMANDS, "/m").unwrap();
        assert_eq!(state.filtered.len(), 1);
        assert_eq!(state.filtered[0].command, "/model");
    }

    #[test]
    fn from_input_no_match() {
        assert!(from_input(TEST_COMMANDS, "/z").is_none());
    }

    #[test]
    fn selected_command() {
        let state = from_input(TEST_COMMANDS, "/").unwrap();
        assert_eq!(state.selected_item().unwrap().command, "/agent");
    }
}
