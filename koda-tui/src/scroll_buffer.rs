//! Render cache for fullscreen TUI history panel.
//!
//! Stores styled `Line`s in a `VecDeque` and provides virtual scrolling
//! with "sticky bottom" behavior. The buffer is a **render cache** — the
//! DB is the source of truth. Lines evicted from the buffer can be
//! re-rendered from DB on demand.
//!
//! See #472 for the fullscreen migration RFC.

use ratatui::text::Line;
use std::collections::{HashSet, VecDeque};

/// Maximum lines held in the render cache, folded-away lines included.
/// A full read keeps its content here for the full view, so this is set
/// for a session's worth of those, not just ~50 pages of what's shown.
const MAX_CACHE_LINES: usize = 10_000;

/// Which view a line belongs to.
///
/// A turn's steps are folded by default: a lookup row, a `Thought for` line,
/// a "… +N lines" marker. The full view (Ctrl+O) shows every line instead.
/// Both live in the buffer at once, each line tagged, so switching is a
/// redraw, not a re-render. One item (a row, a thought, a call's output) can
/// also be opened on its own in the folded view ([`ScrollBuffer::toggle`]),
/// which shows its [`Fold::Full`] lines there too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fold {
    /// Shown in both views: everything that isn't folded.
    #[default]
    Always,
    /// Folded view only, and gone when its item is opened: a summary or a
    /// "… +N lines" marker, which the opened lines replace.
    Folded,
    /// Folded view only, and kept when its item is opened: a lookup row or
    /// a `Thought for` line, which heads the lines it opens to.
    Heading,
    /// Full view only, or its item opened: the lines folding hides.
    Full,
}

/// One rendered line and how to show it.
struct Entry {
    line: Line<'static>,
    /// Columns to skip during NoSelect copy. 0 = no gutter (normal text).
    gutter: u16,
    fold: Fold,
    /// The item it belongs to ([`ScrollBuffer::new_item`]), which opens and
    /// closes as one.
    item: Option<u64>,
}

/// Scrollable buffer of rendered `Line`s with sticky-bottom auto-scroll.
pub struct ScrollBuffer {
    /// The rendered lines (ring buffer).
    entries: VecDeque<Entry>,

    /// Sequence number of `entries[0]`: each pushed line gets the next one,
    /// so a caller can find lines it pushed earlier after older ones were
    /// evicted ([`Self::next_seq`], [`Self::fold_since`]).
    first_seq: u64,

    /// The full view is on: [`Fold::Full`] lines are shown instead of
    /// [`Fold::Folded`] ones.
    full: bool,

    /// Scroll offset: number of lines scrolled UP from the bottom.
    /// 0 = viewing the bottom (latest content).
    scroll_offset: usize,

    /// When true, new lines auto-scroll to keep the bottom visible.
    /// Disengages when the user scrolls up; re-engages when they
    /// scroll back to the bottom.
    sticky_bottom: bool,

    /// Cached terminal width for eviction offset adjustment.
    /// Updated on scroll operations and used by `enforce_capacity()`
    /// to compute visual height of evicted lines.
    cached_term_width: usize,

    /// The last key [`Self::new_item`] gave out.
    last_item: u64,
    /// Items with lines the folded view hides: the ones worth opening.
    foldable: HashSet<u64>,
    /// Items opened in the folded view.
    opened: HashSet<u64>,
    /// The item the keyboard is on, in focus mode (F4).
    focus: Option<u64>,
}

