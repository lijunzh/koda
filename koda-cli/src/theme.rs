//! Semantic color tokens for the TUI.
//!
//! Single source of truth for "what color is this thing" — every renderer
//! consumes this module instead of reaching for [`ratatui::style::Color`]
//! variants directly. Keeps the palette swappable in one place when we
//! eventually ship user-configurable themes (see #945 follow-ups).
//!
//! ## Why a separate module?
//!
//! Before this module existed, the dot-color and content-style tables
//! lived in two files and **disagreed** with each other:
//!
//! | Tool      | Live render (tui_render.rs)  | History replay (history_render.rs) |
//! |-----------|------------------------------|-------------------------------------|
//! | `Bash`    | orange (`tool_call_styles`)  | green (`tool_dot_color`)            |
//! | `Edit`    | amber                        | yellow                              |
//!
//! Sessions resumed from disk rendered the same tool calls in **different
//! colors** than they appeared during the live turn. This module makes
//! [`tool_dot`] the single function both renderers call.
//!
//! ## Layered design (mirrors gemini-cli's `semantic-tokens.ts`)
//!
//! - **Status tokens** (`STATUS_*`) — success/error/warning/info, semantic.
//! - **Content tokens** (`CONTENT_*`) — body text style for tool output by
//!   effect class.
//! - **Tool-call tokens** — [`tool_dot`] for the bullet, plus accent colors
//!   for paths/line numbers/match highlights.
//! - **Code highlight kill-switch** — [`syntax_highlight_enabled`] honors
//!   the `KODA_SYNTAX_HIGHLIGHT=off` env var (steals Claude Code's pattern).

use koda_core::tools::{ToolEffect, classify_tool};
use ratatui::style::{Color, Modifier, Style};

// ── Status tokens ───────────────────────────────────────────
//
// Semantic intent — *not* "red" but "error". Future themes swap the RGB
// without touching call sites.

/// Successful operation, ✓ marks, positive confirmations.
pub const STATUS_SUCCESS: Style = Style::new().fg(Color::Green);

/// Errors, ✗ marks, stderr lines, deletions.
pub const STATUS_ERROR: Style = Style::new().fg(Color::Red);

/// Warnings, ⚠ marks, "would execute" confirmations.
pub const STATUS_WARNING: Style = Style::new().fg(Color::Yellow);

/// Informational messages, neutral notices.
pub const STATUS_INFO: Style = Style::new().fg(Color::Cyan);

// ── Structural / chrome ─────────────────────────────────────

/// Dim text — gutters, separators, decorative chrome that should fade.
pub const DIM: Style = Style::new().fg(Color::DarkGray);

/// Bold modifier (color inherited from context).
pub const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);

/// Structural glyphs: `│ └ ●`. Always dim regardless of tool.
pub const TOOL_PREFIX: Style = Style::new().fg(Color::DarkGray);

// ── Content body styles ─────────────────────────────────────

/// Body of read-only tool output (Read/Grep/List/Glob/…).
///
/// Cool light gray — readable without competing with assistant prose.
pub const CONTENT_READ: Style = Style::new().fg(Color::Rgb(198, 200, 209));

/// Body of mutating tool output (Bash stdout, Write/Edit content).
///
/// Kept dim — users rarely scan this verbatim, so it shouldn't dominate.
pub const CONTENT_WRITE: Style = Style::new().fg(Color::DarkGray);

// ── Tool-call accent palette ──────────────────────────────────
//
// Used for the tool-call dot and the per-tool detail accents below.

const ACCENT_READ: Color = Color::Cyan; // Read/Grep/List/Glob
const ACCENT_WRITE: Color = Color::Rgb(255, 191, 0); // Write/Edit (amber)
const ACCENT_DELETE: Color = Color::Red;
const ACCENT_BASH: Color = Color::Rgb(255, 165, 0); // orange
const ACCENT_NETWORK: Color = Color::Blue;
const ACCENT_AGENT: Color = Color::Magenta;

// Decorative accent styles — not effect-tied. Used for status banners,
// approval previews, and other UI moments that don't fit a tool effect.
pub const ACCENT_AMBER: Style = Style::new().fg(Color::Rgb(255, 191, 0));
pub const ACCENT_MAGENTA: Style = Style::new().fg(Color::Magenta);

/// Color of the `●` glyph that prefixes a tool call header.
///
/// Single source of truth — [`tui_render::tool_call_styles`] and
/// [`history_render::render_tool_call_headers`] both call this.
///
/// Colors are grouped by [`ToolEffect`] when possible, plus a few special
/// cases that don't map cleanly to effects (network, agent dispatch,
/// shell exec — Bash classifies as `LocalMutation` but visually deserves
/// its own slot).
pub fn tool_dot(name: &str) -> Style {
    Style::new().fg(tool_dot_color(name))
}

