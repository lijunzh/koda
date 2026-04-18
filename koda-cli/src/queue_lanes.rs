//! Pure helpers for the TUI's two input queue lanes.
//!
//! Koda's TUI exposes two input queues that fire while inference is running:
//!
//! - **`next_queue`** (Enter mid-turn) — sent to the engine immediately as a
//!   `QueueNext` command to *steer the current turn*.
//! - **`later_queue`** (Ctrl+J) — deferred until the current turn fully
//!   completes, then **batched into one new turn** (FIFO order).
//!
//! Up arrow during inference pops the most recently deferred item back into
//! the editor (LIFO — like a back button). Ctrl+U clears the whole later
//! queue without cancelling inference.
//!
//! The actual keystroke handler in `tui_handlers_inference.rs` is huge and
//! takes ~10 parameters (textarea, history, scroll buffer, db, …). The pure
//! `VecDeque<String>` operations are extracted here so they can be unit
//! tested without spinning up the whole TUI.
//!
//! Production callers (`tui_handlers_inference.rs`, `tui_context::dequeue_input`)
//! use these helpers and add their own UI side effects (scroll-buffer lines,
//! tracing, etc.) around the returned counts.

use std::collections::VecDeque;

/// Push `text` onto the later queue if it is non-empty after trimming.
///
/// Returns the new queue length on success, or `None` if the input was
/// blank/whitespace-only and was dropped.
///
/// Mirrors the guard in `tui_handlers_inference.rs` Ctrl+J handler:
/// `if !text.trim().is_empty() { ... later_queue.push_back(text); }`.
pub(crate) fn enqueue_later(queue: &mut VecDeque<String>, text: String) -> Option<usize> {
    if text.trim().is_empty() {
        return None;
    }
    queue.push_back(text);
    Some(queue.len())
}

/// Pop the most recently deferred item (LIFO) — used by Up arrow during
/// inference to bring the last `Ctrl+J`'d message back into the editor.
///
/// Returns `None` when the queue is empty (caller falls through to normal
/// textarea cursor movement).
pub(crate) fn pop_later(queue: &mut VecDeque<String>) -> Option<String> {
    queue.pop_back()
}

/// Clear the later queue, returning the number of items dropped.
///
/// Used by Ctrl+U during inference. Returns `0` for an empty queue so the
/// caller can decide whether to show the "Cleared N message(s)" line.
pub(crate) fn clear_later(queue: &mut VecDeque<String>) -> usize {
    let n = queue.len();
    queue.clear();
    n
}

