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
//! ## Why "first-cell-only" wrapping (#995 fix)
//!
//! The naive approach is to wrap *each* cell of a path run with its own
//! `OPEN \u2026 BEL <char> CLOSE BEL` pair so a per-cell `MoveTo` from the
//! crossterm backend can't shred the link. That works for *visible*
//! output but silently corrupts ratatui's diff: `Buffer::diff` calls
//! `current.symbol().width()` (via `unicode-width`) which counts every
//! printable byte inside the OSC 8 payload as a column. A wrap with a
//! 30-byte URL inflates a 1-column cell to a `width()` of ~37, which
//! makes the diff treat the next ~36 cells as wide-character trailers
//! and drop them from the update set. On the next frame, those skipped
//! cells display *previous-frame* content \u2014 the "two layers of garbled
//! text" symptom in #995.
//!
//! Fix: put the *entire* visible text of the run plus a single OSC 8
//! pair in the run's first cell, and mark the trailing cells with
//! `set_skip(true)` so the diff leaves them alone. The terminal renders
//! the visible text in one go (cursor advances by the visible width,
//! not the symbol byte count), correctly painting over every column the
//! run occupies. The width inflation is now contained to one cell, and
//! the trailing cells are explicitly skipped \u2014 not implicitly via
//! `to_skip`/`invalidated` arithmetic ratatui can't reason about.
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

use crate::theme;
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

/// Cell predicate: matches the `theme::PATH` style.
///
/// We follow `theme::PATH.fg` (rather than hard-coding `Color::Cyan`) so
/// future theme changes — dark/light variants, user themes — remain in
/// sync without anyone having to remember to update this predicate.
/// The `UNDERLINED` modifier is matched via `contains` so cells that pick
/// up extra modifiers downstream (e.g. cursor highlight) still linkify.
fn is_path_cell(cell: &Cell) -> bool {
    cell.fg == theme::PATH.fg.unwrap_or(Color::Reset)
        && cell.modifier.contains(Modifier::UNDERLINED)
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
///
/// Logs at `debug` when stripping happens — these bytes are exotic
/// (most filesystems disallow them) so any hit is worth seeing in
/// support traces, even though the runtime defense is silent.
fn escape_url(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|&c| c != '\x1B' && c != '\x07')
        .collect();
    if cleaned.len() != raw.len() {
        tracing::debug!(
            stripped = raw.len() - cleaned.len(),
            "escape_url: stripped ESC/BEL bytes from URI"
        );
    }
    cleaned
}

