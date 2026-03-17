//! Render cache for fullscreen TUI history panel.
//!
//! Stores styled `Line`s in a `VecDeque` and provides virtual scrolling
//! with "sticky bottom" behavior. The buffer is a **render cache** — the
//! DB is the source of truth. Lines evicted from the buffer can be
//! re-rendered from DB on demand.
//!
//! See #472 for the fullscreen migration RFC.

use ratatui::text::Line;
use std::collections::VecDeque;

/// Maximum lines held in the render cache.
/// Enough for ~50 pages of scroll-up at 50 lines/page.
const MAX_CACHE_LINES: usize = 2500;

/// Scrollable buffer of rendered `Line`s with sticky-bottom auto-scroll.
pub struct ScrollBuffer {
    /// The rendered lines (ring buffer).
    lines: VecDeque<Line<'static>>,

    /// Scroll offset: number of lines scrolled UP from the bottom.
    /// 0 = viewing the bottom (latest content).
    scroll_offset: usize,

    /// When true, new lines auto-scroll to keep the bottom visible.
    /// Disengages when the user scrolls up; re-engages when they
    /// scroll back to the bottom.
    sticky_bottom: bool,

    /// Oldest DB message ID currently rendered in the buffer.
    /// Used by virtual scroll to know which page to fetch next.
    /// `None` means no DB messages have been loaded yet.
    oldest_message_id: Option<i64>,
}

