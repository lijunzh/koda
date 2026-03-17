//! Mouse text selection for fullscreen TUI.
//!
//! When mouse capture is enabled (for scroll wheel), native terminal
//! text selection is unavailable. This module implements click-drag
//! selection in the history panel with automatic clipboard copy.
//!
//! Design: line-granularity selection with character-level endpoints.
//! The user clicks and drags in the history area; selected text is
//! highlighted with inverted colors. On mouse release, the selection
//! is copied to clipboard automatically.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

/// A position in the history panel.
///
/// `row` is in **buffer space** (absolute visual row index across the
/// entire scroll buffer, accounting for line wrapping). This makes
/// selections stable across scroll operations — the anchor doesn't
/// become stale when the viewport moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VisualPos {
    /// Absolute visual row in the scroll buffer (0 = first row).
    pub row: u16,
    /// Column (0-based).
    pub col: u16,
}

/// Active text selection state.
#[derive(Debug, Clone)]
pub(crate) struct Selection {
    /// Where the drag started (anchor).
    pub anchor: VisualPos,
    /// Current drag position (cursor).
    pub cursor: VisualPos,
}

impl Selection {
    /// Return (start, end) normalized so start ≤ end.
    pub fn ordered(&self) -> (VisualPos, VisualPos) {
        if self.anchor.row < self.cursor.row
            || (self.anchor.row == self.cursor.row && self.anchor.col <= self.cursor.col)
        {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    /// Check if a visual row is within the selection range.
    #[allow(dead_code)] // used in render path, will be wired for per-row highlighting
    pub fn contains_row(&self, row: u16) -> bool {
        let (start, end) = self.ordered();
        row >= start.row && row <= end.row
    }
}

/// Build ALL visual rows from logical lines, accounting for line wrapping.
///
/// Returns one String per visual row across the entire scroll buffer.
/// Used for buffer-space text extraction during copy operations.
pub(crate) fn build_all_visual_rows(lines: &[Line<'_>], viewport_width: usize) -> Vec<String> {
    let mut visual_rows: Vec<String> = Vec::new();
    let w = viewport_width.max(1);

    for line in lines {
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        if text.is_empty() {
            visual_rows.push(String::new());
        } else {
            let chars: Vec<char> = text.chars().collect();
            for chunk in chars.chunks(w) {
                visual_rows.push(chunk.iter().collect());
            }
        }
    }

    visual_rows
}

/// Extract the plain text content visible in the history area.
///
/// Returns one String per visual row, accounting for line wrapping.
/// The returned vec has exactly `viewport_height` entries (or fewer
/// if the buffer is shorter than the viewport).
#[cfg(test)]
fn extract_visible_text(
    lines: &[Line<'_>],
    scroll_from_top: u16,
    viewport_width: usize,
    viewport_height: usize,
) -> Vec<String> {
    let mut visual_rows: Vec<String> = Vec::new();

    // Build all visual rows from logical lines
    for line in lines {
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        if text.is_empty() {
            visual_rows.push(String::new());
        } else {
            // Wrap at viewport_width
            let chars: Vec<char> = text.chars().collect();
            for chunk in chars.chunks(viewport_width.max(1)) {
                visual_rows.push(chunk.iter().collect());
            }
        }
    }

    // Slice to the visible window
    let start = scroll_from_top as usize;
    let end = (start + viewport_height).min(visual_rows.len());
    if start < visual_rows.len() {
        visual_rows[start..end].to_vec()
    } else {
        Vec::new()
    }
}

/// Extract the selected text from a vec of visual rows.
///
/// `rows` can be either the full buffer (from `build_all_visual_rows`) or
/// a viewport slice (from `extract_visible_text`). The selection's row
/// indices must be in the same coordinate space as `rows`.
pub(crate) fn extract_selected_text(rows: &[String], selection: &Selection) -> String {
    let (start, end) = selection.ordered();
    let mut result = String::new();

    for row in start.row..=end.row {
        let idx = row as usize;
        if idx >= rows.len() {
            break;
        }
        let line = &rows[idx];
        let chars: Vec<char> = line.chars().collect();

        let col_start = if row == start.row {
            start.col as usize
        } else {
            0
        };
        let col_end = if row == end.row {
            (end.col as usize + 1).min(chars.len())
        } else {
            chars.len()
        };

        if col_start < chars.len() {
            let selected: String = chars[col_start..col_end.min(chars.len())].iter().collect();
            result.push_str(&selected);
        }
        if row < end.row {
            result.push('\n');
        }
    }

    result
}

/// Apply selection highlighting to lines being rendered.
///
/// Returns modified lines with inverted styles on selected regions.
/// Selection coordinates are in **buffer space** (absolute visual rows).
pub(crate) fn apply_selection_highlight<'a>(
    lines: Vec<Line<'a>>,
    selection: &Selection,
    _scroll_from_top: u16,
    viewport_width: usize,
    _history_y: u16,
) -> Vec<Line<'a>> {
    let (sel_start, sel_end) = selection.ordered();
    let highlight = Style::default()
        .bg(Color::Rgb(68, 68, 120))
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);

    let mut visual_row: u16 = 0;
    let mut result = Vec::with_capacity(lines.len());

    for line in lines {
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        let rows_this_line = if text.is_empty() {
            1
        } else {
            text.chars().count().div_ceil(viewport_width.max(1))
        } as u16;

        // Selection is in buffer space — compare directly against visual_row
        let line_end = visual_row + rows_this_line - 1;
        let in_selection = line_end >= sel_start.row && visual_row <= sel_end.row;

        if in_selection {
            let highlighted_spans: Vec<Span<'a>> = line
                .spans
                .into_iter()
                .map(|s| Span::styled(s.content, highlight))
                .collect();
            result.push(Line::from(highlighted_spans));
        } else {
            result.push(line);
        }

        visual_row += rows_this_line;
    }

    result
}

/// Copy text to system clipboard, returning a user-friendly message.
pub(crate) fn copy_to_clipboard(text: &str) -> Result<String, String> {
    match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text)) {
        Ok(()) => {
            let preview: String = text.chars().take(50).collect();
            let lines = text.lines().count();
            Ok(format!("Copied {lines} line(s): {preview}…"))
        }
        Err(e) => Err(format!("Clipboard error: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_line(text: &str) -> Line<'static> {
        Line::from(text.to_string())
    }

    #[test]
    fn test_selection_ordered() {
        let sel = Selection {
            anchor: VisualPos { row: 5, col: 10 },
            cursor: VisualPos { row: 2, col: 3 },
        };
        let (start, end) = sel.ordered();
        assert_eq!(start.row, 2);
        assert_eq!(end.row, 5);
    }

    #[test]
    fn test_selection_contains_row() {
        let sel = Selection {
            anchor: VisualPos { row: 2, col: 0 },
            cursor: VisualPos { row: 5, col: 10 },
        };
        assert!(!sel.contains_row(1));
        assert!(sel.contains_row(2));
        assert!(sel.contains_row(3));
        assert!(sel.contains_row(5));
        assert!(!sel.contains_row(6));
    }

    #[test]
    fn test_extract_visible_text_basic() {
        let lines = vec![
            make_line("line one"),
            make_line("line two"),
            make_line("line three"),
        ];
        let visible = extract_visible_text(&lines, 0, 80, 10);
        assert_eq!(visible.len(), 3);
        assert_eq!(visible[0], "line one");
        assert_eq!(visible[2], "line three");
    }

    #[test]
    fn test_extract_visible_text_with_scroll() {
        let lines = vec![
            make_line("line one"),
            make_line("line two"),
            make_line("line three"),
        ];
        let visible = extract_visible_text(&lines, 1, 80, 10);
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0], "line two");
    }

    #[test]
    fn test_extract_visible_text_with_wrapping() {
        // A line that wraps at width 10
        let lines = vec![make_line("abcdefghij12345")];
        let visible = extract_visible_text(&lines, 0, 10, 10);
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0], "abcdefghij");
        assert_eq!(visible[1], "12345");
    }

    #[test]
    fn test_extract_selected_text_single_line() {
        let rows = vec!["hello world".to_string()];
        let sel = Selection {
            anchor: VisualPos { row: 0, col: 6 },
            cursor: VisualPos { row: 0, col: 10 },
        };
        let text = extract_selected_text(&rows, &sel);
        assert_eq!(text, "world");
    }

    #[test]
    fn test_extract_selected_text_multi_line() {
        let rows = vec![
            "first line".to_string(),
            "second line".to_string(),
            "third line".to_string(),
        ];
        let sel = Selection {
            anchor: VisualPos { row: 0, col: 6 },
            cursor: VisualPos { row: 2, col: 4 },
        };
        let text = extract_selected_text(&rows, &sel);
        assert_eq!(text, "line\nsecond line\nthird");
    }

    #[test]
    fn test_copy_to_clipboard_format() {
        // We can't test actual clipboard in CI, but we can test the extraction
        let rows = vec!["hello".to_string(), "world".to_string()];
        let sel = Selection {
            anchor: VisualPos { row: 0, col: 0 },
            cursor: VisualPos { row: 1, col: 4 },
        };
        let text = extract_selected_text(&rows, &sel);
        assert_eq!(text, "hello\nworld");
    }

    #[test]
    fn test_build_all_visual_rows_basic() {
        let lines = vec![
            make_line("line one"),
            make_line("line two"),
            make_line("line three"),
        ];
        let rows = build_all_visual_rows(&lines, 80);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], "line one");
        assert_eq!(rows[2], "line three");
    }

    #[test]
    fn test_build_all_visual_rows_with_wrapping() {
        let lines = vec![make_line("abcdefghij12345")];
        let rows = build_all_visual_rows(&lines, 10);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], "abcdefghij");
        assert_eq!(rows[1], "12345");
    }

    #[test]
    fn test_build_all_visual_rows_empty_lines() {
        let lines = vec![make_line("hello"), make_line(""), make_line("world")];
        let rows = build_all_visual_rows(&lines, 80);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], "hello");
        assert_eq!(rows[1], "");
        assert_eq!(rows[2], "world");
    }

    /// Simulates a cross-page selection: anchor at buffer row 2, cursor at
    /// buffer row 8, with a viewport that only shows 5 rows at a time.
    /// The selection should capture all rows 2–8 regardless of viewport.
    #[test]
    fn test_cross_page_selection() {
        let lines: Vec<Line<'_>> = (0..20).map(|i| make_line(&format!("line {i}"))).collect();
        let all_rows = build_all_visual_rows(&lines, 80);
        assert_eq!(all_rows.len(), 20);

        // Selection spans rows 2–8 in buffer space (would cross viewport boundary)
        let sel = Selection {
            anchor: VisualPos { row: 2, col: 0 },
            cursor: VisualPos { row: 8, col: 5 },
        };
        let text = extract_selected_text(&all_rows, &sel);
        assert!(text.contains("line 2"));
        assert!(text.contains("line 5"));
        assert!(text.contains("line 8"));
        assert!(!text.contains("line 1\n"));
        assert!(!text.contains("line 9"));
        assert_eq!(text.lines().count(), 7); // rows 2,3,4,5,6,7,8
    }
}