impl ScrollBuffer {
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.min(4096);
        Self {
            entries: VecDeque::with_capacity(cap),
            first_seq: 0,
            full: false,
            scroll_offset: 0,
            sticky_bottom: true,
            cached_term_width: 80,
            last_item: 0,
            foldable: HashSet::new(),
            opened: HashSet::new(),
            focus: None,
        }
    }

    fn shows(&self, entry: &Entry) -> bool {
        if self.full {
            return matches!(entry.fold, Fold::Always | Fold::Full);
        }
        let open = entry.item.is_some_and(|item| self.opened.contains(&item));
        match entry.fold {
            Fold::Always | Fold::Heading => true,
            Fold::Folded => !open,
            Fold::Full => open,
        }
    }

    fn visible(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| self.shows(e))
    }

    fn push_entry(&mut self, entry: Entry) {
        if entry.fold == Fold::Full
            && let Some(item) = entry.item
        {
            self.foldable.insert(item);
        }
        self.entries.push_back(entry);
        self.enforce_capacity();
        // If sticky, keep scroll at bottom
        if self.sticky_bottom {
            self.scroll_offset = 0;
        }
    }

    /// Append a single line to the buffer.
    ///
    /// If sticky bottom is active, the view stays pinned to the latest
    /// content. If the buffer exceeds the max cache size, the oldest
    /// lines are evicted from the front.
    pub fn push(&mut self, line: Line<'static>) {
        self.push_folded(line, Fold::Always);
    }

    /// Append a line that only one of the two views shows.
    pub fn push_folded(&mut self, line: Line<'static>, fold: Fold) {
        self.push_in(line, fold, None);
    }

    /// Append a line that belongs to `item`, which opens and closes as one.
    pub fn push_in(&mut self, line: Line<'static>, fold: Fold, item: Option<u64>) {
        self.push_entry(Entry {
            line,
            gutter: 0,
            fold,
            item,
        });
    }

    /// Append a line with a gutter width (for NoSelect diff lines).
    pub fn push_with_gutter(&mut self, line: Line<'static>, gutter_width: u16) {
        self.push_entry(Entry {
            line,
            gutter: gutter_width,
            fold: Fold::Always,
            item: None,
        });
    }

    /// Append multiple lines at once.
    pub fn push_lines(&mut self, lines: impl IntoIterator<Item = Line<'static>>) {
        self.push_folded_lines(lines.into_iter().map(|l| (l, Fold::Always)));
    }

    /// Append lines, each with the view it belongs to.
    pub fn push_folded_lines(&mut self, lines: impl IntoIterator<Item = (Line<'static>, Fold)>) {
        self.push_folded_lines_in(lines, None);
    }

    /// Append lines, each with the view it belongs to, all in `item`.
    pub fn push_folded_lines_in(
        &mut self,
        lines: impl IntoIterator<Item = (Line<'static>, Fold)>,
        item: Option<u64>,
    ) {
        for (line, fold) in lines {
            if fold == Fold::Full
                && let Some(item) = item
            {
                self.foldable.insert(item);
            }
            self.entries.push_back(Entry {
                line,
                gutter: 0,
                fold,
                item,
            });
        }
        self.enforce_capacity();

        if self.sticky_bottom {
            self.scroll_offset = 0;
        }
    }

    /// Sequence number the next pushed line will get.
    pub fn next_seq(&self) -> u64 {
        self.first_seq + self.entries.len() as u64
    }

    /// Fold the lines pushed since `start` (a [`Self::next_seq`] value) the
    /// way finished tool output is folded: keep the first `head` and last
    /// `tail` in both views, and show the rest only in the full view, with
    /// `marker(hidden)` in their place in the folded one, all in `item`.
    /// For output that streamed in line by line before anyone knew how long
    /// it would be. Does nothing when no more than one line would be hidden.
    /// Only lines both views show are counted and folded: a line one view
    /// already keeps to itself, such as a row another call pushed meanwhile,
    /// stays as it is.
    pub fn fold_since(
        &mut self,
        start: u64,
        head: usize,
        tail: usize,
        marker: impl FnOnce(usize) -> Line<'static>,
        item: Option<u64>,
    ) {
        let from = start.saturating_sub(self.first_seq) as usize;
        let shown: Vec<usize> = (from..self.entries.len())
            .filter(|&i| self.entries[i].fold == Fold::Always)
            .collect();
        let count = shown.len();
        if count <= head + tail + 1 {
            return;
        }
        let hidden = count - head - tail;
        for &i in &shown[head..head + hidden] {
            self.entries[i].fold = Fold::Full;
            self.entries[i].item = item;
        }
        if let Some(item) = item {
            self.foldable.insert(item);
        }
        self.entries.insert(
            shown[head],
            Entry {
                line: marker(hidden),
                gutter: 0,
                fold: Fold::Folded,
                item,
            },
        );
        if self.sticky_bottom {
            self.scroll_offset = 0;
        }
    }

    /// A new item's key, for lines that open and close as one.
    pub fn new_item(&mut self) -> u64 {
        self.last_item += 1;
        self.last_item
    }

    /// Append a heading that can be redrawn later with [`Self::set_row`]: a
    /// row that sums up calls still running. Returns its item, which its
    /// calls' lines join.
    pub fn push_row(&mut self, line: Line<'static>) -> u64 {
        let item = self.new_item();
        self.push_in(line, Fold::Heading, Some(item));
        item
    }

    /// Redraw the heading of `item`. Does nothing once it has been evicted.
    /// Rows are recent, so the search starts at the end.
    pub fn set_row(&mut self, item: u64, line: Line<'static>) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .rev()
            .find(|e| e.item == Some(item) && e.fold == Fold::Heading)
        {
            entry.line = line;
        }
    }

    /// Move the lines of item `from` into item `to`: a thought that turned
    /// out to be inside a lookup row opens with it.
    pub fn retag(&mut self, from: u64, to: u64) {
        for entry in self.entries.iter_mut().rev() {
            if entry.item == Some(from) {
                entry.item = Some(to);
            }
        }
        if self.foldable.remove(&from) {
            self.foldable.insert(to);
        }
    }

    // ── Opening one item ─────────────────────────────────

    /// Whether `item` is opened in the folded view.
    pub fn is_open(&self, item: u64) -> bool {
        self.opened.contains(&item)
    }

    /// Open `item` in the folded view, or close it, keeping the rows above
    /// it where they were on screen. Only an item with something folded
    /// away opens; nothing happens in the full view, which shows it all.
    pub fn set_open(&mut self, item: u64, open: bool, term_width: usize, viewport_height: usize) {
        if self.full || !self.foldable.contains(&item) || self.is_open(item) == open {
            return;
        }
        let from_top = self.top_row(term_width, viewport_height);
        if open {
            self.opened.insert(item);
        } else {
            self.opened.remove(&item);
        }
        self.set_from_top(from_top, term_width, viewport_height);
    }

    /// Open `item` if it is closed, close it if it is open.
    pub fn toggle(&mut self, item: u64, term_width: usize, viewport_height: usize) {
        let open = !self.is_open(item);
        self.set_open(item, open, term_width, viewport_height);
    }

    /// The item at visual row `row` of the history panel, counted from its
    /// top, if that line belongs to one worth opening.
    pub fn item_at(&self, row: usize, term_width: usize, viewport_height: usize) -> Option<u64> {
        let target = self.top_row(term_width, viewport_height) + row;
        let w = term_width.max(1);
        let mut at = 0;
        for entry in self.visible() {
            let height = visual_height(&entry.line, w);
            if target < at + height {
                return entry.item.filter(|item| self.foldable.contains(item));
            }
            at += height;
        }
        None
    }

    // ── Focus mode (F4) ──────────────────────────────────

    /// The item the keyboard is on, in focus mode.
    pub fn focus(&self) -> Option<u64> {
        self.focus.filter(|_| !self.full)
    }

    /// Leave focus mode.
    pub fn clear_focus(&mut self) {
        self.focus = None;
    }

    /// Put the focus on `item`.
    pub fn set_focus(&mut self, item: u64) {
        self.focus = Some(item);
    }

    /// The items worth opening, in the order they are shown.
    fn foldable_items(&self) -> Vec<u64> {
        let mut items: Vec<u64> = Vec::new();
        for item in self.visible().filter_map(|e| e.item) {
            if self.foldable.contains(&item) && !items.contains(&item) {
                items.push(item);
            }
        }
        items
    }

    /// Enter focus mode on the last item worth opening that is on screen,
    /// or the last one there is, as Codex's F4 does. Returns whether there
    /// was one.
    pub fn focus_last(&mut self, term_width: usize, viewport_height: usize) -> bool {
        if self.full {
            return false;
        }
        let items = self.foldable_items();
        let top = self.top_row(term_width, viewport_height);
        let on_screen = items.iter().rev().find(|&&item| {
            self.row_of(item, term_width)
                .is_some_and(|row| row >= top && row < top + viewport_height)
        });
        self.focus = on_screen.or(items.last()).copied();
        if let Some(item) = self.focus {
            self.show_item(item, term_width, viewport_height);
        }
        self.focus.is_some()
    }

    /// Move the focus `by` items, up when negative, and bring it on screen.
    pub fn move_focus(&mut self, by: isize, term_width: usize, viewport_height: usize) {
        let items = self.foldable_items();
        let Some(at) = self.focus.and_then(|f| items.iter().position(|&i| i == f)) else {
            return;
        };
        let next = (at as isize + by).clamp(0, items.len() as isize - 1) as usize;
        self.focus = Some(items[next]);
        self.show_item(items[next], term_width, viewport_height);
    }

    /// The visual row of the first line of `item` the view shows, from the
    /// top of the buffer.
    fn row_of(&self, item: u64, term_width: usize) -> Option<usize> {
        let w = term_width.max(1);
        let mut at = 0;
        for entry in self.visible() {
            if entry.item == Some(item) {
                return Some(at);
            }
            at += visual_height(&entry.line, w);
        }
        None
    }

    /// Scroll so the start of `item` is on screen, if it isn't.
    fn show_item(&mut self, item: u64, term_width: usize, viewport_height: usize) {
        let Some(row) = self.row_of(item, term_width) else {
            return;
        };
        let top = self.top_row(term_width, viewport_height);
        if row < top || row >= top + viewport_height {
            let centred = row.saturating_sub(viewport_height / 2);
            self.set_from_top(centred, term_width, viewport_height);
        }
    }

    /// The visual row at the top of the history panel.
    fn top_row(&self, term_width: usize, viewport_height: usize) -> usize {
        self.total_visual_lines(term_width)
            .saturating_sub(viewport_height)
            .saturating_sub(self.scroll_offset)
    }

    /// Scroll so visual row `from_top` is at the top of the panel, as far as
    /// the buffer allows.
    fn set_from_top(&mut self, from_top: usize, term_width: usize, viewport_height: usize) {
        self.cached_term_width = term_width;
        let max = self
            .total_visual_lines(term_width)
            .saturating_sub(viewport_height);
        self.scroll_offset = max.saturating_sub(from_top.min(max));
        self.sticky_bottom = self.scroll_offset == 0;
    }

    /// The lines the view shows, each with whether it belongs to the
    /// focused item, for the panel to mark.
    pub fn focused_flags(&self) -> impl Iterator<Item = bool> + '_ {
        let focus = self.focus();
        self.visible()
            .map(move |e| focus.is_some() && e.item == focus)
    }

    /// Whether the full view is on.
    pub fn is_full(&self) -> bool {
        self.full
    }

    /// Switch between the folded and the full view. The view goes to the
    /// bottom: what was on screen may have moved by any amount.
    pub fn set_full(&mut self, full: bool) {
        if self.full != full {
            self.full = full;
            self.scroll_to_bottom();
        }
    }

    /// Scroll up by `n` visual lines. Disengages sticky bottom.
    pub fn scroll_up(&mut self, n: usize, term_width: usize, viewport_height: usize) {
        self.cached_term_width = term_width;
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

    /// Re-clamp `scroll_offset` to valid bounds for the current
    /// terminal dimensions. Must be called after any resize so the
    /// offset doesn't exceed `total_visual - viewport_height`.
    pub fn clamp_offset(&mut self, term_width: usize, viewport_height: usize) {
        self.cached_term_width = term_width;
        let total = self.total_visual_lines(term_width);
        let max_offset = total.saturating_sub(viewport_height);
        if self.scroll_offset > max_offset {
            self.scroll_offset = max_offset;
        }
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
        self.cached_term_width = term_width;
        if !self.entries.is_empty() {
            let total = self.total_visual_lines(term_width);
            self.scroll_offset = total.saturating_sub(viewport_height);
            self.sticky_bottom = false;
        }
    }

    /// Returns `true` when the user has scrolled to the very top of the
    /// buffer. Used to trigger loading older messages from the DB.
    #[allow(dead_code)] // wired when virtual scroll pagination lands
    pub fn at_top(&self, term_width: usize, viewport_height: usize) -> bool {
        if self.entries.is_empty() {
            return false;
        }
        let total = self.total_visual_lines(term_width);
        let max_offset = total.saturating_sub(viewport_height);
        self.scroll_offset >= max_offset && max_offset > 0
    }

    /// Return the lines the current view shows.
    ///
    /// Used by `render_history()` which passes everything to
    /// `Paragraph::wrap().scroll()` — ratatui handles the visual
    /// line math for word-wrapped content.
    pub fn all_lines(&self) -> impl Iterator<Item = &Line<'static>> {
        self.visible().map(|e| &e.line)
    }

    /// Return the per-line gutter widths (parallel with `all_lines()`).
    ///
    /// Used by mouse selection to skip gutter columns during copy.
    pub fn gutter_widths(&self) -> impl Iterator<Item = u16> + '_ {
        self.visible().map(|e| e.gutter)
    }

    /// Compute the total number of visual (wrapped) lines at a given
    /// terminal width. Used for scrollbar state and offset clamping.
    pub fn total_visual_lines(&self, term_width: usize) -> usize {
        let w = term_width.max(1);
        self.visible().map(|e| visual_height(&e.line, w)).sum()
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
        // Clamp to u16::MAX to prevent silent truncation (#528).
        (from_top.min(u16::MAX as usize) as u16, 0)
    }

    /// Total number of lines in the buffer, both views' included.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the buffer is empty.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
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
        self.first_seq += self.entries.len() as u64;
        self.entries.clear();
        self.scroll_offset = 0;
        self.sticky_bottom = true;
    }

    /// Evict oldest lines if we exceed capacity.
    ///
    /// Adjusts `scroll_offset` by the evicted line's visual height
    /// (not a flat 1) to prevent scroll position drift when wrapped
    /// lines are evicted (#528). A line the current view hides takes up
    /// no rows, so it moves nothing.
    fn enforce_capacity(&mut self) {
        let w = self.cached_term_width.max(1);
        while self.entries.len() > MAX_CACHE_LINES {
            if let Some(evicted) = self.entries.pop_front() {
                self.first_seq += 1;
                if self.scroll_offset > 0 && self.shows(&evicted) {
                    let vis = visual_height(&evicted.line, w);
                    self.scroll_offset = self.scroll_offset.saturating_sub(vis);
                }
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
        for line in lines.into_iter().rev() {
            self.entries.push_front(Entry {
                line,
                gutter: 0,
                fold: Fold::Always,
                item: None,
            });
        }
        self.first_seq = self.first_seq.saturating_sub(count as u64);
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
/// Delegates to `wrap_util::visual_line_count` — the single source of truth
/// for word-boundary wrapping consistent with `Wrap { trim: false }`.
fn visual_height(line: &Line<'_>, term_width: usize) -> usize {
    crate::wrap_util::visual_line_count(&line_text(line), term_width)
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
        lines
            .iter()
            .skip(from_top)
            .take(height)
            .map(line_text)
            .collect()
    }

    #[test]
    fn the_full_view_swaps_folded_lines_for_full_ones() {
        let mut buf = ScrollBuffer::new(100);
        buf.push(make_line("header"));
        buf.push_folded(make_line("summary"), Fold::Folded);
        buf.push_folded(make_line("detail"), Fold::Full);
        buf.push(make_line("after"));
        assert_eq!(visible_text(&buf, H), vec!["header", "summary", "after"]);
        buf.set_full(true);
        assert_eq!(visible_text(&buf, H), vec!["header", "detail", "after"]);
        assert_eq!(buf.gutter_widths().count(), 3);
        assert_eq!(buf.len(), 4);
    }

    /// A row is redrawn by its key, in its view, even after a fold put a
    /// marker in before it.
    #[test]
    fn a_row_is_redrawn_in_place_by_its_key() {
        let mut buf = ScrollBuffer::new(100);
        let start = buf.next_seq();
        for i in 1..=10 {
            buf.push(make_line(&format!("l{i}")));
        }
        let row = buf.push_row(make_line("Reading 1 file"));
        buf.push_folded(make_line("its output"), Fold::Full);
        // The streamed output folds; the row, folded view only, stays put.
        buf.fold_since(start, 2, 0, |n| make_line(&format!("+{n}")), None);
        buf.set_row(row, make_line("Read 2 files"));
        assert_eq!(
            visible_text(&buf, H),
            vec!["l1", "l2", "+8", "Read 2 files"]
        );
        buf.set_full(true);
        assert!(!visible_text(&buf, H).contains(&"Read 2 files".to_string()));
    }

    /// One item opens in the folded view: a heading keeps its place with its
    /// lines under it, a marker gives way to the lines it stood for. The
    /// rows above stay where they were on screen.
    #[test]
    fn one_item_opens_and_closes_on_its_own() {
        let mut buf = ScrollBuffer::new(100);
        for i in 1..=20 {
            buf.push(make_line(&format!("l{i}")));
        }
        let row = buf.push_row(make_line("Read 2 files"));
        buf.push_in(make_line("a.rs"), Fold::Full, Some(row));
        buf.push_in(make_line("b.rs"), Fold::Full, Some(row));
        let out = buf.new_item();
        buf.push_in(make_line("+3 lines"), Fold::Folded, Some(out));
        buf.push_in(make_line("hidden"), Fold::Full, Some(out));
        // At the bottom, "Read 2 files" is the top row.
        assert_eq!(visible_text(&buf, 2), vec!["Read 2 files", "+3 lines"]);

        buf.toggle(row, W, 2);
        assert!(buf.is_open(row));
        assert_eq!(visible_text(&buf, 2), vec!["Read 2 files", "a.rs"]);
        buf.scroll_to_bottom();
        assert_eq!(visible_text(&buf, 2), vec!["b.rs", "+3 lines"]);

        buf.toggle(out, W, 2);
        assert_eq!(visible_text(&buf, 2), vec!["b.rs", "hidden"]);
        buf.toggle(row, W, 2);
        buf.toggle(out, W, 2);
        assert_eq!(visible_text(&buf, 2), vec!["Read 2 files", "+3 lines"]);

        // A line with nothing folded away isn't an item to open.
        let plain = buf.new_item();
        buf.push_in(make_line("plain"), Fold::Always, Some(plain));
        buf.toggle(plain, W, 2);
        assert!(!buf.is_open(plain));
        assert_eq!(buf.item_at(1, W, 2), None);
        assert_eq!(buf.item_at(0, W, 2), Some(out));

        // The full view shows everything: nothing to open there.
        buf.set_full(true);
        buf.toggle(row, W, 2);
        assert!(!buf.is_open(row));
    }

    /// F4 lands on the last item on screen; arrows move between items.
    #[test]
    fn focus_moves_between_items() {
        let mut buf = ScrollBuffer::new(100);
        let mut rows = Vec::new();
        for i in 1..=3 {
            let row = buf.push_row(make_line(&format!("row {i}")));
            buf.push_in(make_line(&format!("detail {i}")), Fold::Full, Some(row));
            buf.push(make_line("text"));
            rows.push(row);
        }
        assert!(buf.focus_last(W, H));
        assert_eq!(buf.focus(), Some(rows[2]));
        buf.move_focus(-1, W, H);
        assert_eq!(buf.focus(), Some(rows[1]));
        buf.move_focus(-5, W, H);
        assert_eq!(buf.focus(), Some(rows[0]));
        let flags: Vec<bool> = buf.focused_flags().collect();
        assert_eq!(flags, vec![true, false, false, false, false, false]);
        buf.clear_focus();
        assert_eq!(buf.focus(), None);
    }

    #[test]
    fn fold_since_folds_the_lines_pushed_after_a_point() {
        let mut buf = ScrollBuffer::new(100);
        buf.push(make_line("before"));
        let start = buf.next_seq();
        for i in 1..=10 {
            buf.push(make_line(&format!("l{i}")));
        }
        buf.fold_since(start, 2, 1, |n| make_line(&format!("+{n}")), None);
        assert_eq!(
            visible_text(&buf, H),
            vec!["before", "l1", "l2", "+7", "l10"]
        );
        buf.set_full(true);
        assert_eq!(visible_text(&buf, H).len(), 11);

        // Too few lines to be worth a marker: left alone.
        let mut buf = ScrollBuffer::new(100);
        let start = buf.next_seq();
        for i in 1..=3 {
            buf.push(make_line(&format!("l{i}")));
        }
        buf.fold_since(start, 2, 0, |n| make_line(&format!("+{n}")), None);
        assert_eq!(visible_text(&buf, H), vec!["l1", "l2", "l3"]);
    }

    #[test]
    fn switching_views_returns_to_the_bottom() {
        let mut buf = ScrollBuffer::new(100);
        for i in 0..100 {
            buf.push(make_line(&format!("line {i}")));
        }
        buf.scroll_up(30, W, H);
        assert!(!buf.is_sticky());
        buf.set_full(true);
        assert!(buf.is_sticky());
        assert_eq!(buf.offset(), 0);
    }

    #[test]
    fn evicting_a_hidden_line_moves_nothing_on_screen() {
        let mut buf = ScrollBuffer::new(100);
        buf.push_folded(make_line("hidden"), Fold::Full);
        for i in 0..(MAX_CACHE_LINES - 1) {
            buf.push(make_line(&format!("line {i}")));
        }
        buf.scroll_up(10, W, H);
        let before = buf.offset();
        buf.push(make_line("one more"));
        assert_eq!(buf.offset(), before);
        assert_eq!(buf.len(), MAX_CACHE_LINES);
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

    // ── Word-wrap regression tests (#520) ──

    #[test]
    fn test_visual_height_word_wrap_breaks_before_word() {
        // 76 chars of 'a' + space + 6-char word = 83 chars
        // Character wrap: row 1 = 80 chars, row 2 = 3 chars = 2 rows
        // Word wrap: row 1 = 76 chars + space (77), row 2 = 6-char word = 2 rows
        let text = format!("{}  foobar", "a".repeat(76));
        let line = make_line(&text);
        assert_eq!(visual_height(&line, 80), 2);
    }

    #[test]
    fn test_visual_height_word_wrap_longer_word() {
        // 78 chars + space + 5-char word = 84 chars
        // Character wrap: row 1 = 80 chars, row 2 = 4 chars = 2 rows
        // Word wrap: row 1 = 78+space (79), word "hello" doesn't fit (79+5=84>80)
        //   -> wrap before "hello": row 1 = 79, row 2 = 5 = 2 rows
        let text = format!("{} hello", "x".repeat(78));
        let line = make_line(&text);
        assert_eq!(visual_height(&line, 80), 2);
    }

    #[test]
    fn test_visual_height_word_wrap_three_rows() {
        // Two long words that each nearly fill a row + a short word
        // "<75 a's> <75 b's> end"
        let text = format!("{} {} end", "a".repeat(75), "b".repeat(75));
        let line = make_line(&text);
        // Row 1: 75 a's + space (76), "b"*75 doesn't fit (76+75=151>80)
        // Row 2: 75 b's + space (76), "end" fits (76+3=79<=80) -> actually...
        // Wait: row 2 starts with 75 b's = 75, + space = 76, + "end" = 79 <= 80
        // So: 2 rows
        // Actually: row 1 = "aaa... " (76), b's don't fit, wrap
        // row 2 = "bbb... end" (75+1+3=79)
        // = 2 rows
        assert_eq!(visual_height(&line, 80), 2);
    }

    #[test]
    fn test_visual_height_single_word_longer_than_width() {
        // Single word with no spaces, 200 chars at width 80
        // Must force-break: ceil(200/80) = 3 rows
        let line = make_line(&"x".repeat(200));
        assert_eq!(visual_height(&line, 80), 3);
    }

    #[test]
    fn test_visual_height_exact_width() {
        // Exactly 80 chars = 1 row, not 2
        let line = make_line(&"x".repeat(80));
        assert_eq!(visual_height(&line, 80), 1);
    }

    #[test]
    fn test_visual_height_exact_width_plus_one() {
        let line = make_line(&"x".repeat(81));
        assert_eq!(visual_height(&line, 80), 2);
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

    // ── Clamp offset ──

    #[test]
    fn test_clamp_offset_after_resize() {
        let mut buf = ScrollBuffer::new(2500);
        for i in 0..20 {
            buf.push(make_line(&format!("line {i}")));
        }
        // Scroll to top: 20 lines, viewport 10 → offset = 10
        buf.scroll_to_top(W, 10);
        assert_eq!(buf.offset(), 10);
        assert!(!buf.is_sticky());

        // Simulate resize to viewport height 18 → max_offset = 2
        buf.clamp_offset(W, 18);
        assert_eq!(buf.offset(), 2);
    }

    #[test]
    fn test_clamp_offset_restores_sticky() {
        let mut buf = ScrollBuffer::new(2500);
        for i in 0..5 {
            buf.push(make_line(&format!("line {i}")));
        }
        // Scroll up then grow viewport to fit everything
        buf.scroll_up(3, W, 3);
        assert!(!buf.is_sticky());

        buf.clamp_offset(W, 10); // viewport bigger than content
        assert_eq!(buf.offset(), 0);
        assert!(buf.is_sticky());
    }

    /// Regression: clamp_offset must use the history panel viewport height,
    /// NOT the full terminal height. Using a too-large viewport computes
    /// max_offset too small, clamping the user's scroll position toward
    /// the bottom and re-engaging sticky during inference streaming.
    #[test]
    fn test_clamp_offset_with_correct_viewport_preserves_scroll() {
        let mut buf = ScrollBuffer::new(2500);
        // 50 lines of content
        for i in 0..50 {
            buf.push(make_line(&format!("line {i}")));
        }

        // User scrolls up 15 lines from the bottom
        buf.scroll_up(15, W, 30);
        assert_eq!(buf.offset(), 15);
        assert!(!buf.is_sticky());

        // With correct viewport (30 lines): max_offset = 50 - 30 = 20.
        // 15 < 20 → no clamping.
        buf.clamp_offset(W, 30);
        assert_eq!(buf.offset(), 15);
        assert!(
            !buf.is_sticky(),
            "correct viewport must NOT re-engage sticky"
        );

        // BUG CASE: if caller mistakenly passes full terminal height (40),
        // max_offset = 50 - 40 = 10.  15 > 10 → clamped to 10.
        // This is the wrong behavior the fix prevents.
        buf.clamp_offset(W, 40);
        assert_eq!(buf.offset(), 10, "over-large viewport incorrectly clamps");
    }

    /// Streaming new content while scrolled up must not snap to bottom.
    #[test]
    fn test_push_during_scrolled_up_preserves_position() {
        let mut buf = ScrollBuffer::new(2500);
        for i in 0..30 {
            buf.push(make_line(&format!("line {i}")));
        }

        // User scrolls up 10 lines
        buf.scroll_up(10, W, 20);
        assert!(!buf.is_sticky());
        assert_eq!(buf.offset(), 10);

        // New content arrives (simulating inference streaming)
        buf.push(make_line("streamed token"));
        buf.push(make_line("another token"));

        // Offset and sticky must be unchanged
        assert_eq!(buf.offset(), 10);
        assert!(
            !buf.is_sticky(),
            "push must not re-engage sticky when user scrolled up"
        );
    }

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
