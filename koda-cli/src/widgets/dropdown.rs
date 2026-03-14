//! Generic dropdown widget — rendered inside the ratatui viewport.
//!
//! Reusable dropdown with type-to-filter, scroll, and fixed-height
//! rendering. Used by slash commands, `/model`, `/provider`, etc.
//!
//! See DESIGN.md §14 for the interaction system design.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

// ── Styles ────────────────────────────────────────────

const DIM: Style = Style::new().fg(Color::Rgb(124, 111, 100));
const SELECTED: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
const UNSELECTED: Style = Style::new().fg(Color::Rgb(124, 111, 100));
const DESC: Style = Style::new().fg(Color::Rgb(198, 165, 106));
const HINT: Style = Style::new().fg(Color::Rgb(124, 111, 100));

/// Max visible items in the dropdown (scroll for more).
pub const MAX_VISIBLE: usize = 6;

// ── Trait ────────────────────────────────────────────

/// Trait for items that can be displayed in a dropdown.
pub trait DropdownItem: Clone {
    /// Primary label shown in the list.
    fn label(&self) -> &str;
    /// Optional description shown after the label.
    fn description(&self) -> String;
    /// Whether this item matches a filter string.
    fn matches_filter(&self, filter: &str) -> bool;
}

// ── Built-in item types ────────────────────────────────

/// Simple label+description pair (for static command lists, providers, etc.).
#[derive(Clone, Debug)]
#[allow(dead_code)] // Used in Phase 2 (/model, /provider conversions)
pub struct SimpleItem {
    pub label: String,
    pub description: String,
}

impl SimpleItem {
    #[allow(dead_code)] // Used in Phase 2
    pub fn new(label: impl Into<String>, desc: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            description: desc.into(),
        }
    }
}

impl DropdownItem for SimpleItem {
    fn label(&self) -> &str {
        &self.label
    }
    fn description(&self) -> String {
        self.description.clone()
    }
    fn matches_filter(&self, filter: &str) -> bool {
        let lower = self.label.to_lowercase();
        let filter_lower = filter.to_lowercase();
        lower.contains(&filter_lower)
    }
}

// ── State ────────────────────────────────────────────

/// Generic dropdown state. Owns the filtered item list, selection,
/// and scroll offset. Type parameter `T` must implement `DropdownItem`.
#[derive(Clone)]
pub struct DropdownState<T: DropdownItem> {
    /// All items (unfiltered source).
    all_items: Vec<T>,
    /// Currently visible items after filtering.
    pub filtered: Vec<T>,
    /// Index into `filtered`.
    pub selected: usize,
    /// Scroll offset for the visible window.
    pub scroll_offset: usize,
    /// Title shown above the dropdown.
    pub title: String,
}

impl<T: DropdownItem> DropdownState<T> {
    /// Create a new dropdown with the given items and title.
    pub fn new(items: Vec<T>, title: impl Into<String>) -> Self {
        let filtered = items.clone();
        Self {
            all_items: items,
            filtered,
            selected: 0,
            scroll_offset: 0,
            title: title.into(),
        }
    }

    /// Apply a filter string. Resets selection to 0.
    /// Returns `false` if no items match (caller can dismiss).
    pub fn apply_filter(&mut self, filter: &str) -> bool {
        self.filtered = self
            .all_items
            .iter()
            .filter(|item| item.matches_filter(filter))
            .cloned()
            .collect();
        self.selected = 0;
        self.scroll_offset = 0;
        !self.filtered.is_empty()
    }

    /// Move selection up.
    pub fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        }
    }

    /// Move selection down (wraps around).
    pub fn down(&mut self) {
        if self.selected + 1 < self.filtered.len() {
            self.selected += 1;
        } else {
            self.selected = 0;
            self.scroll_offset = 0;
        }
        let visible = MAX_VISIBLE.min(self.filtered.len());
        if self.selected >= self.scroll_offset + visible {
            self.scroll_offset = self.selected + 1 - visible;
        }
    }

    /// Get the currently selected item, if any.
    pub fn selected_item(&self) -> Option<&T> {
        self.filtered.get(self.selected)
    }

    /// Check if the dropdown has any items to show.
    #[allow(dead_code)] // Used in Phase 2
    pub fn is_empty(&self) -> bool {
        self.filtered.is_empty()
    }
}

// ── Rendering ─────────────────────────────────────────

