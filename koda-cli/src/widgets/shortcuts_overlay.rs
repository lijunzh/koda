//! Multi-line keybinding reference overlay, opened by pressing `?` while
//! the composer is empty and no menu is active.
//!
//! # Why this exists (#1194 + #1195)
//!
//! Pre-#1194, the persistent footer row crammed six keybindings on one
//! line (`enter send · shift+enter newline · tab complete · esc menu ·
//! ↑ history · ctrl+c quit`) AND the startup banner dumped six MORE on
//! a parallel row (`Shift+Tab mode · Ctrl+C cancel · PgUp/PgDn scroll ·
//! Ctrl+D quit`). Twelve keybindings across two unrelated rows, one of
//! them (`Ctrl+C`) appearing twice with conflicting verbs.
//!
//! Field study (`#1163`-style survey) confirmed that **all three reference
//! systems** — codex, claude-code, gemini-cli — converged on the same
//! pattern: a minimal default footer (`? for shortcuts` or nothing), an
//! on-demand expanded overlay with the full set, and zero keybindings in
//! the welcome banner. This module is koda's port of that pattern.
//!
//! Specifically modelled on codex's `FooterMode::ShortcutOverlay`
//! (`bottom_pane/footer.rs::shortcut_overlay_lines`). We match its
//! discipline (one source of truth for keybinding labels, two-column
//! layout when terminal is wide, single-column when narrow).
//!
//! # Layout
//!
//! ```text
//!   Shortcuts                                                See /help
//!
//!   enter         send message      shift+tab    cycle trust mode
//!   shift+enter   new line          ctrl+l       jump to bottom
//!   tab           complete          ctrl+r       reverse history search
//!   esc           clear / cancel    pgup/pgdn    scroll history
//!   ↑/↓           history           ctrl+c       cancel / quit
//!   /             commands          ctrl+d       quit (when empty)
//!   @             file mention      ?            toggle this overlay
//! ```
//!
//! Wide terminals get the two-column layout; narrow terminals fall back
//! to a single column.
//!
//! # Why the menu_area slot?
//!
//! koda's existing `MenuContent` enum already owns the overlay slot
//! between the status bar and the bottom of the screen. Adding a
//! `MenuContent::ShortcutsOverlay` variant rides the existing render
//! plumbing — `render_menu` dispatches by variant, `handle_menu_key`
//! intercepts dismissal — so we get the popup behaviour for free
//! without inventing a new render layer.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// One labelled keybinding row in the overlay grid.
struct ShortcutItem {
    /// Key chord text — rendered in the accent colour (matches the
    /// `KeyBinding → Span` styling used by the composer key-hint footer).
    key: &'static str,
    /// Plain action verb — rendered dim.
    verb: &'static str,
}

/// The full keybinding set, in display order. **One source of truth** —
/// any future keybind addition lives here, and both the on-screen overlay
/// and the `/help` documentation should read from this list (next time
/// `/help` is touched, fold it in).
const SHORTCUTS: &[ShortcutItem] = &[
    ShortcutItem {
        key: "enter",
        verb: "send message",
    },
    ShortcutItem {
        key: "shift+enter",
        verb: "new line",
    },
    ShortcutItem {
        key: "tab",
        verb: "complete",
    },
    ShortcutItem {
        key: "esc",
        verb: "clear / cancel",
    },
    ShortcutItem {
        key: "\u{2191}/\u{2193}",
        verb: "history",
    },
    ShortcutItem {
        key: "/",
        verb: "commands",
    },
    ShortcutItem {
        key: "@",
        verb: "file mention",
    },
    ShortcutItem {
        key: "shift+tab",
        verb: "cycle trust mode",
    },
    ShortcutItem {
        key: "ctrl+l",
        verb: "jump to bottom",
    },
    ShortcutItem {
        key: "ctrl+r",
        verb: "reverse history search",
    },
    ShortcutItem {
        key: "pgup/pgdn",
        verb: "scroll history",
    },
    ShortcutItem {
        key: "ctrl+c",
        verb: "cancel / quit",
    },
    ShortcutItem {
        key: "ctrl+d",
        verb: "quit (when empty)",
    },
    ShortcutItem {
        key: "?",
        verb: "toggle this overlay",
    },
];

