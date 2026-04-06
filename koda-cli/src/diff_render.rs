//! Diff preview renderer — native ratatui `Line`/`Span` output.
//!
//! Takes structured [`DiffPreview`] data from koda-core and produces
//! `Vec<Line<'static>>` with:
//! - Proper unified diff with hunk headers and context lines
//! - Syntax highlighting with cross-hunk context (via pre-highlighted files)
//! - Dark background tint for diff lines (red removed, green added)
//! - Gutter metadata for NoSelect copy support

use crate::highlight;
use koda_core::preview::{
    DeleteDirPreview, DeleteFilePreview, DiffLine, DiffPreview, DiffTag, UnifiedDiffPreview,
    WriteNewPreview,
};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

// ── Styles ────────────────────────────────────────────────────

const LINE_RED_BG: Color = Color::Rgb(50, 0, 0);
const LINE_GREEN_BG: Color = Color::Rgb(0, 35, 0);
const DIM: Style = Style::new().fg(Color::DarkGray);
const HUNK_HEADER: Style = Style::new().fg(Color::Cyan);

/// Width of the gutter: 4-digit line number + space + sigil + space = 7.
/// Used by NoSelect to know how many leading columns to skip on copy.
pub const GUTTER_WIDTH: u16 = 7;

// ── Public API ────────────────────────────────────────────────

/// Render a [`DiffPreview`] as native ratatui `Line`s.
///
/// Each diff line's gutter (line numbers + ±) occupies [`GUTTER_WIDTH`]
/// columns. The caller can use this to implement NoSelect on copy.
pub fn render_lines(preview: &DiffPreview) -> Vec<Line<'static>> {
    match preview {
        DiffPreview::UnifiedDiff(diff) => render_unified_diff(diff),
        DiffPreview::WriteNew(w) => render_write_new(w),
        DiffPreview::DeleteFile(d) => render_delete_file(d),
        DiffPreview::DeleteDir(d) => render_delete_dir(d),
        DiffPreview::FileNotYetExists => {
            vec![Line::styled("(file does not exist yet)", DIM)]
        }
        DiffPreview::PathNotFound => {
            vec![Line::styled("(path does not exist)", DIM)]
        }
    }
}

// ── Unified diff renderer ─────────────────────────────────────

fn render_unified_diff(diff: &UnifiedDiffPreview) -> Vec<Line<'static>> {
    let ext = std::path::Path::new(&diff.path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    // Pre-highlight both files for cross-hunk syntax context
    let old_highlights = highlight::pre_highlight(&diff.old_content, ext);
    let new_highlights = highlight::pre_highlight(&diff.new_content, ext);

    let mut lines = Vec::new();

    // File header
    lines.push(Line::styled(format!("╭─── {} ───╮", diff.path), DIM));

    for (i, hunk) in diff.hunks.iter().enumerate() {
        // Hunk separator (between hunks, not before the first)
        if i > 0 {
            lines.push(Line::styled("  ⋯", DIM));
        }

        // Hunk header: @@ -old_start,old_count +new_start,new_count @@
        lines.push(Line::styled(
            format!(
                "@@ -{},{} +{},{} @@",
                hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
            ),
            HUNK_HEADER,
        ));

        // Hunk lines
        for diff_line in &hunk.lines {
            let rendered = render_diff_line(diff_line, &old_highlights, &new_highlights);
            lines.push(rendered);
        }
    }

    // Close frame
    lines.push(Line::styled(format!("╰─── {} ───╯", diff.path), DIM));

    if diff.truncated {
        lines.push(Line::styled("... diff truncated (file too large)", DIM));
    }

    lines
}

