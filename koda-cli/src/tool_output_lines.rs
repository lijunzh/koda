//! Per-tool-output line rendering — one styled `Line` per source line.
//!
//! These functions are the small, focused renderers for tool-specific
//! output formats: each understands one tool's text shape (Grep's
//! `file:line:content`, List's `d path` / `  path`, Glob's bare paths)
//! and turns it into a colored `Line<'static>`.
//!
//! Living here (instead of inside [`crate::tui_render`]) keeps the
//! event-dispatch loop in `tui_render` focused on *when* to render,
//! while this module owns *how* each tool's output looks. New tools
//! that need bespoke line rendering should add their helper here.

use crate::scroll_buffer::ScrollBuffer;
use crate::theme;
use crate::tui_output::{self, DIM};
use ratatui::text::{Line, Span};

/// Render a single `List` entry with directory/file coloring.
///
/// `List` output format: `d path/to/dir` (directory) or `  path/to/file`
/// (file). Directories show in bold; files inherit the extension-bucket
/// color from [`theme::style_for_extension`] so the same `.rs` file looks
/// the same here, in `Glob`, and (eventually) in any future file picker.
pub(crate) fn render_list_line(buffer: &mut ScrollBuffer, line: &str) {
    let is_dir = line.starts_with("d ");
    let path_str = if is_dir {
        &line[2..]
    } else {
        line.trim_start()
    };

    let style = if is_dir {
        theme::DIRECTORY
    } else {
        theme::style_for_extension(path_extension(path_str))
    };

    let prefix = if is_dir { "\u{1f4c1} " } else { "   " };
    tui_output::emit_line(
        buffer,
        Line::from(vec![
            Span::styled("  \u{2502} ", DIM),
            Span::raw(prefix),
            Span::styled(path_str.to_string(), style),
        ]),
    );
}

/// Render a single `Glob` result line with extension-based coloring.
///
/// `Glob` output is structured as a header (`N files matched:`), then
/// bare relative paths, optionally capped by `[Capped at N results]`.
/// The header / cap lines render dim; paths inherit extension colors
/// from [`theme::style_for_extension`] so a `Glob` and a `List` of the
/// same directory paint files identically.
pub(crate) fn render_glob_line(buffer: &mut ScrollBuffer, line: &str) {
    let trimmed = line.trim();
    // Headers and meta lines: dim them out so the file paths pop.
    let is_meta = trimmed.is_empty()
        || trimmed.ends_with("matched:")
        || trimmed.starts_with("No files matched")
        || trimmed.starts_with("[Capped");
    if is_meta {
        tui_output::emit_line(
            buffer,
            Line::from(vec![
                Span::styled("  \u{2502} ", DIM),
                Span::styled(line.to_string(), DIM),
            ]),
        );
        return;
    }

    let style = theme::style_for_extension(path_extension(trimmed));
    tui_output::emit_line(
        buffer,
        Line::from(vec![
            Span::styled("  \u{2502} ", DIM),
            Span::raw("   "),
            Span::styled(trimmed.to_string(), style),
        ]),
    );
}

/// Render a single `Grep` result line with the file path highlighted.
///
/// `Grep` output format: `file_path:line_number:content`. The file path
/// renders in cyan, the line number in yellow, and — when the search
/// pattern is known — every matched substring inside the content gets
/// the [`theme::MATCH_HIT`] style (bold amber). Pattern-as-regex is
/// honored; an invalid regex is treated as a literal substring search
/// to keep highlighting graceful even when the user passes an unusual
/// pattern.
pub(crate) fn render_grep_line(buffer: &mut ScrollBuffer, line: &str, pattern: Option<&str>) {
    let parsed = line.split_once(':').and_then(|(file, rest)| {
        rest.split_once(':')
            .map(|(lineno, content)| (file.to_string(), lineno.to_string(), content.to_string()))
    });

    let (file, lineno, content) = match parsed {
        Some(t) => t,
        None => {
            // Unrecognized format — just dim the prefix and pass through.
            tui_output::emit_line(
                buffer,
                Line::from(vec![
                    Span::styled("  \u{2502} ", DIM),
                    Span::raw(line.to_string()),
                ]),
            );
            return;
        }
    };

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(8);
    spans.push(Span::styled("  \u{2502} ", DIM));
    spans.push(Span::styled(file, theme::PATH));
    spans.push(Span::styled(":", DIM));
    spans.push(Span::styled(lineno, theme::LINENO));
    spans.push(Span::styled(":", DIM));
    extend_with_match_highlights(&mut spans, &content, pattern);
    tui_output::emit_line(buffer, Line::from(spans));
}

