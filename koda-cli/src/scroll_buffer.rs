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
}

impl ScrollBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            lines: VecDeque::with_capacity(capacity.min(4096)),
            scroll_offset: 0,
            sticky_bottom: true,
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

    /// Scroll up by `n` lines. Disengages sticky bottom.
    pub fn scroll_up(&mut self, n: usize) {
        let max_offset = self.lines.len().saturating_sub(1);
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
    pub fn scroll_to_top(&mut self) {
        if !self.lines.is_empty() {
            self.scroll_offset = self.lines.len().saturating_sub(1);
            self.sticky_bottom = false;
        }
    }

    /// Return the slice of lines visible in a viewport of `height` rows.
    ///
    /// Lines are returned bottom-up: the last element is the bottommost
    /// visible line. The caller renders them top-to-bottom.
    pub fn visible_lines(&self, height: usize) -> Vec<&Line<'static>> {
        if self.lines.is_empty() || height == 0 {
            return Vec::new();
        }

        let total = self.lines.len();
        // Bottom of visible window (exclusive)
        let bottom = total.saturating_sub(self.scroll_offset);
        // Top of visible window (inclusive)
        let top = bottom.saturating_sub(height);

        (top..bottom).map(|i| &self.lines[i]).collect()
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
}

/// Extract plain text from a `Line` by concatenating all span contents.
fn line_text(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Span;

    fn make_line(text: &str) -> Line<'static> {
        Line::from(Span::raw(text.to_string()))
    }

    #[test]
    fn test_push_and_visible() {
        let mut buf = ScrollBuffer::new(2500);
        for i in 0..10 {
            buf.push(make_line(&format!("line {i}")));
        }
        assert_eq!(buf.len(), 10);

        // Viewport of 3 lines at bottom
        let visible = buf.visible_lines(3);
        assert_eq!(visible.len(), 3);
        assert_eq!(line_text(visible[0]), "line 7");
        assert_eq!(line_text(visible[1]), "line 8");
        assert_eq!(line_text(visible[2]), "line 9");
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
        let visible = buf.visible_lines(2);
        assert_eq!(line_text(visible[1]), "line 5");
    }

    #[test]
    fn test_scroll_up_breaks_sticky() {
        let mut buf = ScrollBuffer::new(2500);
        for i in 0..10 {
            buf.push(make_line(&format!("line {i}")));
        }

        buf.scroll_up(3);
        assert!(!buf.is_sticky());
        assert_eq!(buf.offset(), 3);

        let visible = buf.visible_lines(3);
        assert_eq!(line_text(visible[0]), "line 4");
        assert_eq!(line_text(visible[2]), "line 6");
    }

    #[test]
    fn test_scroll_down_restores_sticky() {
        let mut buf = ScrollBuffer::new(2500);
        for i in 0..10 {
            buf.push(make_line(&format!("line {i}")));
        }

        buf.scroll_up(5);
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

        buf.scroll_up(100);
        assert_eq!(buf.offset(), 4); // max is len - 1
    }

    #[test]
    fn test_eviction() {
        let mut buf = ScrollBuffer::new(2500);
        for i in 0..MAX_CACHE_LINES + 100 {
            buf.push(make_line(&format!("line {i}")));
        }
        assert_eq!(buf.len(), MAX_CACHE_LINES);
        // Oldest lines should be evicted
        let visible = buf.visible_lines(1);
        assert_eq!(
            line_text(visible[0]),
            format!("line {}", MAX_CACHE_LINES + 99)
        );
    }

    #[test]
    fn test_visible_empty_buffer() {
        let buf = ScrollBuffer::new(2500);
        assert!(buf.visible_lines(10).is_empty());
    }

    #[test]
    fn test_visible_height_larger_than_buffer() {
        let mut buf = ScrollBuffer::new(2500);
        buf.push(make_line("only line"));
        let visible = buf.visible_lines(100);
        assert_eq!(visible.len(), 1);
    }

    #[test]
    fn test_scroll_to_top_and_bottom() {
        let mut buf = ScrollBuffer::new(2500);
        for i in 0..20 {
            buf.push(make_line(&format!("line {i}")));
        }

        buf.scroll_to_top();
        assert!(!buf.is_sticky());
        assert_eq!(buf.offset(), 19);

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
        buf.scroll_up(100);
        let offset_before = buf.offset();

        // Push more lines, triggering eviction
        for i in 0..50 {
            buf.push(make_line(&format!("new {i}")));
        }

        // Offset should have been adjusted down
        assert!(buf.offset() < offset_before);
        assert_eq!(buf.len(), MAX_CACHE_LINES);
    }
}
