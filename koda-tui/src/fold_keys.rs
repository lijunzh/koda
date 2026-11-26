//! Opening one folded item: a click on it, or the keyboard's focus mode.
//!
//! The folded view sums a turn's steps up in rows (`Read 3 files`,
//! `∴ Thought for 12s`, `… +40 lines`). One of them opens in place on its
//! own, as Claude Code's fullscreen view opens a row on a click, and Codex's
//! TUI from the keyboard: F4 puts the focus on the last item on screen,
//! Up and Down move it, Enter opens or closes the item, Right opens it,
//! Left closes it, and Esc or F4 leaves. Ctrl+O still opens everything.
//!
//! Both the idle loop and the turn's loop send their keys and clicks here.

use crate::scroll_buffer::ScrollBuffer;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Handle `key` if it belongs to focus mode: F4 always, the others while
/// the focus is on an item. Returns whether it was used. Call it only when
/// no dialog or menu wants the key.
pub(crate) fn handle_key(
    buffer: &mut ScrollBuffer,
    key: KeyEvent,
    term_width: usize,
    viewport_height: usize,
) -> bool {
    if key.modifiers != KeyModifiers::NONE {
        return false;
    }
    if key.code == KeyCode::F(4) {
        if buffer.focus().is_some() {
            buffer.clear_focus();
        } else {
            buffer.focus_last(term_width, viewport_height);
        }
        return true;
    }
    let Some(item) = buffer.focus() else {
        return false;
    };
    match key.code {
        KeyCode::Up => buffer.move_focus(-1, term_width, viewport_height),
        KeyCode::Down => buffer.move_focus(1, term_width, viewport_height),
        KeyCode::Enter => buffer.toggle(item, term_width, viewport_height),
        KeyCode::Right => buffer.set_open(item, true, term_width, viewport_height),
        KeyCode::Left => buffer.set_open(item, false, term_width, viewport_height),
        KeyCode::Esc => buffer.clear_focus(),
        _ => return false,
    }
    true
}

/// A click on row `screen_row` of the history panel: open or close the item
/// there, if it has something folded away. In focus mode the focus moves to
/// it. Returns whether there was one.
pub(crate) fn click(
    buffer: &mut ScrollBuffer,
    screen_row: usize,
    term_width: usize,
    viewport_height: usize,
) -> bool {
    let Some(item) = buffer.item_at(screen_row, term_width, viewport_height) else {
        return false;
    };
    buffer.toggle(item, term_width, viewport_height);
    if buffer.focus().is_some() {
        buffer.set_focus(item);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scroll_buffer::Fold;
    use ratatui::text::Line;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn text(buffer: &ScrollBuffer) -> Vec<String> {
        buffer.all_lines().map(|l| l.to_string()).collect()
    }

    fn two_rows() -> (ScrollBuffer, u64, u64) {
        let mut buffer = ScrollBuffer::new(64);
        let first = buffer.push_row(Line::from("Read 2 files"));
        buffer.push_in(Line::from("a.rs"), Fold::Full, Some(first));
        buffer.push(Line::from("note"));
        let second = buffer.push_row(Line::from("Ran 1 command"));
        buffer.push_in(Line::from("ls"), Fold::Full, Some(second));
        (buffer, first, second)
    }

    /// F4, Up, Enter opens the row above; Left closes it; Esc leaves.
    #[test]
    fn the_keyboard_opens_one_row() {
        let (mut buffer, first, second) = two_rows();
        // Nothing focused: arrows and Enter belong to the prompt.
        assert!(!handle_key(&mut buffer, key(KeyCode::Up), 80, 20));
        assert!(handle_key(&mut buffer, key(KeyCode::F(4)), 80, 20));
        assert_eq!(buffer.focus(), Some(second));
        assert!(handle_key(&mut buffer, key(KeyCode::Up), 80, 20));
        assert_eq!(buffer.focus(), Some(first));
        assert!(handle_key(&mut buffer, key(KeyCode::Enter), 80, 20));
        assert_eq!(
            text(&buffer),
            vec!["Read 2 files", "a.rs", "note", "Ran 1 command"]
        );
        assert!(handle_key(&mut buffer, key(KeyCode::Left), 80, 20));
        assert_eq!(text(&buffer), vec!["Read 2 files", "note", "Ran 1 command"]);
        // Another key goes on to the prompt.
        assert!(!handle_key(&mut buffer, key(KeyCode::Char('x')), 80, 20));
        assert!(handle_key(&mut buffer, key(KeyCode::Esc), 80, 20));
        assert_eq!(buffer.focus(), None);
        assert!(!handle_key(&mut buffer, key(KeyCode::Esc), 80, 20));
    }

    /// A click opens the row under it and closes it again; a click on a
    /// line with nothing folded away does nothing.
    #[test]
    fn a_click_opens_the_row_under_it() {
        let (mut buffer, first, _) = two_rows();
        assert!(click(&mut buffer, 0, 80, 20));
        assert!(buffer.is_open(first));
        assert_eq!(text(&buffer)[1], "a.rs");
        // "a.rs" belongs to the row: a click on it closes the row.
        assert!(click(&mut buffer, 1, 80, 20));
        assert!(!buffer.is_open(first));
        assert!(!click(&mut buffer, 1, 80, 20), "{:?}", text(&buffer));
    }
}