/// Append spans for `content`, highlighting every substring matching `pattern`.
///
/// Falls back to a single plain span when:
/// - no pattern was provided,
/// - the pattern doesn't compile as regex *and* doesn't appear literally,
/// - or there are no matches at all.
///
/// Cap of 32 highlight passes per line keeps pathological inputs from
/// blowing up render time. Anything past the cap renders as plain text.
fn extend_with_match_highlights(
    spans: &mut Vec<Span<'static>>,
    content: &str,
    pattern: Option<&str>,
) {
    const MAX_HITS_PER_LINE: usize = 32;

    let pattern = match pattern {
        Some(p) if !p.is_empty() => p,
        _ => {
            spans.push(Span::raw(content.to_string()));
            return;
        }
    };

    // Try regex first (matches Grep tool's behavior); fall back to a
    // literal-substring scan if compilation fails.
    let ranges: Vec<(usize, usize)> = match regex::RegexBuilder::new(pattern)
        .case_insensitive(false)
        .build()
    {
        Ok(re) => re
            .find_iter(content)
            .take(MAX_HITS_PER_LINE)
            .map(|m: regex::Match<'_>| (m.start(), m.end()))
            .collect(),
        Err(_) => content
            .match_indices(pattern)
            .take(MAX_HITS_PER_LINE)
            .map(|(i, s)| (i, i + s.len()))
            .collect(),
    };

    if ranges.is_empty() {
        spans.push(Span::raw(content.to_string()));
        return;
    }

    let mut cursor = 0usize;
    for (start, end) in ranges {
        if start < cursor || end > content.len() {
            // Skip overlapping/invalid ranges defensively.
            continue;
        }
        if start > cursor {
            spans.push(Span::raw(content[cursor..start].to_string()));
        }
        spans.push(Span::styled(
            content[start..end].to_string(),
            theme::MATCH_HIT,
        ));
        cursor = end;
    }
    if cursor < content.len() {
        spans.push(Span::raw(content[cursor..].to_string()));
    }
}

/// Lowercase extension for a path string, or `""` if none.
fn path_extension(path: &str) -> &str {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn match_hits<'a>(spans: &'a [Span<'static>]) -> Vec<&'a str> {
        spans
            .iter()
            .filter(|s| s.style == theme::MATCH_HIT)
            .map(|s| s.content.as_ref())
            .collect()
    }

    fn drain_lines(buf: &ScrollBuffer) -> Vec<&Line<'static>> {
        buf.all_lines().collect()
    }

    // ── render_grep_line + match highlighting ───────────────────────

    #[test]
    fn grep_line_highlights_literal_pattern() {
        let mut buf = ScrollBuffer::new(100);
        render_grep_line(&mut buf, "src/foo.rs:42:let foo = bar;", Some("foo"));
        let lines = drain_lines(&buf);
        let hits = match_hits(&lines.last().unwrap().spans);
        assert_eq!(hits, vec!["foo"], "literal pattern not highlighted");
    }

    #[test]
    fn grep_line_highlights_multiple_hits() {
        let mut buf = ScrollBuffer::new(100);
        render_grep_line(&mut buf, "a.rs:1:foo and foo again", Some("foo"));
        let lines = drain_lines(&buf);
        let hits = match_hits(&lines.last().unwrap().spans);
        assert_eq!(hits.len(), 2, "expected 2 highlighted matches");
    }

    #[test]
    fn grep_line_no_pattern_no_highlights() {
        let mut buf = ScrollBuffer::new(100);
        render_grep_line(&mut buf, "a.rs:1:hello", None);
        let lines = drain_lines(&buf);
        let hits = match_hits(&lines.last().unwrap().spans);
        assert!(hits.is_empty());
    }

    #[test]
    fn grep_line_invalid_regex_falls_back_to_literal() {
        // Unbalanced bracket would fail regex compilation; we should
        // still find the literal substring "[broke" inside the content.
        let mut buf = ScrollBuffer::new(100);
        render_grep_line(&mut buf, "a.rs:1:has [broken regex inside", Some("[broke"));
        let lines = drain_lines(&buf);
        let hits = match_hits(&lines.last().unwrap().spans);
        assert_eq!(hits, vec!["[broke"], "literal fallback failed");
    }

    #[test]
    fn grep_line_unparseable_format_passes_through() {
        let mut buf = ScrollBuffer::new(100);
        render_grep_line(&mut buf, "no colons here", Some("x"));
        let lines = drain_lines(&buf);
        // Should still emit one line, no panic.
        assert_eq!(lines.len(), 1);
    }

    // ── render_glob_line ────────────────────────────────────────────

    #[test]
    fn glob_line_dims_header() {
        let mut buf = ScrollBuffer::new(100);
        render_glob_line(&mut buf, "3 files matched:");
        let lines = drain_lines(&buf);
        let body_styles: Vec<_> = lines
            .last()
            .unwrap()
            .spans
            .iter()
            .map(|s| s.style)
            .collect();
        assert!(body_styles.iter().all(|s| *s == DIM));
    }

    #[test]
    fn glob_line_colors_path_by_extension() {
        let mut buf = ScrollBuffer::new(100);
        render_glob_line(&mut buf, "src/main.rs");
        let lines = drain_lines(&buf);
        let expected = theme::style_for_extension("rs");
        let found = lines
            .last()
            .unwrap()
            .spans
            .iter()
            .any(|s| s.style.fg == expected.fg && s.content.as_ref() == "src/main.rs");
        assert!(found, "glob path not colored by extension");
    }

    // ── render_list_line ────────────────────────────────────────────

    #[test]
    fn list_line_bolds_directories() {
        let mut buf = ScrollBuffer::new(100);
        render_list_line(&mut buf, "d src/tools");
        let lines = drain_lines(&buf);
        let found = lines
            .last()
            .unwrap()
            .spans
            .iter()
            .any(|s| s.style == theme::DIRECTORY && s.content.as_ref() == "src/tools");
        assert!(found, "directory not bolded");
    }
}