impl ScrollBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            lines: VecDeque::with_capacity(capacity.min(4096)),
            scroll_offset: 0,
            sticky_bottom: true,
            oldest_message_id: None,
        }
    }

    /// Append a single line to the buffer.
    ///
    /// If sticky bottom is active, the view stays pinned to the latest
    /// content. If the buffer exceeds the max cache size, the oldest
    /// lines are evicted from the front.
    pub fn push(&mut self, line: Line<'static>) {
        self.lines.push_back(line);
        self.enforce_capacity();

        // If sticky, keep scroll at bottom
        if self.sticky_bottom {
            self.scroll_offset = 0;
        }
    }

    /// Append multiple lines at once.
    pub fn push_lines(&mut self, lines: impl IntoIterator<Item = Line<'static>>) {
        for line in lines {
            self.lines.push_back(line);
        }
        self.enforce_capacity();

        if self.sticky_bottom {
            self.scroll_offset = 0;
        }
    }

    /// Scroll up by `n` visual lines. Disengages sticky bottom.
    pub fn scroll_up(&mut self, n: usize, term_width: usize, viewport_height: usize) {
        let total = self.total_visual_lines(term_width);
        let max_offset = total.saturating_sub(viewport_height);
        self.scroll_offset = (self.scroll_offset + n).min(max_offset);
        self.sticky_bottom = false;
    }

    /// Scroll down by `n` lines. Re-engages sticky bottom if we reach
    /// the bottom.
    pub fn scroll_down(&mut self, n: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
        if self.scroll_offset == 0 {
            self.sticky_bottom = true;
        }
    }

    /// Jump to the bottom and re-engage sticky mode.
    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
        self.sticky_bottom = true;
    }

    /// Jump to the top of the buffer.
    pub fn scroll_to_top(&mut self, term_width: usize, viewport_height: usize) {
        if !self.lines.is_empty() {
            let total = self.total_visual_lines(term_width);
            self.scroll_offset = total.saturating_sub(viewport_height);
            self.sticky_bottom = false;
        }
    }

    /// Returns `true` when the user has scrolled to the very top of the
    /// buffer. Used to trigger loading older messages from the DB.
    #[allow(dead_code)] // wired when virtual scroll pagination lands
    pub fn at_top(&self, term_width: usize, viewport_height: usize) -> bool {
        if self.lines.is_empty() {
            return false;
        }
        let total = self.total_visual_lines(term_width);
        let max_offset = total.saturating_sub(viewport_height);
        self.scroll_offset >= max_offset && max_offset > 0
    }

    /// Return all lines in the buffer.
    ///
    /// Used by `render_history()` which passes everything to
    /// `Paragraph::wrap().scroll()` — ratatui handles the visual
    /// line math for word-wrapped content.
    pub fn all_lines(&self) -> impl Iterator<Item = &Line<'static>> {
        self.lines.iter()
    }

    /// Compute the total number of visual (wrapped) lines at a given
    /// terminal width. Used for scrollbar state and offset clamping.
    pub fn total_visual_lines(&self, term_width: usize) -> usize {
        let w = term_width.max(1);
        self.lines
            .iter()
            .map(|l| visual_height(l, w))
            .sum()
    }

    /// Compute the Paragraph scroll-from-top offset for the current
    /// scroll position. Returns `(row_offset, 0)` for `Paragraph::scroll()`.
    ///
    /// `scroll_offset` is visual lines from the bottom.
    /// Paragraph wants visual lines from the top.
    pub fn paragraph_scroll(&self, viewport_height: usize, term_width: usize) -> (u16, u16) {
        let total = self.total_visual_lines(term_width);
        let from_top = total
            .saturating_sub(viewport_height)
            .saturating_sub(self.scroll_offset);
        (from_top as u16, 0)
    }

    /// Total number of lines in the buffer.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Whether the buffer is empty.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Current scroll offset (lines from bottom).
    pub fn offset(&self) -> usize {
        self.scroll_offset
    }

    /// Whether sticky bottom is active.
    pub fn is_sticky(&self) -> bool {
        self.sticky_bottom
    }

    /// Get the oldest DB message ID rendered in this buffer.
    #[allow(dead_code)] // wired when virtual scroll pagination lands
    pub fn oldest_message_id(&self) -> Option<i64> {
        self.oldest_message_id
    }

    /// Set the oldest DB message ID (called after rendering history).
    pub fn set_oldest_message_id(&mut self, id: i64) {
        self.oldest_message_id = Some(id);
    }

    /// Clear all lines and reset scroll state.
    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.lines.clear();
        self.scroll_offset = 0;
        self.sticky_bottom = true;
    }

    /// Extract the last fenced code block from the buffer.
    ///
    /// Scans backward for ``` fences and returns the content between them.
    /// Used by Ctrl+Y (copy last code block).
    pub fn last_code_block(&self) -> Option<String> {
        let mut end_fence = None;
        let mut start_fence = None;

        // Scan backward through lines
        for (i, line) in self.lines.iter().enumerate().rev() {
            let text = line_text(line);
            let trimmed = text.trim();

            if trimmed == "```" || trimmed.starts_with("```") {
                if end_fence.is_none() {
                    // Found closing fence
                    end_fence = Some(i);
                } else {
                    // Found opening fence
                    start_fence = Some(i);
                    break;
                }
            }
        }

        match (start_fence, end_fence) {
            (Some(start), Some(end)) if start < end => {
                let code: Vec<String> = (start + 1..end)
                    .map(|i| line_text(&self.lines[i]))
                    .collect();
                Some(code.join("\n"))
            }
            _ => None,
        }
    }

    /// Extract the last assistant response from the buffer.
    ///
    /// Scans backward for the response separator ("───") and returns
    /// everything after it. Used by Ctrl+Shift+Y (copy last response).
    pub fn last_response(&self) -> Option<String> {
        let mut sep_idx = None;

        for (i, line) in self.lines.iter().enumerate().rev() {
            let text = line_text(line);
            // Response separator is "  ───" (the ResponseStart line)
            if text.trim().chars().all(|c| c == '─') && text.trim().len() >= 3 {
                sep_idx = Some(i);
                break;
            }
        }

        sep_idx.map(|start| {
            let response: Vec<String> = (start + 1..self.lines.len())
                .map(|i| line_text(&self.lines[i]))
                .collect();
            response.join("\n").trim().to_string()
        })
    }

    /// Evict oldest lines if we exceed capacity.
    fn enforce_capacity(&mut self) {
        while self.lines.len() > MAX_CACHE_LINES {
            self.lines.pop_front();
            // Adjust scroll offset since lines shifted
            if self.scroll_offset > 0 {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
            }
        }
    }

    /// Prepend lines at the top of the buffer (for DB-backed virtual scroll).
    ///
    /// Used when the user scrolls past the top of the cache and older
    /// messages are fetched from the DB and re-rendered.
    #[allow(dead_code)] // wired in a follow-up PR
    pub fn prepend_lines(&mut self, lines: impl IntoIterator<Item = Line<'static>>) {
        let lines: Vec<_> = lines.into_iter().collect();
        let count = lines.len();
        // Push in reverse so they appear in the original order at the front
        for line in lines.into_iter().rev() {
            self.lines.push_front(line);
        }
        // Adjust scroll offset to keep the viewport stable
        // (content shifted down by `count` logical lines)
        self.scroll_offset += count;
        self.enforce_capacity();
    }
}

