//! Inline queue preview widget.
//!
//! Shown between the bottom separator and the status bar whenever the
//! `later_queue` is non-empty.  Renders up to [`MAX_VISIBLE`] items with a
//! 1-based index prefix and a truncated text preview, followed by an overflow
//! line and keybinding hints when there are more items.
//!
//! ```text
//!   📋 1  also add unit tests for the new lane
//!   📋 2  then update the docs
//!   + 1 more  ·  ↑ pop  ·  Ctrl+U clear
//! ```
//!
//! The widget is purely presentational — it never mutates state.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Widget,
};

/// Maximum number of queue items rendered as individual rows.
pub const MAX_VISIBLE: usize = 3;

/// The queue preview widget.
///
/// Construct via [`QueuePreview::new`], then pass to [`ratatui::Frame::render_widget`].
pub struct QueuePreview<'a> {
    /// Visible items (already truncated to [`MAX_VISIBLE`]).
    items: &'a [String],
    /// Total queue length (may be > items.len() when there's overflow).
    total: usize,
}

impl<'a> QueuePreview<'a> {
    /// Create a new preview.
    ///
    /// `items` is the slice to render (at most [`MAX_VISIBLE`] entries);
    /// `total` is the full queue length used for the overflow indicator.
    pub fn new(items: &'a [String], total: usize) -> Self {
        Self { items, total }
    }

    /// How many terminal rows this widget will occupy for a given queue length.
    ///
    /// Returns 0 when the queue is empty, so callers can skip the layout slot.
    pub fn height_for(total: usize) -> u16 {
        if total == 0 {
            return 0;
        }
        let item_rows = total.min(MAX_VISIBLE) as u16;
        let hint_row = 1u16; // always show keybinding hints
        item_rows + hint_row
    }
}

impl Widget for QueuePreview<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let max_text_w = area.width.saturating_sub(8) as usize; // prefix = "  📋 N  "

        for (row, item) in self.items.iter().enumerate() {
            let y = area.y + row as u16;
            if y >= area.y + area.height {
                break;
            }

            // Flatten newlines first, then truncate to terminal width
            // with a trailing ellipsis when the text is too long.
            let flat = item.replace('\n', " ");
            let preview: String = if flat.chars().count() > max_text_w {
                let mut s: String = flat.chars().take(max_text_w.saturating_sub(1)).collect();
                s.push('…');
                s
            } else {
                flat
            };

            let line = Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled("\u{1f4cb} ", Style::default().fg(Color::Yellow)),
                Span::styled(
                    format!("{}  ", row + 1),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(preview, Style::default().fg(Color::Gray)),
            ]);
            line.render(Rect::new(area.x, y, area.width, 1), buf);
        }

        // Overflow / hint row
        let overflow = self.total.saturating_sub(MAX_VISIBLE);
        let hint_y = area.y + self.items.len() as u16;
        if overflow > 0 && hint_y < area.y + area.height {
            let line = Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    format!("+ {overflow} more"),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    "  \u{b7}  \u{2191} pop  \u{b7}  Ctrl+U clear",
                    Style::default().fg(Color::Rgb(80, 80, 80)),
                ),
            ]);
            line.render(Rect::new(area.x, hint_y, area.width, 1), buf);
        } else if hint_y < area.y + area.height && self.total > 0 {
            // No overflow — still show the keybinding hints on the last row
            let line = Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    "\u{2191} pop  \u{b7}  Ctrl+U clear",
                    Style::default().fg(Color::Rgb(80, 80, 80)),
                ),
            ]);
            line.render(Rect::new(area.x, hint_y, area.width, 1), buf);
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};

    fn render(items: &[String], total: usize, width: u16) -> Vec<String> {
        let height = QueuePreview::height_for(total).max(1);
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        QueuePreview::new(items, total).render(area, &mut buf);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn height_zero_when_empty() {
        assert_eq!(QueuePreview::height_for(0), 0);
    }

    #[test]
    fn height_one_item_includes_hint_row() {
        // 1 item row + 1 hint row = 2
        assert_eq!(QueuePreview::height_for(1), 2);
    }

    #[test]
    fn height_three_items_includes_hint_row() {
        // 3 item rows + 1 hint row = 4
        assert_eq!(QueuePreview::height_for(3), 4);
    }

    #[test]
    fn height_four_items_capped_plus_hint() {
        // 3 visible + 1 hint = 4
        assert_eq!(QueuePreview::height_for(4), 4);
    }

    #[test]
    fn single_item_renders_text() {
        let items = vec!["add tests".to_string()];
        let rows = render(&items, 1, 60);
        assert!(rows[0].contains("add tests"), "row: {:?}", rows[0]);
        assert!(rows[0].contains('1'), "should show index 1");
    }

    #[test]
    fn overflow_row_shows_count() {
        let items: Vec<String> = (0..MAX_VISIBLE).map(|i| format!("item {i}")).collect();
        let rows = render(&items, MAX_VISIBLE + 2, 60);
        let last = rows.last().unwrap();
        assert!(last.contains("+ 2 more"), "overflow row: {last:?}");
    }

    #[test]
    fn long_text_truncated_with_ellipsis() {
        let long = "x".repeat(200);
        let items = vec![long];
        let rows = render(&items, 1, 40);
        assert!(
            rows[0].ends_with('…'),
            "should end with ellipsis: {:?}",
            rows[0]
        );
    }

    #[test]
    fn multiline_input_flattened() {
        let items = vec!["line1\nline2".to_string()];
        let rows = render(&items, 1, 60);
        assert!(!rows[0].contains('\n'), "newline should be flattened");
    }
}