/// Wrap a contiguous PATH-styled run with a single OSC 8 hyperlink.
///
/// The run's first cell carries the full payload
/// (`OSC8_OPEN <uri> BEL <visible text> OSC8_CLOSE BEL`); trailing
/// cells in the run are cleared and marked `skip = true` so ratatui's
/// diff won't push them to the terminal. See the module-level rationale
/// (#995) for why this asymmetric layout is required.
fn wrap_run_with_osc8(buf: &mut Buffer, y: u16, start: u16, end: u16, uri: &str) {
    // Concatenate the visible text from each cell in the run.
    let mut visible = String::new();
    for x in start..end {
        visible.push_str(buf[(x, y)].symbol());
    }
    if visible.is_empty() {
        return;
    }
    // Sanitize the visible text the same way we sanitize the URI: ESC and
    // BEL bytes from a hostile filename would otherwise prematurely close
    // the OSC 8 hyperlink and let attacker-controlled bytes execute as
    // terminal control sequences.
    let safe_visible = escape_url(&visible);
    buf[(start, y)].set_symbol(&format!("\x1B]8;;{uri}\x07{safe_visible}\x1B]8;;\x07"));
    // Clear the trailing cells and mark them skip. The first cell's wide
    // symbol already paints these columns visually \u2014 keeping stale
    // per-cell symbols here would mislead callers that read the buffer
    // (mouse-select extraction, snapshot tests) and confuse ratatui's
    // diff if the run ever shrinks between frames.
    for x in (start + 1)..end {
        let cell = &mut buf[(x, y)];
        cell.set_symbol("");
        cell.set_skip(true);
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

    /// Regression for #995: OSC 8 wrapping must not inflate ratatui's
    /// per-cell width accounting in a way that makes `Buffer::diff` skip
    /// neighboring cells.
    ///
    /// `Buffer::diff` reads `current.symbol().width()` (via `unicode-width`)
    /// and uses it to set `to_skip`/`invalidated` for trailing cells. If
    /// the symbol contains the printable bytes of a long URI, that width
    /// can balloon to 30+ columns \u2014 ratatui then treats the next 30 cells
    /// as wide-character trailers and drops them from the diff. On the
    /// next frame those skipped cells display *previous-frame* content,
    /// which is the visible "two layers of garbled text" symptom in the
    /// bug report.
    ///
    /// The fix: only the run's *first* cell carries the OSC 8 payload,
    /// trailing cells are cleared and `set_skip(true)`'d so the diff
    /// doesn't skip beyond the run.
    #[test]
    fn osc8_wrap_does_not_inflate_diff_skip_beyond_run() {
        use ratatui::text::{Line, Span};
        use ratatui::widgets::{Paragraph, Widget};
        use unicode_width::UnicodeWidthStr;

        // Lay out: "Read <PATH-styled>some/long/path.rs</PATH>  <plain>END</plain>"
        // The plain "END" sits AFTER the path run \u2014 if the diff over-skips
        // due to OSC 8 width inflation, those cells won't be in the diff.
        let area = Rect::new(0, 0, 40, 1);
        let path = "some/long/path.rs";
        let line = Line::from(vec![
            Span::raw("Read "),
            Span::styled(path, theme::PATH),
            Span::raw("  END"),
        ]);
        let mut prev = Buffer::empty(area);
        Paragraph::new(line.clone()).render(area, &mut prev);
        link_paths_in_buffer(&mut prev, area, Path::new("/p"));

        // Build a "next" frame with different visible text in the path run
        // (same length so column layout stays comparable). If the diff
        // works correctly, the cell containing the new path's first char
        // must appear in the update set.
        let next_path = "diff/long/PATH.rs";
        let next_line = Line::from(vec![
            Span::raw("Read "),
            Span::styled(next_path, theme::PATH),
            Span::raw("  END"),
        ]);
        let mut next = Buffer::empty(area);
        Paragraph::new(next_line).render(area, &mut next);
        link_paths_in_buffer(&mut next, area, Path::new("/p"));

        let updates = prev.diff(&next);

        // The first cell of the path run (column 5) MUST be in the diff
        // \u2014 it carries the new OSC 8 payload + visible text.
        assert!(
            updates.iter().any(|(x, _, _)| *x == 5),
            "diff missing path-run head cell: {updates:?}"
        );

        // Sanity check: the trailing cells of the path run should NOT
        // appear in the diff (they're skip=true). If they DO appear,
        // we've regressed back to per-cell wrapping.
        let path_len = path.chars().count() as u16;
        let trailing_in_diff: Vec<u16> = updates
            .iter()
            .map(|(x, _, _)| *x)
            .filter(|&x| x > 5 && x < 5 + path_len)
            .collect();
        assert!(
            trailing_in_diff.is_empty(),
            "trailing path cells should be skip=true, but found in diff: {trailing_in_diff:?}"
        );

        // The width of the head cell's symbol is naturally inflated by the
        // OSC 8 escape \u2014 that's expected. What matters is that the
        // trailing cells are explicitly skipped, which we just verified.
        let head_width = next[(5, 0)].symbol().width();
        assert!(
            head_width > 1,
            "OSC 8 wrap should inflate head cell width (got {head_width})"
        );
    }

    /// Regression for #995 (companion): when the path-run shrinks between
    /// frames, the now-blank cells must end up in the diff so the
    /// terminal clears the leftover characters from the longer prior path.
    #[test]
    fn osc8_wrap_clears_leftover_when_path_shrinks() {
        use ratatui::text::{Line, Span};
        use ratatui::widgets::{Paragraph, Widget};

        let area = Rect::new(0, 0, 40, 1);

        // Frame 1: long path occupies cols 0..20.
        let mut prev = Buffer::empty(area);
        Paragraph::new(Line::from(Span::styled("a/very/long/path.rs", theme::PATH)))
            .render(area, &mut prev);
        link_paths_in_buffer(&mut prev, area, Path::new("/p"));

        // Frame 2: short path occupies cols 0..5 only; the remaining
        // columns become blank cells.
        let mut next = Buffer::empty(area);
        Paragraph::new(Line::from(Span::styled("a.rs", theme::PATH))).render(area, &mut next);
        link_paths_in_buffer(&mut next, area, Path::new("/p"));

        let updates = prev.diff(&next);

        // Expect at least one update at columns >= 4 (where the long
        // path's leftover chars sat) so the terminal clears them.
        let trailing_clears: Vec<u16> = updates
            .iter()
            .map(|(x, _, _)| *x)
            .filter(|&x| x >= 4)
            .collect();
        assert!(
            !trailing_clears.is_empty(),
            "shrinking path must produce trailing clear updates, got: {updates:?}"
        );
    }
}
