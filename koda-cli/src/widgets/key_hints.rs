//! Single-row key-hint footer rendered between the input textarea and the
//! status bar. Surfaces the most-used keybindings so first-time users don't
//! have to read docs to discover Tab-completion, Shift+Enter newlines, etc.
//!
//! # Why this exists (PR 4 of #1178)
//!
//! Before this widget, the row below the input was a flat
//! `─────────────────` separator — pure visual chrome with no information
//! value. The codex `composer::key_hint` port (PR 1 of #1178) gave us a
//! `From<KeyBinding> for Span` impl that styles a key as a dim
//! `lowercase+modifier` token (e.g. `ctrl+c`, `shift+enter`). This widget
//! pairs each rendered key with a one-word verb and joins them with `·`
//! separators, replacing the dead-space separator with discoverable hints.
//!
//! # Truncation behavior
//!
//! Hints are rendered left-to-right. When the available width is narrower
//! than the full set, items are dropped from the right one at a time until
//! the remaining set fits. This keeps the high-priority items (`enter`,
//! `shift+enter`) visible on narrow terminals and gracefully degrades on
//! 80-column screens.
//!
//! # What is *not* in here
//!
//! - Vim-mode-aware hints: when `/vim` is active, `i a o` (enter INSERT)
//!   and `esc` (back to NORMAL) become the most relevant bindings. That
//!   reactivity is intentional follow-up scope; v1 ships a static set so
//!   the rendering is dead-simple and the discovery win is immediate.
//! - User-configurable hints: the set is hard-coded against
//!   `composer::keymap::EditorKeymap::defaults()`. If a future PR adds a
//!   user-keymap config, this widget should rebuild from that instead of
//!   hard-coding the bindings.

use crate::composer::key_hint::{KeyBinding, ctrl, plain, shift};
use crossterm::event::KeyCode;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

/// Single (key-binding, verb) hint pair.
///
/// `verb` is the imperative action (e.g. `"send"`, `"newline"`) shown
/// dimmed-grey next to the key token.
#[derive(Debug, Clone, Copy)]
pub struct Hint {
    pub key: KeyBinding,
    pub verb: &'static str,
}

impl Hint {
    pub const fn new(key: KeyBinding, verb: &'static str) -> Self {
        Self { key, verb }
    }
}

/// Footer widget rendering an ordered list of [`Hint`]s on one row.
///
/// Build with [`KeyHints::default_set`] for the standard koda set, or
/// `KeyHints { items }` directly for tests / custom callers.
pub struct KeyHints {
    items: Vec<Hint>,
}

impl KeyHints {
    /// The standard koda hint set (6 items, ~85-100 visual columns).
    ///
    /// Order is by priority — narrow terminals truncate from the right,
    /// so the most-essential hints (`enter`, `shift+enter`) live first.
    /// Bindings mirror `composer::keymap::EditorKeymap::defaults()` for
    /// the textarea-internal items and koda's own dispatch for the rest
    /// (`tab` → completer, `esc` → menu cancel, `↑↓` → history, `ctrl+c`
    /// → quit).
    pub fn default_set() -> Self {
        Self {
            items: vec![
                Hint::new(plain(KeyCode::Enter), "send"),
                Hint::new(shift(KeyCode::Enter), "newline"),
                Hint::new(plain(KeyCode::Tab), "complete"),
                Hint::new(plain(KeyCode::Esc), "menu"),
                Hint::new(plain(KeyCode::Up), "history"),
                Hint::new(ctrl(KeyCode::Char('c')), "quit"),
            ],
        }
    }
}

impl Widget for KeyHints {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let spans = build_hint_spans(&self.items, area.width as usize);
        Paragraph::new(Line::from(spans)).render(area, buf);
    }
}

/// Build the styled spans for a hint row, dropping items from the right
/// until the rendered width fits in `max_width` columns.
///
/// Layout per item: `<key-span> <verb-span>` joined by ` · ` separators.
/// The verb is dimmed slightly less than the key so the eye lands on the
/// action first, then the chord.
fn build_hint_spans(items: &[Hint], max_width: usize) -> Vec<Span<'static>> {
    if items.is_empty() || max_width == 0 {
        return Vec::new();
    }
    // Iteratively drop from the right until the rendered width fits.
    // Cheap O(n²) — n is always ≤ 6 in practice; not worth a smarter algo.
    for take in (1..=items.len()).rev() {
        let spans = render_n(items, take);
        let width: usize = spans.iter().map(|s| s.width()).sum();
        if width <= max_width {
            return spans;
        }
    }
    // Even the first item alone doesn't fit — render it anyway and let
    // the Paragraph clip. Better than an empty footer that hides the
    // whole feature on narrow terminals.
    render_n(items, 1)
}

/// Render exactly `n` items as `key verb · key verb · ...` spans.
fn render_n(items: &[Hint], n: usize) -> Vec<Span<'static>> {
    let verb_style = Style::default().fg(Color::Rgb(140, 140, 140));
    let sep_style = Style::default().fg(Color::Rgb(80, 80, 80));
    let mut spans = Vec::with_capacity(n * 4);
    for (i, hint) in items.iter().take(n).enumerate() {
        if i > 0 {
            spans.push(Span::styled(" \u{00b7} ", sep_style));
        }
        spans.push(Span::from(hint.key));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(hint.verb, verb_style));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rendered width fits any `max_width >= total_width(default_set)`.
    #[test]
    fn default_set_fits_when_width_is_generous() {
        let items = KeyHints::default_set().items;
        let spans = build_hint_spans(&items, 200);
        let width: usize = spans.iter().map(|s| s.width()).sum();
        assert!(
            width <= 200,
            "default set must fit in 200 cols (got {width})"
        );
        assert_eq!(
            spans.iter().filter(|s| s.content == " \u{00b7} ").count(),
            items.len() - 1,
            "n items \u{2192} n-1 separators",
        );
    }

    /// On narrow terminals, items drop from the right; `enter send`
    /// (the highest-priority hint) survives.
    #[test]
    fn narrow_terminal_drops_low_priority_items_first() {
        let items = KeyHints::default_set().items;
        let spans = build_hint_spans(&items, 30);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("enter") && text.contains("send"),
            "highest-priority hint must survive narrow render: {text}"
        );
        assert!(
            !text.contains("ctrl+c"),
            "lowest-priority hint must be dropped on 30-col terminal: {text}"
        );
    }

    /// Zero width yields no spans (don't crash on degenerate input).
    #[test]
    fn zero_width_returns_empty() {
        let items = KeyHints::default_set().items;
        assert!(build_hint_spans(&items, 0).is_empty());
    }

    /// Empty hint list renders nothing regardless of width.
    #[test]
    fn empty_items_returns_empty() {
        assert!(build_hint_spans(&[], 200).is_empty());
    }

    /// End-to-end Widget render path: rendered buffer contains all verbs
    /// when the area is wide enough.
    #[test]
    fn widget_render_writes_all_verbs_into_buffer() {
        let area = Rect::new(0, 0, 120, 1);
        let mut buf = Buffer::empty(area);
        KeyHints::default_set().render(area, &mut buf);
        let text: String = (0..area.width)
            .map(|x| buf.cell((x, 0)).map(|c| c.symbol()).unwrap_or(" "))
            .collect();
        for verb in ["send", "newline", "complete", "menu", "history", "quit"] {
            assert!(
                text.contains(verb),
                "rendered footer must contain {verb:?}: {text}"
            );
        }
    }
}