/// Drain the later queue and join all items into one batched turn (FIFO).
///
/// Used by `tui_context::dequeue_input` after inference completes: every
/// deferred item gets concatenated with `\n\n` separators and submitted as
/// a single new inference turn so the model sees them as one coherent
/// follow-up.
///
/// Returns `Some((joined_text, item_count))` when at least one item was
/// drained, or `None` when the queue was empty.
pub(crate) fn drain_later_as_batch(queue: &mut VecDeque<String>) -> Option<(String, usize)> {
    if queue.is_empty() {
        return None;
    }
    let items: Vec<String> = queue.drain(..).collect();
    let count = items.len();
    Some((items.join("\n\n"), count))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty() -> VecDeque<String> {
        VecDeque::new()
    }

    fn seeded(items: &[&str]) -> VecDeque<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // ── enqueue_later ─────────────────────────────────────────────────────

    #[test]
    fn enqueue_later_pushes_text_and_returns_new_count() {
        let mut q = empty();
        assert_eq!(enqueue_later(&mut q, "first".into()), Some(1));
        assert_eq!(enqueue_later(&mut q, "second".into()), Some(2));
        assert_eq!(enqueue_later(&mut q, "third".into()), Some(3));
        assert_eq!(q.len(), 3);
    }

    #[test]
    fn enqueue_later_ignores_empty_string() {
        let mut q = empty();
        assert_eq!(enqueue_later(&mut q, String::new()), None);
        assert!(q.is_empty(), "empty input must not be queued");
    }

    #[test]
    fn enqueue_later_ignores_whitespace_only_string() {
        let mut q = empty();
        assert_eq!(enqueue_later(&mut q, "   \n\t  \n".into()), None);
        assert!(q.is_empty(), "whitespace-only input must not be queued");
    }

    #[test]
    fn enqueue_later_preserves_text_with_internal_whitespace() {
        let mut q = empty();
        // The trim is only for the *guard*; the text itself is stored as-is
        // (model needs the original formatting).
        assert_eq!(enqueue_later(&mut q, "  hello  world  ".into()), Some(1));
        assert_eq!(q.front().unwrap(), "  hello  world  ");
    }

    // ── pop_later ─────────────────────────────────────────────────────────

    #[test]
    fn pop_later_returns_most_recently_pushed_item() {
        // LIFO behavior: Up arrow during inference brings back the LAST
        // thing the user deferred (the most recent Ctrl+J).
        let mut q = seeded(&["alpha", "beta", "gamma"]);
        assert_eq!(pop_later(&mut q), Some("gamma".into()));
        assert_eq!(pop_later(&mut q), Some("beta".into()));
        assert_eq!(pop_later(&mut q), Some("alpha".into()));
        assert_eq!(pop_later(&mut q), None);
    }

    #[test]
    fn pop_later_on_empty_queue_returns_none() {
        let mut q = empty();
        assert_eq!(pop_later(&mut q), None);
    }

    // ── clear_later ───────────────────────────────────────────────────────

    #[test]
    fn clear_later_drains_queue_and_returns_count() {
        let mut q = seeded(&["a", "b", "c", "d"]);
        let dropped = clear_later(&mut q);
        assert_eq!(dropped, 4);
        assert!(q.is_empty(), "queue must be empty after clear");
    }

    #[test]
    fn clear_later_on_empty_returns_zero() {
        let mut q = empty();
        assert_eq!(clear_later(&mut q), 0);
    }

    // ── drain_later_as_batch ──────────────────────────────────────────────

    #[test]
    fn drain_as_batch_returns_none_when_empty() {
        let mut q = empty();
        assert!(drain_later_as_batch(&mut q).is_none());
    }

    #[test]
    fn drain_as_batch_preserves_fifo_order() {
        // FIFO: items deferred earliest appear FIRST in the batched turn,
        // matching natural reading order. This is the regression guard the
        // QA expert called out by name (queue_items_sent_in_fifo_order).
        let mut q = seeded(&["one", "two", "three", "four"]);
        let (joined, count) = drain_later_as_batch(&mut q).unwrap();
        assert_eq!(count, 4);
        assert_eq!(joined, "one\n\ntwo\n\nthree\n\nfour");
        assert!(q.is_empty(), "drain must empty the queue");
    }

    #[test]
    fn drain_as_batch_single_item_has_no_separator() {
        let mut q = seeded(&["only"]);
        let (joined, count) = drain_later_as_batch(&mut q).unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            joined, "only",
            "single item must not gain a trailing \\n\\n"
        );
    }

    #[test]
    fn drain_as_batch_uses_double_newline_separator() {
        // Regression guard: must be \n\n, not \n. The model sees each item
        // as a logically separate paragraph.
        let mut q = seeded(&["a", "b"]);
        let (joined, _) = drain_later_as_batch(&mut q).unwrap();
        assert!(
            joined.contains("\n\n"),
            "batch separator must be a blank line: got {joined:?}"
        );
        assert!(
            !joined.contains("\n\n\n"),
            "no triple newlines: got {joined:?}"
        );
    }

    // ── End-to-end lifecycle (#897 explicit list) ─────────────────────────

    #[test]
    fn push_to_later_queue_during_inference_enqueues() {
        // QA-named: "push_to_later_queue_during_inference_enqueues"
        let mut q = empty();
        enqueue_later(&mut q, "deferred 1".into()).unwrap();
        enqueue_later(&mut q, "deferred 2".into()).unwrap();
        assert_eq!(q.len(), 2);
        assert_eq!(q.front().unwrap(), "deferred 1");
        assert_eq!(q.back().unwrap(), "deferred 2");
    }

    #[test]
    fn up_arrow_pops_from_later_queue() {
        // QA-named: "up_arrow_pops_from_later_queue"
        let mut q = empty();
        enqueue_later(&mut q, "first".into());
        enqueue_later(&mut q, "second".into());
        // Up arrow → pop the most recent.
        assert_eq!(pop_later(&mut q), Some("second".into()));
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn ctrl_u_clears_later_queue() {
        // QA-named: "ctrl_u_clears_later_queue"
        let mut q = seeded(&["one", "two", "three"]);
        let n = clear_later(&mut q);
        assert_eq!(n, 3);
        assert!(q.is_empty());
    }

    #[test]
    fn queue_empty_after_all_items_popped() {
        // QA-named: "queue_empty_after_all_items_popped"
        let mut q = seeded(&["x", "y"]);
        pop_later(&mut q);
        pop_later(&mut q);
        assert!(q.is_empty());
        // And one more pop must yield None, not panic.
        assert_eq!(pop_later(&mut q), None);
    }

    #[test]
    fn queue_items_sent_in_fifo_order_after_inference() {
        // QA-named: "queue_items_sent_in_fifo_order_after_inference"
        let mut q = empty();
        enqueue_later(&mut q, "step 1".into());
        enqueue_later(&mut q, "step 2".into());
        enqueue_later(&mut q, "step 3".into());
        let (batched, _) = drain_later_as_batch(&mut q).unwrap();
        // Order in the batched turn matches insertion order.
        let s1 = batched.find("step 1").expect("step 1 in batch");
        let s2 = batched.find("step 2").expect("step 2 in batch");
        let s3 = batched.find("step 3").expect("step 3 in batch");
        assert!(s1 < s2 && s2 < s3, "FIFO order required: got {batched:?}");
    }

    // ── Mixed lifecycle: push → pop → clear → drain ───────────────────────

    #[test]
    fn full_lifecycle_push_pop_clear_drain() {
        let mut q = empty();
        // Push 3, pop 1, clear 2.
        enqueue_later(&mut q, "a".into());
        enqueue_later(&mut q, "b".into());
        enqueue_later(&mut q, "c".into());
        assert_eq!(pop_later(&mut q), Some("c".into()));
        assert_eq!(clear_later(&mut q), 2);
        assert!(drain_later_as_batch(&mut q).is_none());

        // After a full clear the queue is reusable.
        enqueue_later(&mut q, "fresh".into());
        let (batched, count) = drain_later_as_batch(&mut q).unwrap();
        assert_eq!(count, 1);
        assert_eq!(batched, "fresh");
    }
}