/// Render a single diff line with gutter + syntax-highlighted content.
///
/// Uses pre-computed highlights from the full file for correct cross-hunk
/// syntax context (multiline strings, comments, etc.).
fn render_diff_line(
    line: &DiffLine,
    old_highlights: &[Vec<Span<'static>>],
    new_highlights: &[Vec<Span<'static>>],
) -> Line<'static> {
    let (sigil, sigil_color, bg_color, highlights, line_num) = match line.tag {
        DiffTag::Context => {
            let num = line.old_line.unwrap_or(0);
            (' ', Color::DarkGray, None, old_highlights, num)
        }
        DiffTag::Delete => {
            let num = line.old_line.unwrap_or(0);
            ('-', Color::Red, Some(LINE_RED_BG), old_highlights, num)
        }
        DiffTag::Insert => {
            let num = line.new_line.unwrap_or(0);
            ('+', Color::Green, Some(LINE_GREEN_BG), new_highlights, num)
        }
    };

    let mut spans = Vec::new();

    // Gutter: line number + sigil (GUTTER_WIDTH chars total)
    let gutter_style = Style::default().fg(sigil_color).add_modifier(Modifier::DIM);
    spans.push(Span::styled(
        format!("{:>4} {} ", line_num, sigil),
        gutter_style,
    ));

    // Content: use pre-highlighted spans if available, with background tint
    let idx = line_num.saturating_sub(1); // 0-based index
    if idx < highlights.len() {
        for hl_span in &highlights[idx] {
            let mut style = hl_span.style;
            if let Some(bg) = bg_color {
                style = style.bg(bg);
            }
            spans.push(Span::styled(hl_span.content.clone(), style));
        }
    } else {
        // Fallback: no highlighting available
        let style = match bg_color {
            Some(bg) => Style::default().bg(bg),
            None => Style::default(),
        };
        spans.push(Span::styled(line.content.clone(), style));
    }

    Line::from(spans)
}

// ── Write new file ────────────────────────────────────────────

fn render_write_new(w: &WriteNewPreview) -> Vec<Line<'static>> {
    let ext = std::path::Path::new(&w.path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    let mut lines = vec![Line::styled(
        format!(
            "╭─── {} (new file: {} lines, {} bytes) ───╮",
            w.path, w.line_count, w.byte_count
        ),
        DIM,
    )];

    let mut hl = crate::highlight::CodeHighlighter::new(ext);
    for (i, content) in w.first_lines.iter().enumerate() {
        let mut spans = vec![Span::styled(
            format!("{:>4} + ", i + 1),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::DIM),
        )];
        let highlighted = hl.highlight_spans(content);
        for mut s in highlighted {
            s.style = s.style.bg(LINE_GREEN_BG);
            spans.push(s);
        }
        lines.push(Line::from(spans));
    }

    if w.truncated {
        lines.push(Line::styled(
            format!("... +{} more lines", w.line_count - w.first_lines.len()),
            DIM,
        ));
    }

    lines.push(Line::styled(format!("╰─── {} ───╯", w.path), DIM));

    lines
}

// ── Delete ────────────────────────────────────────────────────

fn render_delete_file(d: &DeleteFilePreview) -> Vec<Line<'static>> {
    vec![Line::styled(
        format!("Removing {} lines ({} bytes)", d.line_count, d.byte_count),
        Style::default().bg(LINE_RED_BG),
    )]
}