/// Width threshold for switching to single-column layout. 80 cols is the
/// classic narrow-terminal floor; below that the two-column grid would
/// truncate keys mid-word.
const NARROW_TERMINAL_WIDTH: u16 = 80;

/// Vertical padding lines (header + blank separator + bottom blank).
/// Kept as a constant so `overlay_height` and `build_overlay_lines` agree.
const CHROME_LINES: u16 = 3;

/// How many lines the overlay needs at the given width. Caller uses this
/// to size the `menu_area` `Constraint::Length` so the overlay doesn't
/// clip the status bar above it.
pub fn overlay_height(width: u16) -> u16 {
    let item_lines = if width < NARROW_TERMINAL_WIDTH {
        SHORTCUTS.len() as u16
    } else {
        // Two columns: ceil(N / 2) rows.
        SHORTCUTS.len().div_ceil(2) as u16
    };
    item_lines + CHROME_LINES
}

/// Build the styled lines for the overlay. Caller renders these into a
/// `Paragraph` inside the `menu_area` slot.
pub fn build_overlay_lines(width: u16) -> Vec<Line<'static>> {
    let header_style = Style::default()
        .fg(Color::Rgb(198, 165, 106))
        .add_modifier(Modifier::BOLD);
    let hint_style = Style::default().fg(Color::Rgb(124, 111, 100));

    let mut lines = Vec::with_capacity(overlay_height(width) as usize);
    lines.push(Line::from(vec![
        Span::styled("  Shortcuts", header_style),
        Span::styled("    press ", hint_style),
        Span::styled(
            "?",
            Style::default()
                .fg(Color::Rgb(198, 165, 106))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" or ", hint_style),
        Span::styled(
            "esc",
            Style::default()
                .fg(Color::Rgb(198, 165, 106))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" to close", hint_style),
    ]));
    lines.push(Line::from(""));

    if width < NARROW_TERMINAL_WIDTH {
        // Single-column fallback. Keep the same key-column width so verbs
        // line up vertically — a ragged left edge would look like a bug.
        let key_col_width = max_key_width(SHORTCUTS);
        for item in SHORTCUTS {
            lines.push(format_row(item, key_col_width));
        }
    } else {
        // Two-column grid. Distribute items column-major so the visual
        // reading order is top-to-bottom in column 1, then top-to-bottom
        // in column 2 — matches how a sighted user scans a help table
        // and matches codex's `shortcut_overlay_lines` layout.
        let rows = SHORTCUTS.len().div_ceil(2);
        let key_col_width = max_key_width(SHORTCUTS);
        // Left column verb width → fixed at the longest verb in the
        // first half so the right column starts at a stable x-offset
        // regardless of which item happens to be tallest.
        let left_verb_width = SHORTCUTS
            .iter()
            .take(rows)
            .map(|i| i.verb.chars().count())
            .max()
            .unwrap_or(0);

        for (row, left) in SHORTCUTS.iter().take(rows).enumerate() {
            let right = SHORTCUTS.get(row + rows);
            let mut spans = vec![
                Span::raw("  "),
                Span::styled(pad_right(left.key, key_col_width), accent_style()),
                Span::raw("  "),
                Span::styled(pad_right(left.verb, left_verb_width), verb_style()),
            ];
            if let Some(right) = right {
                spans.extend([
                    Span::raw("    "),
                    Span::styled(pad_right(right.key, key_col_width), accent_style()),
                    Span::raw("  "),
                    Span::styled(right.verb.to_string(), verb_style()),
                ]);
            }
            lines.push(Line::from(spans));
        }
    }

    lines.push(Line::from(""));
    lines
}

/// Right-pad a string to `width` columns with spaces. Returns owned
/// `String` so callers can push it into a `Span::styled`.
fn pad_right(s: &str, width: usize) -> String {
    let visible = s.chars().count();
    if visible >= width {
        s.to_string()
    } else {
        let mut out = String::with_capacity(s.len() + (width - visible));
        out.push_str(s);
        for _ in 0..(width - visible) {
            out.push(' ');
        }
        out
    }
}

fn max_key_width(items: &[ShortcutItem]) -> usize {
    items
        .iter()
        .map(|i| i.key.chars().count())
        .max()
        .unwrap_or(0)
}

