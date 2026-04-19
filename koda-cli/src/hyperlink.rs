//! OSC 8 hyperlink post-render pass.
//!
//! Walks the rendered ratatui [`Buffer`] after `Paragraph` has finished
//! laying it out, finds runs of cells styled with [`crate::theme::PATH`]
//! (cyan + underlined \u2014 our compile-time marker for "this is a clickable
//! file path or URL"), and rewrites each cell's symbol so the bytes
//! crossterm sends to the terminal include the OSC 8 escape sequence:
//!
//! ```text
//! \x1B]8;;<URI>\x07<visible char>\x1B]8;;\x07
//! ```
//!
//! ## Why per-cell wrapping
//!
//! ratatui's crossterm backend emits a `MoveTo` between every cell when
//! flushing a frame, which means a single OSC 8 sequence spanning many
//! cells gets shredded by cursor moves. The fix \u2014 borrowed from
//! `codex-rs/tui/src/onboarding/auth.rs::mark_url_hyperlink` \u2014 is to put
//! the *full* `OPEN \u2026 BEL <char> CLOSE BEL` pair inside *each* cell's
//! symbol. Terminals dedupe consecutive identical OSC 8 IDs, so adjacent
//! cells render as one continuous link.
//!
//! ## Why no terminal-support probe
//!
//! The OSC 8 spec requires non-supporting terminals to silently swallow
//! the sequence. They never display as garbage. A capability probe would
//! be classic "scaffold around weakness" (see `DESIGN.md` P3) \u2014 we don't
//! need it.
//!
//! ## Why no env-var kill switch
//!
//! See `DESIGN.md` P1: runtime flags that alter behavior within a
//! scenario are not allowed. If hyperlinks ever break a terminal in
//! practice, the fix is to detect *that terminal* (compile-time list)
//! or remove the modifier from [`crate::theme::PATH`], not to ship a
//! knob.

use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use std::path::Path;

/// Post-process `area` of `buf`: turn runs of [`theme::PATH`]-styled
/// cells into clickable OSC 8 hyperlinks.
///
/// `project_root` is used to resolve relative file paths to absolute
/// `file://` URIs. Absolute paths and `http(s)://` URLs pass through.
///
/// This is a pure mutation of cell symbols \u2014 zero impact on layout,
/// width, or styling. Safe to call unconditionally on every frame.
pub fn link_paths_in_buffer(buf: &mut Buffer, area: Rect, project_root: &Path) {
    for y in area.top()..area.bottom() {
        link_paths_in_row(buf, area, y, project_root);
    }
}

/// Walk one row left-to-right, group contiguous PATH-styled cells into
/// runs, and wrap each run with an OSC 8 sequence keyed off the run's
/// concatenated text.
fn link_paths_in_row(buf: &mut Buffer, area: Rect, y: u16, project_root: &Path) {
    let mut x = area.left();
    while x < area.right() {
        if !is_path_cell(&buf[(x, y)]) {
            x += 1;
            continue;
        }
        // Found the start of a path run \u2014 scan to its end.
        let start = x;
        let mut text = String::new();
        while x < area.right() && is_path_cell(&buf[(x, y)]) {
            text.push_str(buf[(x, y)].symbol());
            x += 1;
        }
        let end = x;

        let uri = match resolve_uri(text.trim(), project_root) {
            Some(u) => u,
            None => continue,
        };
        wrap_run_with_osc8(buf, y, start, end, &uri);
    }
}

/// Cell predicate: matches `theme::PATH` exactly (cyan fg + underlined).
///
/// We deliberately match the *style* not the modifier alone so unrelated
/// underlined text (e.g. markdown emphasis if it ever lands underlined)
/// doesn't get linkified.
fn is_path_cell(cell: &Cell) -> bool {
    cell.fg == Color::Cyan && cell.modifier.contains(Modifier::UNDERLINED)
}

/// Resolve the visible text of a path run to a URI suitable for OSC 8.
///
/// - `http://` and `https://` pass through unchanged.
/// - Absolute filesystem paths become `file://<path>`.
/// - Relative paths are joined to `project_root`.
/// - Empty / whitespace-only text yields `None` (skip the run).
fn resolve_uri(text: &str, project_root: &Path) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    if text.starts_with("http://") || text.starts_with("https://") {
        return Some(escape_url(text));
    }
    let abs: std::path::PathBuf = if text.starts_with('/') {
        Path::new(text).to_path_buf()
    } else {
        project_root.join(text)
    };
    let abs_str = abs.to_string_lossy();
    let mut uri = String::with_capacity(abs_str.len() + 8);
    uri.push_str("file://");
    if !abs_str.starts_with('/') {
        uri.push('/');
    }
    // Light percent-encoding for chars that confuse OSC 8 parsers /
    // markdown link extractors. Mirrors `transcript::file_uri`.
    for ch in abs_str.chars() {
        match ch {
            ' ' => uri.push_str("%20"),
            '(' => uri.push_str("%28"),
            ')' => uri.push_str("%29"),
            '[' => uri.push_str("%5B"),
            ']' => uri.push_str("%5D"),
            _ => uri.push(ch),
        }
    }
    Some(escape_url(&uri))
}