/// Underlying [`Color`] for [`tool_dot`] — exposed for callers that
/// already use `Style::new().fg(...)` boilerplate.
pub fn tool_dot_color(name: &str) -> Color {
    // Name-based special cases first — these override the effect-based
    // bucket below for visual reasons (e.g. WebFetch is ReadOnly per
    // ToolEffect, but we want network I/O to *look* distinct from local
    // file reads so users notice egress).
    match name {
        "WebFetch" | "WebSearch" => return ACCENT_NETWORK,
        "Task" | "Agent" | "InvokeAgent" => return ACCENT_AGENT,
        "Bash" => return ACCENT_BASH,
        "Delete" => return ACCENT_DELETE,
        _ => {}
    }
    match classify_tool(name) {
        ToolEffect::ReadOnly => ACCENT_READ,
        ToolEffect::RemoteAction => ACCENT_NETWORK,
        ToolEffect::LocalMutation => ACCENT_WRITE,
        ToolEffect::Destructive => ACCENT_DELETE,
    }
}

/// Body content style for a tool's output, picked by effect class.
///
/// Read-only output gets the light off-white ([`CONTENT_READ`]); mutating
/// output stays dim ([`CONTENT_WRITE`]). Stderr always wins over both.
pub fn content_style_for(tool_name: &str, is_stderr: bool) -> Style {
    if is_stderr {
        STATUS_ERROR
    } else if matches!(classify_tool(tool_name), ToolEffect::ReadOnly) {
        CONTENT_READ
    } else {
        CONTENT_WRITE
    }
}

// ── File / path accents (used by Grep, List, Glob renderers) ────────

/// File path in tool output — tinted to stand out from line content.
///
/// The `UNDERLINED` modifier makes the path *visually* read as a link
/// even on terminals that don't render OSC 8 hyperlinks. On supporting
/// terminals (iTerm2, Kitty, Wezterm, Ghostty, VSCode, Alacritty,
/// Windows Terminal, …) the [`crate::hyperlink`] post-render pass turns
/// every cell with this style into a clickable `file://` link. The two
/// effects compound — we never need to ask whether the terminal supports
/// hyperlinks because the underline carries the meaning either way.
pub const PATH: Style = Style::new()
    .fg(Color::Cyan)
    .add_modifier(Modifier::UNDERLINED);

/// Line number in tool output — yellow keeps it scan-able next to the
/// cyan path without competing for the eye.
pub const LINENO: Style = Style::new().fg(Color::Yellow);

/// Highlighted *match* substring inside Grep content — bold + warm
/// background-less accent that survives against [`CONTENT_READ`].
pub const MATCH_HIT: Style = Style::new()
    .fg(Color::Rgb(255, 191, 0))
    .add_modifier(Modifier::BOLD);

/// Directory entries in `List` output — bold default fg.
pub const DIRECTORY: Style = Style::new().add_modifier(Modifier::BOLD);

/// Color a filename by its extension category.
///
/// Used by `List` and `Glob` so the same `.rs` file renders the same color
/// in both surfaces. Returning [`Style`] (not bare [`Color`]) makes future
/// extensions (e.g. italic for tests, dim for generated) straightforward.
pub fn style_for_extension(ext: &str) -> Style {
    match ext {
        // Source code — green.
        "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "go" | "rb" | "java" | "c" | "cpp" | "h"
        | "cs" | "swift" | "kt" | "scala" | "clj" | "ex" | "exs" | "ml" | "hs" | "lua" | "php"
        | "pl" | "sh" | "bash" | "zsh" => Style::new().fg(Color::Green),
        // Config / data — yellow.
        "toml" | "yaml" | "yml" | "json" | "json5" | "xml" | "ini" | "cfg" | "conf" | "env"
        | "properties" => Style::new().fg(Color::Yellow),
        // Docs — neutral.
        "md" | "txt" | "rst" | "adoc" | "tex" => Style::new().fg(Color::White),
        // Lock / generated — dim.
        "lock" | "sum" | "min" => Style::new().fg(Color::DarkGray),
        // Unknown — default fg.
        _ => Style::new().fg(Color::Reset),
    }
}

// ── Warm palette (koda's bear identity, used by banners) ────────────

pub const WARM_TITLE: Style = Style::new()
    .fg(Color::Rgb(229, 192, 123))
    .add_modifier(Modifier::BOLD);
