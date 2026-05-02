//! Slash-command popup: data, matching, and overlay rendering.
//!
//! Closes the loop opened by epic #1116 (composer port). Consolidates
//! everything that the user sees when they type `/` into one module:
//!
//! 1. **`SLASH_COMMANDS`** — the canonical (command, description, arg-hint)
//!    table. Single source of truth, formerly in `crate::completer`.
//! 2. **`SlashCommand`** — the dropdown item type. Implements
//!    [`DropdownItem`](crate::widgets::dropdown::DropdownItem) so the
//!    generic dropdown widget can render and filter it.
//! 3. **`from_input`** — builds a [`DropdownState<SlashCommand>`] from the
//!    current composer input. Returns `None` if no commands match.
//! 4. **`build_menu_lines`** — renders a populated dropdown into the
//!    `Line<'static>` strip drawn above the textarea.
//! 5. **`next_match`** — pure cycling helper used by Tab-completion in
//!    [`crate::completer::InputCompleter`]. Stateful filter + idx live
//!    on the completer; this fn just answers "given filter `trimmed`,
//!    what are the candidate completions?".
//!
//! ## History
//!
//! - `SLASH_COMMANDS` lived in `completer.rs` since the original Tab-
//!   completion port.
//! - `SlashCommand` + `from_input` + `build_menu_lines` lived in
//!   `widgets/slash_menu.rs` since the slash auto-dropdown was added.
//! - Splitting them across two files made adding a new slash command
//!   a two-file edit and forced anyone debugging slash UX to grep
//!   across `completer/`, `widgets/`, and `tui_context/`. Now it's
//!   one module.
//!
//! See #1187 for the refactor RFC.

use crate::widgets::dropdown::{self, DropdownItem, DropdownState};
use ratatui::text::Line;

/// All known slash commands with `(command, description, arg_hint)`.
///
/// `arg_hint` is `Some("<placeholder>")` for commands that take an
/// argument, `None` for self-contained commands and picker-openers.
/// Single source of truth — used by both Tab-completion
/// ([`crate::completer::InputCompleter`]) and the auto-dropdown
/// ([`from_input`]).
pub const SLASH_COMMANDS: &[(&str, &str, Option<&str>)] = &[
    ("/agent", "Switch to a sub-agent", Some("<name>")),
    (
        "/agents",
        "List running background tasks (sub-agents + processes)",
        None,
    ),
    (
        "/cancel",
        "Cancel a background task by id (agent:N or process:N from /agents)",
        Some("<id>"),
    ),
    (
        "/compact",
        "Summarize conversation to reclaim context",
        None,
    ),
    (
        "/copy",
        "Copy last response to clipboard (/copy 2 for 2nd-last)",
        Some("[n]"),
    ),
    (
        "/debug-bundle",
        "Write a self-contained debug .zip to ~/.config/koda/debug-bundles/",
        None,
    ),
    ("/diff", "Show git diff (review, commit)", None),
    ("/exit", "Quit the session", None),
    ("/expand", "Show full output of last tool call", None),
    ("/help", "Show commands and shortcuts", None),
    ("/key", "Manage API keys", None),
    (
        "/mcp",
        "Manage MCP servers (add, remove, list)",
        Some("[add|remove|list]"),
    ),
    ("/memory", "View/save project & global memory", None),
    ("/model", "Pick a model (aliases + local)", None),
    ("/provider", "Browse all models from a provider", None),
    (
        "/purge",
        "Delete archived history (e.g. /purge 90d)",
        Some("<days>"),
    ),
    ("/sessions", "List/resume/delete sessions", None),
    ("/skills", "List available skills (search with query)", None),
    ("/undo", "Undo last turn's file changes", None),
    ("/verbose", "Toggle full tool output", None),
    ("/vim", "Toggle vim-mode editing in the input", None),
];

// ── Dropdown overlay (used by the auto-popup) ──────────────────────────────