fn accent_style() -> Style {
    Style::default().fg(Color::Rgb(198, 165, 106))
}

fn verb_style() -> Style {
    Style::default().fg(Color::Rgb(140, 140, 140))
}

fn format_row(item: &ShortcutItem, key_col_width: usize) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(pad_right(item.key, key_col_width), accent_style()),
        Span::raw("  "),
        Span::styled(item.verb.to_string(), verb_style()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wide terminal: header + blank + ceil(N/2) items + blank.
    #[test]
    fn overlay_height_two_column_for_wide_terminal() {
        let h = overlay_height(120);
        let expected = SHORTCUTS.len().div_ceil(2) as u16 + CHROME_LINES;
        assert_eq!(h, expected, "wide layout uses two columns");
    }

    /// Narrow terminal: one row per item.
    #[test]
    fn overlay_height_single_column_for_narrow_terminal() {
        let h = overlay_height(60);
        let expected = SHORTCUTS.len() as u16 + CHROME_LINES;
        assert_eq!(h, expected, "narrow layout falls back to one column");
    }

    /// Boundary: at exactly 80 cols we use the two-column layout (the
    /// `<` makes 80 the smallest two-column width).
    #[test]
    fn overlay_height_uses_two_columns_at_threshold() {
        assert_eq!(overlay_height(80), overlay_height(120));
    }

    /// Every shortcut's verb appears in the rendered output (no items
    /// silently dropped during column distribution).
    #[test]
    fn build_overlay_lines_contains_every_verb() {
        let lines = build_overlay_lines(120);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        for item in SHORTCUTS {
            assert!(
                text.contains(item.verb),
                "verb {:?} missing from rendered overlay (text={text})",
                item.verb,
            );
        }
    }

    /// Header includes the `?` and `esc` dismiss hints — the user must
    /// be able to discover how to close the overlay from the overlay
    /// itself (no out-of-band documentation lookup).
    #[test]
    fn header_advertises_dismiss_keys() {
        let lines = build_overlay_lines(120);
        let header: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(header.contains('?'), "header must mention `?`: {header}");
        assert!(
            header.contains("esc"),
            "header must mention `esc`: {header}"
        );
        assert!(
            header.to_lowercase().contains("close"),
            "header must say how to dismiss: {header}",
        );
    }

    /// Narrow layout returns exactly the items + chrome (no two-column
    /// padding sneaking in).
    #[test]
    fn narrow_layout_lines_match_height() {
        let lines = build_overlay_lines(60);
        assert_eq!(lines.len() as u16, overlay_height(60));
    }

    /// Wide layout returns exactly the rows + chrome.
    #[test]
    fn wide_layout_lines_match_height() {
        let lines = build_overlay_lines(120);
        assert_eq!(lines.len() as u16, overlay_height(120));
    }

    /// pad_right is idempotent for already-wide strings (no truncation).
    #[test]
    fn pad_right_does_not_truncate() {
        assert_eq!(pad_right("abcdef", 3), "abcdef");
    }

    /// pad_right adds trailing spaces to reach the target width.
    #[test]
    fn pad_right_pads_to_width() {
        let padded = pad_right("ab", 5);
        assert_eq!(padded, "ab   ");
        assert_eq!(padded.chars().count(), 5);
    }

    /// Visual smoke check — dumps the rendered overlay at three widths
    /// to stderr. Not asserting; eyeball with `cargo test -- --nocapture`.
    /// Kept as a permanent test (rather than ad-hoc binary) so future
    /// changes to the layout are easy to verify visually without writing
    /// new scaffolding each time.
    #[test]
    fn visual_dump() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::widgets::{Paragraph, Widget};
        for width in [120u16, 80, 60] {
            let h = overlay_height(width);
            let area = Rect::new(0, 0, width, h);
            let mut buf = Buffer::empty(area);
            Paragraph::new(build_overlay_lines(width)).render(area, &mut buf);
            eprintln!("\n=== width={width}, height={h} ===");
            for y in 0..area.height {
                let row: String = (0..area.width)
                    .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                    .collect();
                eprintln!("|{}|", row.trim_end());
            }
        }
    }
}