/// Build dropdown lines for rendering in the viewport.
/// Always returns exactly `MAX_VISIBLE + 2` lines (fixed height).
pub fn build_dropdown_lines<T: DropdownItem>(state: &DropdownState<T>) -> Vec<Line<'static>> {
    let visible = MAX_VISIBLE.min(state.filtered.len());
    let end = (state.scroll_offset + visible).min(state.filtered.len());
    let window = &state.filtered[state.scroll_offset..end];
    let has_above = state.scroll_offset > 0;
    let has_below = end < state.filtered.len();

    let mut lines = Vec::with_capacity(MAX_VISIBLE + 2);

    // Title with scroll indicator
    let title = if has_above {
        format!("  {} \u{25b2} more", state.title)
    } else {
        format!("  {}", state.title)
    };
    lines.push(Line::from(Span::styled(title, DIM)));

    // Visible options
    for (i, item) in window.iter().enumerate() {
        let absolute_idx = state.scroll_offset + i;
        let is_selected = absolute_idx == state.selected;
        let label = item.label().to_string();
        let desc = item.description();
        let mut spans = Vec::with_capacity(4);

        if is_selected {
            spans.push(Span::styled(
                "  \u{203a} ",
                Style::default().fg(Color::Cyan),
            ));
            spans.push(Span::styled(label, SELECTED));
        } else {
            spans.push(Span::raw("    "));
            spans.push(Span::styled(label, UNSELECTED));
        }
        if !desc.is_empty() {
            spans.push(Span::styled(format!("  {desc}"), DESC));
        }

        lines.push(Line::from(spans));
    }

    // Pad empty slots to maintain fixed height
    for _ in visible..MAX_VISIBLE {
        lines.push(Line::from(""));
    }

    // Hint with scroll indicator
    let hint = if has_below {
        "  \u{2191}/\u{2193} navigate \u{00b7} enter select \u{00b7} esc cancel  \u{25bc} more"
    } else {
        "  \u{2191}/\u{2193} navigate \u{00b7} enter select \u{00b7} esc cancel"
    };
    lines.push(Line::from(Span::styled(hint, HINT)));

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_items() -> Vec<SimpleItem> {
        vec![
            SimpleItem::new("/agent", "Agents"),
            SimpleItem::new("/compact", "Compact"),
            SimpleItem::new("/cost", "Cost"),
            SimpleItem::new("/diff", "Diff"),
            SimpleItem::new("/exit", "Quit"),
            SimpleItem::new("/expand", "Expand"),
            SimpleItem::new("/model", "Pick model"),
        ]
    }

    #[test]
    fn new_contains_all() {
        let dd = DropdownState::new(test_items(), "Test");
        assert_eq!(dd.filtered.len(), 7);
        assert_eq!(dd.selected, 0);
    }

    #[test]
    fn filter_narrows() {
        let mut dd = DropdownState::new(test_items(), "Test");
        assert!(dd.apply_filter("/m"));
        assert_eq!(dd.filtered.len(), 1); // /model
        assert_eq!(dd.filtered[0].label(), "/model");
    }

    #[test]
    fn filter_no_match() {
        let mut dd = DropdownState::new(test_items(), "Test");
        assert!(!dd.apply_filter("/z"));
        assert!(dd.is_empty());
    }

    #[test]
    fn filter_case_insensitive() {
        let mut dd = DropdownState::new(test_items(), "Test");
        assert!(dd.apply_filter("/MODEL"));
        assert_eq!(dd.filtered.len(), 1);
    }

    #[test]
    fn navigation() {
        let mut dd = DropdownState::new(test_items(), "Test");
        assert_eq!(dd.selected_item().unwrap().label(), "/agent");
        dd.down();
        assert_eq!(dd.selected_item().unwrap().label(), "/compact");
        for _ in 0..5 {
            dd.down();
        }
        assert_eq!(dd.selected_item().unwrap().label(), "/model");
        dd.down(); // wraps
        assert_eq!(dd.selected_item().unwrap().label(), "/agent");
        dd.up(); // saturates at 0
        assert_eq!(dd.selected_item().unwrap().label(), "/agent");
    }

    #[test]
    fn scroll_indicators() {
        let dd = DropdownState::new(test_items(), "Test");
        let lines = build_dropdown_lines(&dd);
        // 8 items, 6 visible → should have ▼ more
        let hint: String = lines
            .last()
            .unwrap()
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(hint.contains('\u{25bc}'), "should show scroll-down: {hint}");
    }

    #[test]
    fn fixed_height() {
        let dd = DropdownState::new(test_items(), "Test");
        let lines = build_dropdown_lines(&dd);
        assert_eq!(lines.len(), 8); // title + 6 slots + hint

        // Filtered to 2 items — still 8 lines
        let mut dd2 = DropdownState::new(test_items(), "Test");
        dd2.apply_filter("/e");
        let lines = build_dropdown_lines(&dd2);
        assert_eq!(lines.len(), 8);
    }

    #[test]
    fn selected_marker() {
        let dd = DropdownState::new(test_items(), "Test");
        let lines = build_dropdown_lines(&dd);
        let first: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(first.contains('\u{203a}'), "got: {first}");
        let second: String = lines[2].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!second.contains('\u{203a}'), "got: {second}");
    }

    #[test]
    fn selected_item_empty() {
        let mut dd = DropdownState::new(test_items(), "Test");
        dd.apply_filter("/zzz");
        assert!(dd.selected_item().is_none());
    }
}