/// A slash command, in the shape the dropdown widget consumes.
#[derive(Clone, Debug)]
pub struct SlashCommand {
    pub command: &'static str,
    pub description: &'static str,
    /// Argument placeholder shown when the command needs user input
    /// (e.g. `Some("<name>")`). `None` for self-contained commands and
    /// picker-openers.
    pub arg_hint: Option<&'static str>,
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

/// Build a slash menu dropdown from the command table and the current
/// composer input. Returns `None` if no commands match the filter.
///
/// Caller passes `commands` explicitly (rather than reading
/// [`SLASH_COMMANDS`] directly) so tests can supply a deterministic
/// fixture without bloating the assertions with the real ~22-command
/// table.
pub fn from_input(
    commands: &'static [(&'static str, &'static str, Option<&'static str>)],
    input: &str,
) -> Option<DropdownState<SlashCommand>> {
    let items: Vec<SlashCommand> = commands
        .iter()
        .map(|(cmd, desc, arg_hint)| SlashCommand {
            command: cmd,
            description: desc,
            arg_hint: *arg_hint,
        })
        .collect();
    let mut dd = DropdownState::new(items, "\u{1f43b} Commands");
    if dd.apply_filter(input) {
        Some(dd)
    } else {
        None
    }
}

/// Render a populated dropdown into the lines drawn above the textarea.
/// Delegates to the generic dropdown renderer — this exists as a thin
/// alias so callers don't need to know the dropdown internals.
pub fn build_menu_lines(state: &DropdownState<SlashCommand>) -> Vec<Line<'static>> {
    dropdown::build_dropdown_lines(state)
}

// ── Tab-completion matching (used by InputCompleter) ───────────────────────

/// Compute the slash-command completion candidates for a given partial
/// input. Pure function — no state, no I/O. Returned strings are the
/// full replacement text (e.g. `"/diff"`), suitable for direct insertion.
///
/// Matching rules:
/// - Prefix match (`cmd.starts_with(trimmed)`)
/// - Excludes the input itself if it's already a complete command (no
///   point in "completing" `/diff` to `/diff`)
///
/// Cycling state (idx, last-token-seen) is the caller's problem — see
/// [`crate::completer::InputCompleter::complete`] for how this slots
/// into Tab-cycling.
pub fn matches_for(trimmed: &str) -> Vec<String> {
    SLASH_COMMANDS
        .iter()
        .filter(|(cmd, _, _)| cmd.starts_with(trimmed) && *cmd != trimmed)
        .map(|(cmd, _, _)| cmd.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_COMMANDS: &[(&str, &str, Option<&str>)] = &[
        ("/agent", "Agents", Some("<name>")),
        ("/compact", "Compact", None),
        ("/diff", "Diff", None),
        ("/exit", "Quit", None),
        ("/expand", "Expand", None),
        ("/model", "Pick model", None),
    ];

    // ── Dropdown construction ──

    #[test]
    fn from_input_all() {
        let state = from_input(TEST_COMMANDS, "/").unwrap();
        assert_eq!(state.filtered.len(), 6);
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

    // ── Tab-completion matching ──
    //
    // These use the real SLASH_COMMANDS table (not TEST_COMMANDS) so the
    // assertions double as a smoke test that the production table still
    // contains the commands users expect.

    #[test]
    fn matches_for_unique_prefix_returns_one() {
        let m = matches_for("/dif");
        assert_eq!(m, vec!["/diff".to_string()]);
    }

    #[test]
    fn matches_for_complete_command_returns_none() {
        // "/diff" is already complete — completing it to itself is useless.
        let m = matches_for("/diff");
        assert!(
            !m.iter().any(|s| s == "/diff"),
            "self-completion must be filtered out"
        );
    }

    #[test]
    fn matches_for_no_match_returns_empty() {
        assert!(matches_for("/zzzznope").is_empty());
    }

    #[test]
    fn matches_for_ambiguous_prefix_returns_all_options() {
        // "/a" should match /agent, /agents, but nothing without the prefix.
        let m = matches_for("/a");
        assert!(m.iter().any(|s| s == "/agent"));
        assert!(m.iter().any(|s| s == "/agents"));
        assert!(
            m.iter().all(|s| s.starts_with("/a")),
            "matches must all share the queried prefix"
        );
    }
}