fn render_delete_dir(d: &DeleteDirPreview) -> Vec<Line<'static>> {
    if d.recursive {
        vec![Line::styled(
            "Removing directory and all contents",
            Style::default().bg(LINE_RED_BG),
        )]
    } else {
        vec![Line::styled(
            "Removing empty directory",
            Style::default().bg(LINE_RED_BG),
        )]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use koda_core::preview::*;

    #[test]
    fn test_unified_diff_has_hunk_headers() {
        let preview = DiffPreview::UnifiedDiff(UnifiedDiffPreview {
            path: "test.rs".into(),
            old_content: "fn main() {\n    println!(\"hello\");\n}\n".into(),
            new_content: "fn main() {\n    println!(\"world\");\n}\n".into(),
            hunks: vec![DiffHunk {
                old_start: 1,
                old_count: 3,
                new_start: 1,
                new_count: 3,
                lines: vec![
                    DiffLine {
                        tag: DiffTag::Context,
                        content: "fn main() {".into(),
                        old_line: Some(1),
                        new_line: Some(1),
                    },
                    DiffLine {
                        tag: DiffTag::Delete,
                        content: "    println!(\"hello\");".into(),
                        old_line: Some(2),
                        new_line: None,
                    },
                    DiffLine {
                        tag: DiffTag::Insert,
                        content: "    println!(\"world\");".into(),
                        old_line: None,
                        new_line: Some(2),
                    },
                    DiffLine {
                        tag: DiffTag::Context,
                        content: "}".into(),
                        old_line: Some(3),
                        new_line: Some(3),
                    },
                ],
            }],
            truncated: false,
        });

        let lines = render_lines(&preview);
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();

        // Should have file header
        assert!(text[0].contains("test.rs"), "header: {}", text[0]);
        // Should have hunk header
        assert!(
            text.iter().any(|t| t.contains("@@")),
            "should have hunk header"
        );
        // Should have line numbers with sigils
        assert!(
            text.iter().any(|t| t.contains(" - ")),
            "should have delete marker"
        );
        assert!(
            text.iter().any(|t| t.contains(" + ")),
            "should have insert marker"
        );
    }

    #[test]
    fn test_write_new_rendering() {
        let preview = DiffPreview::WriteNew(WriteNewPreview {
            path: "new.rs".into(),
            line_count: 10,
            byte_count: 200,
            first_lines: vec!["fn main() {}".into()],
            truncated: true,
        });
        let lines = render_lines(&preview);
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(text[0].contains("new.rs"));
        assert!(text.iter().any(|t| t.contains("more lines")));
    }

    #[test]
    fn test_hunk_separator_between_hunks() {
        let preview = DiffPreview::UnifiedDiff(UnifiedDiffPreview {
            path: "test.rs".into(),
            old_content: String::new(),
            new_content: String::new(),
            hunks: vec![
                DiffHunk {
                    old_start: 1,
                    old_count: 1,
                    new_start: 1,
                    new_count: 1,
                    lines: vec![DiffLine {
                        tag: DiffTag::Context,
                        content: "a".into(),
                        old_line: Some(1),
                        new_line: Some(1),
                    }],
                },
                DiffHunk {
                    old_start: 50,
                    old_count: 1,
                    new_start: 50,
                    new_count: 1,
                    lines: vec![DiffLine {
                        tag: DiffTag::Context,
                        content: "b".into(),
                        old_line: Some(50),
                        new_line: Some(50),
                    }],
                },
            ],
            truncated: false,
        });
        let lines = render_lines(&preview);
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(
            text.iter().any(|t| t.contains('⋯')),
            "should have hunk separator"
        );
    }

    #[test]
    fn test_delete_file_shows_line_and_byte_count() {
        let preview = DiffPreview::DeleteFile(DeleteFilePreview {
            line_count: 42,
            byte_count: 1024,
        });
        let lines = render_lines(&preview);
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        let full = text.join(" ");
        assert!(full.contains("42"), "should mention line count: {full}");
        assert!(full.contains("1024"), "should mention byte count: {full}");
    }

    #[test]
    fn test_delete_dir_recursive_message() {
        let preview = DiffPreview::DeleteDir(DeleteDirPreview { recursive: true });
        let lines = render_lines(&preview);
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        let full = text.join(" ");
        assert!(
            full.contains("all contents") || full.contains("recursive"),
            "recursive delete should mention all contents: {full}"
        );
    }

    #[test]
    fn test_delete_dir_non_recursive_message() {
        let preview = DiffPreview::DeleteDir(DeleteDirPreview { recursive: false });
        let lines = render_lines(&preview);
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        let full = text.join(" ");
        assert!(
            full.contains("empty"),
            "non-recursive should mention empty directory: {full}"
        );
    }

    #[test]
    fn test_unified_diff_truncated_flag() {
        // When truncated=true the render should include some truncation indicator.
        let preview = DiffPreview::UnifiedDiff(UnifiedDiffPreview {
            path: "big.rs".into(),
            old_content: String::new(),
            new_content: String::new(),
            hunks: vec![DiffHunk {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 1,
                lines: vec![DiffLine {
                    tag: DiffTag::Insert,
                    content: "new line".into(),
                    old_line: None,
                    new_line: Some(1),
                }],
            }],
            truncated: true,
        });
        let lines = render_lines(&preview);
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        // Check that there's at least a line indicating truncation
        assert!(
            text.iter().any(|t| {
                t.to_lowercase().contains("truncat")
                    || t.contains('…')
                    || t.contains("more")
                    || t.contains("⋯")
            }),
            "truncated diff should have an indicator: {text:?}"
        );
    }

    #[test]
    fn test_file_not_yet_exists_variant() {
        let preview = DiffPreview::FileNotYetExists;
        let lines = render_lines(&preview);
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(
            text.iter().any(|t| t.contains("not exist")),
            "FileNotYetExists should say the file does not exist: {text:?}"
        );
    }

    #[test]
    fn test_path_not_found_variant() {
        let preview = DiffPreview::PathNotFound;
        let lines = render_lines(&preview);
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(
            text.iter().any(|t| t.contains("not exist")),
            "PathNotFound should say path does not exist: {text:?}"
        );
    }
}