/// Strip ESC and BEL from a URI to prevent escape-sequence injection.
///
/// A malicious / malformed upstream value containing `\x1B` or `\x07`
/// could otherwise break out of the OSC 8 payload and inject arbitrary
/// terminal control sequences. Same defense codex applies.
fn escape_url(raw: &str) -> String {
    raw.chars()
        .filter(|&c| c != '\x1B' && c != '\x07')
        .collect()
}

/// Replace the symbol of every cell in `[start, end)` of row `y` with
/// `OSC8 uri BEL <sym> OSC8 BEL` so each cell is a self-contained link.
fn wrap_run_with_osc8(buf: &mut Buffer, y: u16, start: u16, end: u16, uri: &str) {
    for x in start..end {
        let cell = &mut buf[(x, y)];
        let sym = cell.symbol().to_string();
        if sym.is_empty() {
            continue;
        }
        cell.set_symbol(&format!("\x1B]8;;{uri}\x07{sym}\x1B]8;;\x07"));
    }
}

/// Strip OSC 8 sequences from a string \u2014 useful for snapshot tests
/// that want to assert visible content only.
#[cfg(test)]
pub fn strip_osc8(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1B' && chars.peek() == Some(&']') {
            // Consume ']' and everything up to and including BEL.
            chars.next();
            for c in chars.by_ref() {
                if c == '\x07' {
                    break;
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme;
    use ratatui::style::Style;

    /// Fill a single-row buffer with `text`, applying `style` to each cell.
    fn buffer_with_styled_text(text: &str, style: Style) -> (Buffer, Rect) {
        let area = Rect::new(0, 0, text.chars().count() as u16, 1);
        let mut buf = Buffer::empty(area);
        for (i, ch) in text.chars().enumerate() {
            let x = i as u16;
            buf[(x, 0)].set_symbol(&ch.to_string());
            buf[(x, 0)].set_style(style);
        }
        (buf, area)
    }

    fn row_symbols(buf: &Buffer, area: Rect) -> String {
        let mut out = String::new();
        for x in area.left()..area.right() {
            out.push_str(buf[(x, 0)].symbol());
        }
        out
    }

    #[test]
    fn relative_path_resolves_under_project_root() {
        let (mut buf, area) = buffer_with_styled_text("src/main.rs", theme::PATH);
        link_paths_in_buffer(&mut buf, area, Path::new("/home/me/proj"));
        let raw = row_symbols(&buf, area);
        assert!(
            raw.contains("\x1B]8;;file:///home/me/proj/src/main.rs\x07"),
            "expected OSC 8 with absolute file URI, got: {raw:?}"
        );
        // Visible text is preserved.
        assert_eq!(strip_osc8(&raw), "src/main.rs");
    }

    #[test]
    fn absolute_path_passes_through_root() {
        let (mut buf, area) = buffer_with_styled_text("/etc/hosts", theme::PATH);
        link_paths_in_buffer(&mut buf, area, Path::new("/proj"));
        let raw = row_symbols(&buf, area);
        assert!(raw.contains("\x1B]8;;file:///etc/hosts\x07"), "{raw:?}");
    }

    #[test]
    fn http_url_passes_through_unchanged() {
        let (mut buf, area) = buffer_with_styled_text("https://example.com/x", theme::PATH);
        link_paths_in_buffer(&mut buf, area, Path::new("/proj"));
        let raw = row_symbols(&buf, area);
        assert!(
            raw.contains("\x1B]8;;https://example.com/x\x07"),
            "URL should be linked verbatim, got: {raw:?}"
        );
    }

    #[test]
    fn unstyled_cells_are_left_alone() {
        // Plain text \u2014 no PATH style, no linking.
        let style = Style::default();
        let (mut buf, area) = buffer_with_styled_text("nothing here", style);
        link_paths_in_buffer(&mut buf, area, Path::new("/proj"));
        let raw = row_symbols(&buf, area);
        assert_eq!(raw, "nothing here");
        assert!(!raw.contains('\x1B'));
    }

    #[test]
    fn cyan_without_underline_is_not_linked() {
        // Just cyan (no underline) \u2014 not a PATH cell. Don't link.
        let style = Style::new().fg(Color::Cyan);
        let (mut buf, area) = buffer_with_styled_text("not a path", style);
        link_paths_in_buffer(&mut buf, area, Path::new("/proj"));
        let raw = row_symbols(&buf, area);
        assert!(!raw.contains('\x1B'), "got: {raw:?}");
    }

    #[test]
    fn underline_without_cyan_is_not_linked() {
        // Markdown emphasis-style underline shouldn't accidentally linkify.
        let style = Style::new().add_modifier(Modifier::UNDERLINED);
        let (mut buf, area) = buffer_with_styled_text("emphasized", style);
        link_paths_in_buffer(&mut buf, area, Path::new("/proj"));
        let raw = row_symbols(&buf, area);
        assert!(!raw.contains('\x1B'), "got: {raw:?}");
    }

    #[test]
    fn escape_injection_in_path_text_is_neutered() {
        // A path containing ESC (e.g. attacker-controlled filename via
        // a sub-agent transcript) must not be able to break out of the
        // OSC 8 payload.
        let nasty = "a\x1Bb"; // 3 chars in the row
        let (mut buf, area) = buffer_with_styled_text(nasty, theme::PATH);
        link_paths_in_buffer(&mut buf, area, Path::new("/proj"));
        let raw = row_symbols(&buf, area);
        // ESC must not appear inside the OSC 8 payload portion. Strip
        // visible chars and make sure no second `\x1B]` appears beyond
        // the initial OPEN (and the matching CLOSE in each cell).
        assert!(
            !raw.contains("\x1Bb"),
            "raw ESC byte must not survive in the URI payload: {raw:?}"
        );
    }

    #[test]
    fn empty_run_is_skipped() {
        // All-whitespace styled run \u2014 nothing to link.
        let (mut buf, area) = buffer_with_styled_text("   ", theme::PATH);
        link_paths_in_buffer(&mut buf, area, Path::new("/proj"));
        let raw = row_symbols(&buf, area);
        assert!(!raw.contains('\x1B'), "got: {raw:?}");
    }

    #[test]
    fn two_separate_paths_in_one_row_are_linked_independently() {
        // Layout: "a.rs  b.rs" \u2014 two PATH runs separated by plain spaces.
        let area = Rect::new(0, 0, 10, 1);
        let mut buf = Buffer::empty(area);
        let path_style = theme::PATH;
        let plain = Style::default();
        for (i, ch) in "a.rs".chars().enumerate() {
            buf[(i as u16, 0)].set_symbol(&ch.to_string());
            buf[(i as u16, 0)].set_style(path_style);
        }
        for (i, ch) in "  ".chars().enumerate() {
            let x = (4 + i) as u16;
            buf[(x, 0)].set_symbol(&ch.to_string());
            buf[(x, 0)].set_style(plain);
        }
        for (i, ch) in "b.rs".chars().enumerate() {
            let x = (6 + i) as u16;
            buf[(x, 0)].set_symbol(&ch.to_string());
            buf[(x, 0)].set_style(path_style);
        }
        link_paths_in_buffer(&mut buf, area, Path::new("/p"));
        let raw = row_symbols(&buf, area);
        assert!(raw.contains("\x1B]8;;file:///p/a.rs\x07"), "{raw:?}");
        assert!(raw.contains("\x1B]8;;file:///p/b.rs\x07"), "{raw:?}");
        assert_eq!(strip_osc8(&raw), "a.rs  b.rs");
    }

    #[test]
    fn space_in_path_is_percent_encoded() {
        let (mut buf, area) = buffer_with_styled_text("My File.rs", theme::PATH);
        link_paths_in_buffer(&mut buf, area, Path::new("/proj"));
        let raw = row_symbols(&buf, area);
        assert!(
            raw.contains("file:///proj/My%20File.rs"),
            "spaces must be percent-encoded, got: {raw:?}"
        );
    }

    #[test]
    fn strip_osc8_removes_open_and_close() {
        let s = "\x1B]8;;file:///x\x07hello\x1B]8;;\x07";
        assert_eq!(strip_osc8(s), "hello");
    }

    #[test]
    fn escape_url_strips_esc_and_bel() {
        assert_eq!(escape_url("a\x1Bb\x07c"), "abc");
    }

    /// End-to-end: build a real ratatui `Line` with a `theme::PATH`-styled
    /// span, render it through `Paragraph` like the live TUI does, then run
    /// our post-render hook and assert the path cells got OSC 8 wrapped.
    /// Catches any drift between how `theme::PATH` lands in the buffer and
    /// what our predicate matches.
    #[test]
    fn end_to_end_paragraph_then_hook_links_styled_span() {
        use ratatui::text::{Line, Span};
        use ratatui::widgets::{Paragraph, Widget};

        let area = Rect::new(0, 0, 30, 1);
        let mut buf = Buffer::empty(area);
        let line = Line::from(vec![
            Span::raw("\u{25cf} Read "),
            Span::styled("src/main.rs", theme::PATH),
        ]);
        Paragraph::new(line).render(area, &mut buf);

        link_paths_in_buffer(&mut buf, area, Path::new("/proj"));

        let mut raw = String::new();
        for x in 0..30 {
            raw.push_str(buf[(x, 0)].symbol());
        }
        assert!(
            raw.contains("\x1B]8;;file:///proj/src/main.rs\x07"),
            "end-to-end OSC 8 missing, got: {raw:?}"
        );
        // Visible content unchanged.
        let visible = strip_osc8(&raw);
        assert!(visible.contains("\u{25cf} Read src/main.rs"));
    }
}
