//! Word-wrapping input renderer.
//!
//! Renders TextArea content with word-wrap (matching conversation text)
//! while preserving cursor position. The TextArea handles editing logic;
//! this module only handles display.
//!
//! Fixes #517: input prompt should word-wrap like conversation text blocks.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Paragraph, Widget, Wrap},
};
use ratatui_textarea::TextArea;
use unicode_width::UnicodeWidthChar;

/// Render the TextArea content with word-wrapping into the given area,
/// drawing the cursor at the correct visual position.
pub fn render_wrapped_input(
    textarea: &TextArea,
    area: Rect,
    buf: &mut Buffer,
    cursor_style: Style,
) {
    let lines = textarea.lines();
    let cursor = textarea.cursor();
    let (cursor_row, cursor_col) = (cursor.0, cursor.1);
    let width = area.width as usize;

    if width == 0 || area.height == 0 {
        return;
    }

    // Build styled lines for Paragraph rendering
    let display_lines: Vec<Line<'_>> = lines
        .iter()
        .map(|l| Line::from(Span::raw(l.as_str())))
        .collect();

    // Render the text with word-wrapping
    let paragraph = Paragraph::new(display_lines).wrap(Wrap { trim: false });
    paragraph.render(area, buf);

    // Calculate cursor visual position in the wrapped text
    let (vis_row, vis_col) = logical_to_visual(lines, cursor_row, cursor_col, width);

    // Draw cursor if it's within the visible area
    let cursor_y = area.y + vis_row as u16;
    let cursor_x = area.x + vis_col as u16;
    if cursor_y < area.y + area.height && cursor_x < area.x + area.width {
        let cell = &mut buf[(cursor_x, cursor_y)];
        cell.set_style(cursor_style);
    }
}

/// Compute the visual height (wrapped lines) for all textarea content.
///
/// Used to dynamically size the input area in the layout.
pub fn wrapped_height(textarea: &TextArea, width: usize) -> usize {
    if width == 0 {
        return textarea.lines().len().max(1);
    }
    textarea
        .lines()
        .iter()
        .map(|line| visual_line_count(line, width))
        .sum::<usize>()
        .max(1)
}

/// Map a logical cursor position `(row, col)` to a visual position
/// `(visual_row, visual_col)` accounting for word wrapping.
fn logical_to_visual(
    lines: &[String],
    cursor_row: usize,
    cursor_col: usize,
    width: usize,
) -> (usize, usize) {
    let mut visual_row = 0usize;

    // Count visual rows for all lines before the cursor line
    for line in lines.iter().take(cursor_row) {
        visual_row += visual_line_count(line, width);
    }

    // Now find the visual position within the cursor line
    let cursor_line = lines.get(cursor_row).map(|s| s.as_str()).unwrap_or("");
    let (extra_rows, vis_col) = cursor_visual_offset(cursor_line, cursor_col, width);
    visual_row += extra_rows;

    (visual_row, vis_col)
}

/// Compute how many visual lines a text line occupies at a given width.
///
/// Delegates to `wrap_util::visual_line_count` — single source of truth.
fn visual_line_count(line: &str, width: usize) -> usize {
    crate::wrap_util::visual_line_count(line, width)
}

/// For a given cursor column position in a line, compute:
/// - How many visual rows down from the start of this line the cursor is
/// - What the visual column is on that visual row
fn cursor_visual_offset(line: &str, cursor_col: usize, width: usize) -> (usize, usize) {
    let w = width.max(1);
    let mut visual_row = 0usize;
    let mut col = 0usize;
    let mut word_start_col = 0usize;
    let mut in_word = false;
    for (char_idx, ch) in line.chars().enumerate() {
        if char_idx == cursor_col {
            return (visual_row, col);
        }

        let char_w = ch.width().unwrap_or(0);
        let is_space = ch == ' ' || ch == '\t';

        if is_space {
            in_word = false;
            if col + char_w > w {
                visual_row += 1;
                col = char_w;
            } else {
                col += char_w;
            }
            word_start_col = col;
        } else {
            if !in_word {
                word_start_col = col;
                in_word = true;
            }
            if col + char_w > w {
                if word_start_col > 0 && word_start_col <= w {
                    visual_row += 1;
                    let word_len_so_far = col - word_start_col;
                    col = word_len_so_far + char_w;
                    word_start_col = 0;
                } else {
                    visual_row += 1;
                    col = char_w;
                    word_start_col = 0;
                }
            } else {
                col += char_w;
            }
        }
    }

    // Cursor at end of line
    (visual_row, col)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_visual_line_count_short() {
        assert_eq!(visual_line_count("hello", 80), 1);
    }

    #[test]
    fn test_visual_line_count_empty() {
        assert_eq!(visual_line_count("", 80), 1);
    }

    #[test]
    fn test_visual_line_count_wrap() {
        // 100 chars of 'x' at width 80 = 2 visual lines
        let line = "x".repeat(100);
        assert_eq!(visual_line_count(&line, 80), 2);
    }

    #[test]
    fn test_visual_line_count_word_wrap() {
        // "<75 a's> <75 b's>" at width 80
        // Row 1: 75 a's + space (76), b's don't fit → wrap
        // Row 2: 75 b's
        let line = format!("{} {}", "a".repeat(75), "b".repeat(75));
        assert_eq!(visual_line_count(&line, 80), 2);
    }

    #[test]
    fn test_cursor_visual_offset_no_wrap() {
        assert_eq!(cursor_visual_offset("hello world", 5, 80), (0, 5));
    }

    #[test]
    fn test_cursor_visual_offset_after_wrap() {
        // "<80 x's>abc" at width 80
        // Row 0: 80 x's, Row 1: abc
        // Cursor at col 82 → visual (1, 2)
        let line = format!("{}abc", "x".repeat(80));
        assert_eq!(cursor_visual_offset(&line, 82, 80), (1, 2));
    }

    #[test]
    fn test_cursor_visual_offset_at_end() {
        let line = "hello";
        assert_eq!(cursor_visual_offset(line, 5, 80), (0, 5));
    }

    #[test]
    fn test_wrapped_height_multiline() {
        let mut ta = TextArea::default();
        ta.insert_str("short line");
        ta.insert_newline();
        ta.insert_str("another line");
        // 2 logical lines, both fit in 80 cols
        assert_eq!(wrapped_height(&ta, 80), 2);
    }

    #[test]
    fn test_wrapped_height_long_line() {
        let mut ta = TextArea::default();
        ta.insert_str("x".repeat(200));
        // 1 logical line, 200 chars / 80 = 3 visual lines
        assert_eq!(wrapped_height(&ta, 80), 3);
    }

    #[test]
    fn test_logical_to_visual_first_line() {
        let lines = vec!["hello world".to_string()];
        assert_eq!(logical_to_visual(&lines, 0, 5, 80), (0, 5));
    }

    #[test]
    fn test_logical_to_visual_second_line() {
        let lines = vec!["first".to_string(), "second".to_string()];
        assert_eq!(logical_to_visual(&lines, 1, 3, 80), (1, 3));
    }

    #[test]
    fn test_logical_to_visual_with_wrapped_previous() {
        // First line wraps into 3 visual lines at width 80
        let lines = vec!["x".repeat(200), "hello".to_string()];
        // First line = 3 visual rows, cursor on second line col 3
        assert_eq!(logical_to_visual(&lines, 1, 3, 80), (3, 3));
    }
}