pub const WARM_ACCENT: Style = Style::new().fg(Color::Rgb(209, 154, 102));
pub const WARM_MUTED: Style = Style::new().fg(Color::Rgb(124, 111, 100));
pub const WARM_INFO: Style = Style::new().fg(Color::Rgb(198, 165, 106));

// ── Syntax highlighting kill switch ─────────────────────────────────

/// Returns `false` when the user has set `KODA_SYNTAX_HIGHLIGHT=off`.
///
/// Steals the pattern from Claude Code's `CLAUDE_CODE_SYNTAX_HIGHLIGHT`.
/// Cheap insurance against terminals where ANSI-RGB output looks awful
/// (e.g. monochrome themes) — users get a one-flag escape hatch instead
/// of having to file a bug.
///
/// Memoized after the first call — the env var is read once via a
/// `OnceLock` so subsequent invocations are a load+branch with zero
/// allocation. Process restarts pick up env-var changes.
pub fn syntax_highlight_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("KODA_SYNTAX_HIGHLIGHT")
            .map(|v| {
                !matches!(
                    v.to_ascii_lowercase().as_str(),
                    "off" | "0" | "false" | "no"
                )
            })
            .unwrap_or(true)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_dot_groups_read_only_tools() {
        // Cyan family for read-only tools — single source of truth that
        // history_render and tui_render now both consume.
        assert_eq!(tool_dot_color("Read"), ACCENT_READ);
        assert_eq!(tool_dot_color("Grep"), ACCENT_READ);
        assert_eq!(tool_dot_color("List"), ACCENT_READ);
        assert_eq!(tool_dot_color("Glob"), ACCENT_READ);
    }

    #[test]
    fn tool_dot_distinguishes_destructive_from_mutating() {
        // Delete needs to *pop* — red, not amber.
        assert_eq!(tool_dot_color("Delete"), ACCENT_DELETE);
        assert_eq!(tool_dot_color("Write"), ACCENT_WRITE);
        assert_eq!(tool_dot_color("Edit"), ACCENT_WRITE);
    }

    #[test]
    fn tool_dot_network_distinct_from_read() {
        // External I/O gets its own color so users notice when the
        // model is hitting the wire.
        assert_eq!(tool_dot_color("WebFetch"), ACCENT_NETWORK);
        assert_eq!(tool_dot_color("WebSearch"), ACCENT_NETWORK);
        assert_ne!(tool_dot_color("WebFetch"), ACCENT_READ);
    }

    #[test]
    fn content_style_uses_read_for_readonly_tools() {
        // The fix from #804 — read-only output is legible, not dim.
        assert_eq!(content_style_for("Read", false), CONTENT_READ);
        assert_eq!(content_style_for("Grep", false), CONTENT_READ);
    }

    #[test]
    fn content_style_uses_write_for_mutating_tools() {
        assert_eq!(content_style_for("Bash", false), CONTENT_WRITE);
        assert_eq!(content_style_for("Write", false), CONTENT_WRITE);
    }

    #[test]
    fn content_style_stderr_always_wins() {
        // Even read-only stderr is an error — color reflects severity,
        // not tool effect.
        assert_eq!(content_style_for("Read", true), STATUS_ERROR);
        assert_eq!(content_style_for("Bash", true), STATUS_ERROR);
    }

    #[test]
    fn extension_style_buckets_source_files_consistently() {
        // .rs and .py should pick the same bucket so a polyglot project
        // doesn't look like a christmas tree.
        let rs = style_for_extension("rs");
        let py = style_for_extension("py");
        assert_eq!(rs.fg, py.fg);
    }

    #[test]
    fn extension_style_distinguishes_categories() {
        // Source ≠ config ≠ docs ≠ lockfile.
        let src = style_for_extension("rs").fg.unwrap();
        let cfg = style_for_extension("toml").fg.unwrap();
        let doc = style_for_extension("md").fg.unwrap();
        let lock = style_for_extension("lock").fg.unwrap();
        // All four should be different — if two collide we've lost
        // information.
        let colors = [src, cfg, doc, lock];
        for i in 0..colors.len() {
            for j in (i + 1)..colors.len() {
                assert_ne!(colors[i], colors[j], "extension buckets collide");
            }
        }
    }

    #[test]
    fn syntax_kill_switch_default_is_on() {
        // We can't safely mutate env in a unit test (other tests would
        // race), so just confirm the default path works. Integration
        // coverage for the off path lives in highlight.rs tests.
        // Note: this asserts the *cached* value, so if some other test
        // poisoned the env this would catch it.
        if std::env::var("KODA_SYNTAX_HIGHLIGHT").is_err() {
            assert!(syntax_highlight_enabled());
        }
    }
}