/// Extract plain text from a `Line` by concatenating all span contents.
fn line_text(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Compute how many visual rows a `Line` occupies at the given terminal width.
///
/// A 200-char line in an 80-column terminal wraps to 3 visual rows.
/// Empty lines always occupy 1 row.
fn visual_height(line: &Line<'_>, term_width: usize) -> usize {
    let w = line.width();
    if w == 0 {
        1
    } else {
        w.div_ceil(term_width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Span;

    const W: usize = 80; // test terminal width
    const H: usize = 50; // test viewport height

    fn make_line(text: &str) -> Line<'static> {
        Line::from(Span::raw(text.to_string()))
    }

    /// Collect the visible lines via paragraph_scroll logic.
    /// For tests with short lines that don't wrap, this matches the old visible_lines().
    fn visible_text(buf: &ScrollBuffer, height: usize) -> Vec<String> {
        let lines: Vec<Line<'_>> = buf.all_lines().cloned().collect();
        let total_visual = buf.total_visual_lines(W);
        let from_top = total_visual
            .saturating_sub(height)
            .saturating_sub(buf.offset());
        // Simulate what Paragraph would show
        lines.iter().skip(from_top).take(height).map(line_text).collect()
    }

    #[test]
    fn test_push_and_visible() {
        let mut buf = ScrollBuffer::new(2500);
        for i in 0..10 {
            buf.push(make_line(&format!("line {i}")));
        }
        assert_eq!(buf.len(), 10);

        // Viewport of 3 lines at bottom
        let visible = visible_text(&buf, 3);
        assert_eq!(visible.len(), 3);
        assert_eq!(visible[0], "line 7");
        assert_eq!(visible[1], "line 8");
        assert_eq!(visible[2], "line 9");
    }

    #[test]
    fn test_sticky_bottom() {
        let mut buf = ScrollBuffer::new(2500);
        for i in 0..5 {
            buf.push(make_line(&format!("line {i}")));
        }
        assert!(buf.is_sticky());
        assert_eq!(buf.offset(), 0);

        // New lines keep us at bottom
        buf.push(make_line("line 5"));
        assert_eq!(buf.offset(), 0);
        let visible = visible_text(&buf, 2);
        assert_eq!(visible[1], "line 5");
    }

    #[test]
    fn test_scroll_up_breaks_sticky() {
        let mut buf = ScrollBuffer::new(2500);
        for i in 0..10 {
            buf.push(make_line(&format!("line {i}")));
        }

        // Use viewport smaller than content so scroll has room
        buf.scroll_up(3, W, 5);
        assert!(!buf.is_sticky());
        assert_eq!(buf.offset(), 3);
    }

    #[test]
    fn test_scroll_down_restores_sticky() {
        let mut buf = ScrollBuffer::new(2500);
        for i in 0..10 {
            buf.push(make_line(&format!("line {i}")));
        }

        buf.scroll_up(5, W, 5);
        assert!(!buf.is_sticky());

        buf.scroll_down(5);
        assert!(buf.is_sticky());
        assert_eq!(buf.offset(), 0);
    }

    #[test]
    fn test_scroll_up_clamped() {
        let mut buf = ScrollBuffer::new(2500);
        for i in 0..5 {
            buf.push(make_line(&format!("line {i}")));
        }

        // Viewport height 3, 5 lines total → max offset = 5-3 = 2
        buf.scroll_up(100, W, 3);
        assert_eq!(buf.offset(), 2);
    }

    #[test]
    fn test_eviction() {
        let mut buf = ScrollBuffer::new(2500);
        for i in 0..MAX_CACHE_LINES + 100 {
            buf.push(make_line(&format!("line {i}")));
        }
        assert_eq!(buf.len(), MAX_CACHE_LINES);
        // Latest line should be at the bottom
        let visible = visible_text(&buf, 1);
        assert_eq!(visible[0], format!("line {}", MAX_CACHE_LINES + 99));
    }

    #[test]
    fn test_empty_buffer() {
        let buf = ScrollBuffer::new(2500);
        assert_eq!(buf.all_lines().count(), 0);
        assert_eq!(buf.total_visual_lines(80), 0);
    }

    #[test]
    fn test_scroll_to_top_and_bottom() {
        let mut buf = ScrollBuffer::new(2500);
        for i in 0..20 {
            buf.push(make_line(&format!("line {i}")));
        }

        // 20 lines, viewport 10 → max offset = 10
        buf.scroll_to_top(W, 10);
        assert!(!buf.is_sticky());
        assert_eq!(buf.offset(), 10);

        buf.scroll_to_bottom();
        assert!(buf.is_sticky());
        assert_eq!(buf.offset(), 0);
    }

    #[test]
    fn test_last_code_block() {
        let mut buf = ScrollBuffer::new(2500);
        buf.push(make_line("some text"));
        buf.push(make_line("```rust"));
        buf.push(make_line("  fn main() {}"));
        buf.push(make_line("  let x = 42;"));
        buf.push(make_line("```"));
        buf.push(make_line("more text"));

        let code = buf.last_code_block().unwrap();
        assert_eq!(code, "  fn main() {}\n  let x = 42;");
    }

    #[test]
    fn test_last_code_block_none() {
        let mut buf = ScrollBuffer::new(2500);
        buf.push(make_line("no code here"));
        assert!(buf.last_code_block().is_none());
    }

    #[test]
    fn test_last_response() {
        let mut buf = ScrollBuffer::new(2500);
        buf.push(make_line("user message"));
        buf.push(make_line("  ───"));
        buf.push(make_line("  response line 1"));
        buf.push(make_line("  response line 2"));

        let response = buf.last_response().unwrap();
        assert!(response.contains("response line 1"));
        assert!(response.contains("response line 2"));
    }

    #[test]
    fn test_push_lines_batch() {
        let mut buf = ScrollBuffer::new(2500);
        let batch: Vec<Line<'static>> = (0..5).map(|i| make_line(&format!("line {i}"))).collect();
        buf.push_lines(batch);
        assert_eq!(buf.len(), 5);
    }

    #[test]
    fn test_eviction_adjusts_scroll_offset() {
        let mut buf = ScrollBuffer::new(2500);
        // Fill to capacity
        for i in 0..MAX_CACHE_LINES {
            buf.push(make_line(&format!("line {i}")));
        }
        // Scroll up
        buf.scroll_up(100, W, H);
        let offset_before = buf.offset();

        // Push more lines, triggering eviction
        for i in 0..50 {
            buf.push(make_line(&format!("new {i}")));
        }

        // Offset should have been adjusted down
        assert!(buf.offset() < offset_before);
        assert_eq!(buf.len(), MAX_CACHE_LINES);
    }

    // ── Visual line math ──

    #[test]
    fn test_visual_height_short_line() {
        let line = make_line("hello"); // 5 chars
        assert_eq!(visual_height(&line, 80), 1);
    }

    #[test]
    fn test_visual_height_wrapping_line() {
        // 160 chars in an 80-column terminal = 2 visual lines
        let line = make_line(&"x".repeat(160));
        assert_eq!(visual_height(&line, 80), 2);
    }

    #[test]
    fn test_visual_height_empty_line() {
        let line = make_line("");
        assert_eq!(visual_height(&line, 80), 1);
    }

    #[test]
    fn test_total_visual_lines() {
        let mut buf = ScrollBuffer::new(2500);
        buf.push(make_line("short")); // 1 visual line
        buf.push(make_line(&"x".repeat(160))); // 2 visual lines
        buf.push(make_line("")); // 1 visual line
        assert_eq!(buf.total_visual_lines(80), 4);
    }

    #[test]
    fn test_paragraph_scroll_at_bottom() {
        let mut buf = ScrollBuffer::new(2500);
        for i in 0..20 {
            buf.push(make_line(&format!("line {i}")));
        }
        // At bottom: offset=0, viewport=10, total=20
        // → scroll from top = 20 - 10 - 0 = 10
        let (row, _) = buf.paragraph_scroll(10, 80);
        assert_eq!(row, 10);
    }

    #[test]
    fn test_paragraph_scroll_at_top() {
        let mut buf = ScrollBuffer::new(2500);
        for i in 0..20 {
            buf.push(make_line(&format!("line {i}")));
        }
        buf.scroll_to_top(80, 10);
        // At top: offset=10, viewport=10, total=20
        // → scroll from top = 20 - 10 - 10 = 0
        let (row, _) = buf.paragraph_scroll(10, 80);
        assert_eq!(row, 0);
    }

    // ── Prepend ──

    #[test]
    fn test_prepend_lines() {
        let mut buf = ScrollBuffer::new(2500);
        buf.push(make_line("current"));
        buf.scroll_up(0, W, H); // stay at bottom

        let old_lines = vec![make_line("old1"), make_line("old2")];
        buf.prepend_lines(old_lines);

        assert_eq!(buf.len(), 3);
        // Offset adjusted by prepend count
        assert_eq!(buf.offset(), 2);
        // First line is now "old1"
        let first = line_text(buf.all_lines().next().unwrap());
        assert_eq!(first, "old1");
    }
}
