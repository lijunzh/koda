//! History navigation: index math for Up/Down recall in the composer.
//!
//! The composer's Up/Down arrows walk through previously submitted
//! prompts (`history: Vec<String>`). This module owns the *pure index
//! arithmetic*; the actual textarea-swap and side effects live on
//! [`crate::tui_context::TuiContext`] because they need access to the
//! textarea and DB.
//!
//! ## Why a separate module
//!
//! - **Testability** — the index logic is the only piece with edge
//!   cases worth unit-testing (empty history, top-saturation,
//!   off-the-end). A standalone module lets the tests live next to
//!   the code without dragging in a full `TuiContext` fixture.
//! - **Locality** — composer-related state (textarea, slash, paste-
//!   burst, history) all live under the `composer/` tree. Future
//!   syncs from codex are easier to review when our shape mirrors
//!   theirs.
//!
//! See #1187 for the refactor RFC.

/// Compute the next history index when pressing Up (older).
///
/// Returns `None` if the history is empty. Saturates at index 0 — Up
/// at the oldest entry stays on the oldest entry rather than wrapping
/// around (matches CC / fish / zsh semantics).
pub fn history_up_index(current: Option<usize>, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match current {
        None => len - 1,
        Some(i) => i.saturating_sub(1),
    })
}

/// Compute the next history index when pressing Down (newer).
///
/// Returns `None` when moving past the most recent entry — the caller
/// interprets this as "clear the input" (back to a blank composer),
/// matching CC / fish / zsh semantics.
pub fn history_down_index(current: Option<usize>, len: usize) -> Option<usize> {
    match current {
        Some(i) if i + 1 < len => Some(i + 1),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Up navigation ─────────────────────────────────────────

    #[test]
    fn up_from_none_lands_on_most_recent() {
        // Pressing Up with no current selection drops to the last entry.
        assert_eq!(history_up_index(None, 5), Some(4));
    }

    #[test]
    fn up_from_middle_moves_one_older() {
        assert_eq!(history_up_index(Some(3), 5), Some(2));
    }

    #[test]
    fn up_at_top_saturates_at_zero() {
        // Up at the oldest entry stays put — does NOT wrap around.
        assert_eq!(history_up_index(Some(0), 5), Some(0));
    }

    #[test]
    fn up_with_empty_history_returns_none() {
        // No history = no navigation possible.
        assert_eq!(history_up_index(None, 0), None);
    }

    // ── Down navigation ───────────────────────────────────────

    #[test]
    fn down_from_middle_moves_one_newer() {
        assert_eq!(history_down_index(Some(2), 5), Some(3));
    }

    #[test]
    fn down_past_last_returns_none() {
        // Down past the most recent entry → caller clears the composer.
        assert_eq!(history_down_index(Some(4), 5), None);
    }

    #[test]
    fn down_from_none_returns_none() {
        // Down with no selection is a no-op (nothing to advance from).
        assert_eq!(history_down_index(None, 5), None);
    }
}
