//! Per-session context cache for `load_context` (#1166 audit item A).
//!
//! ## Why this exists
//!
//! Benchmarks (`benches/assemble_context_bench.rs`) showed that
//! `Database::load_context` was 93–96% of the per-iteration cost of
//! `assemble_context` — the SQL select + row deserialization swamps the
//! pure-CPU sanitization passes. At a realistic 1000-message session
//! that's ~5ms per inference loop iteration; in a 50-iter agentic turn
//! that's ~250ms wasted re-fetching rows we already had.
//!
//! ## Design
//!
//! Per-session: `(messages_after_sanitization, max_id_returned, compaction_gen)`.
//!
//! On `load_context`:
//!   1. If `compaction_gen` snapshot differs from the current gen → full
//!      reload (compaction is the only retroactive mutation against
//!      previously-cached rows).
//!   2. Otherwise delta-fetch rows with `id > max_id_returned`, append
//!      them to the cached message list, re-run sanitization passes,
//!      cache + return.
//!
//! ## Why this is correct
//!
//! - **Append-only happy path.** New `Role::User`/`Role::Tool`
//!   inserts always get a strictly higher `id` (SQLite ROWID) than
//!   anything we've returned. The delta query catches them.
//! - **Newly-completed assistants.** An assistant row inserted earlier
//!   without `completed_at` is filtered out by `load_context`'s WHERE
//!   clause, so it does NOT contribute to `max_id_returned`. When
//!   `mark_message_complete` later sets `completed_at`, the next
//!   `load_context` delta query (`id > max_id_returned`) re-evaluates
//!   it because its id is still > our high-water mark.
//! - **Compaction.** Sets `compacted_at` on existing rows (retroactive
//!   filter change). We bump `compaction_gen` to force a full reload —
//!   this is rare (user-triggered or auto-compact at high context %)
//!   so amortizes to ~0.
//! - **Session deletion.** Cache entry is removed by
//!   `Database::clear_context_cache_for` from the delete path.
//!
//! ## Invariant required by callers
//!
//! For correctness, callers must follow this ordering for any single
//! session: an assistant row's `mark_message_complete` MUST be called
//! before any subsequent rows are inserted (i.e. before the next
//! tool-result row, before the next iteration's assistant). koda's
//! inference loop honors this naturally
//! (see `inference.rs:1081`); sub-agent dispatch and microcompact
//! likewise.
//!
//! Violating the invariant would cause a newly-completed assistant
//! whose id falls *below* our cached `max_id_returned` to be missed by
//! subsequent delta queries until the next compaction or full reload.
//! If a future code path needs out-of-order completion, either
//! invalidate the cache via `Database::clear_context_cache_for` or
//! extend `ContextCacheEntry` with a `pending_complete_ids` set.
//!
//! ## Cost
//!
//! Cache hit (no new rows): O(N_cached) for the sanitization re-run on
//! the cached vec — but no SQL roundtrip. ~95% reduction at N=1000.
//!
//! Cache hit + delta (k new rows): O(N_cached + k) sanitization, plus a
//! tiny SELECT for k rows. Wins as long as k ≪ N.
//!
//! Cache miss / compaction: identical cost to today. No regression.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::persistence::Message;

/// One cached session's last-known context state.
#[derive(Debug, Clone)]
pub(crate) struct ContextCacheEntry {
    /// Sanitized messages from the last `load_context` call.
    pub messages: Vec<Message>,
    /// Highest `id` actually present in `messages` after sanitization
    /// (NOT the max id in the DB). The next delta query uses this as
    /// `id > max_id_returned`.
    ///
    /// `None` when `messages` is empty (e.g. brand-new session).
    pub max_id_returned: Option<i64>,
    /// Snapshot of `Database::compaction_gen` at the time of caching.
    /// Mismatch → invalidate.
    pub compaction_gen_snapshot: u64,
}

/// Concurrent map of `session_id → ContextCacheEntry`.
///
/// We use `std::sync::Mutex` (not `tokio::sync::Mutex`) because critical
/// sections are pure in-memory operations (clone-into / clone-out) and
/// never hold across `.await`. SQL roundtrips happen *outside* the lock.
#[derive(Debug, Default)]
pub(crate) struct ContextCache {
    entries: Mutex<HashMap<String, ContextCacheEntry>>,
}

impl ContextCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Snapshot the current entry for `session_id`, if any.
    ///
    /// Returns a clone so the caller can release the lock before doing
    /// any async DB work.
    pub(crate) fn snapshot(&self, session_id: &str) -> Option<ContextCacheEntry> {
        self.entries
            .lock()
            .expect("ContextCache mutex poisoned")
            .get(session_id)
            .cloned()
    }

    /// Replace (or insert) the entry for `session_id`.
    pub(crate) fn store(&self, session_id: &str, entry: ContextCacheEntry) {
        self.entries
            .lock()
            .expect("ContextCache mutex poisoned")
            .insert(session_id.to_string(), entry);
    }

    /// Forget the cached entry for `session_id`.
    ///
    /// Called from `delete_session` and exposed via
    /// `Database::clear_context_cache_for` for tests that mutate rows
    /// behind the cache's back.
    pub(crate) fn invalidate(&self, session_id: &str) {
        self.entries
            .lock()
            .expect("ContextCache mutex poisoned")
            .remove(session_id);
    }

    /// Forget every cached entry. Used by tests; never on the hot path.
    #[cfg(test)]
    pub(crate) fn clear_all(&self) {
        self.entries
            .lock()
            .expect("ContextCache mutex poisoned")
            .clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(max_id: Option<i64>, gen_snap: u64) -> ContextCacheEntry {
        ContextCacheEntry {
            messages: vec![],
            max_id_returned: max_id,
            compaction_gen_snapshot: gen_snap,
        }
    }

    #[test]
    fn snapshot_returns_none_when_empty() {
        let cache = ContextCache::new();
        assert!(cache.snapshot("s1").is_none());
    }

    #[test]
    fn store_then_snapshot_round_trip() {
        let cache = ContextCache::new();
        cache.store("s1", entry(Some(42), 7));
        let got = cache.snapshot("s1").unwrap();
        assert_eq!(got.max_id_returned, Some(42));
        assert_eq!(got.compaction_gen_snapshot, 7);
    }

    #[test]
    fn invalidate_removes_only_target_session() {
        let cache = ContextCache::new();
        cache.store("s1", entry(Some(10), 0));
        cache.store("s2", entry(Some(20), 0));
        cache.invalidate("s1");
        assert!(cache.snapshot("s1").is_none());
        assert!(cache.snapshot("s2").is_some());
    }

    #[test]
    fn store_overwrites_existing_entry() {
        let cache = ContextCache::new();
        cache.store("s1", entry(Some(10), 0));
        cache.store("s1", entry(Some(99), 5));
        let got = cache.snapshot("s1").unwrap();
        assert_eq!(got.max_id_returned, Some(99));
        assert_eq!(got.compaction_gen_snapshot, 5);
    }

    #[test]
    fn clear_all_removes_every_entry() {
        let cache = ContextCache::new();
        cache.store("s1", entry(Some(10), 0));
        cache.store("s2", entry(Some(20), 0));
        cache.clear_all();
        assert!(cache.snapshot("s1").is_none());
        assert!(cache.snapshot("s2").is_none());
    }
}
