//! Sub-agent result caching.
//!
//! Caches sub-agent results keyed by `(agent_name, prompt_hash)` within a
//! session. On cache hit, returns the previous response immediately —
//! zero-cost retries for compaction-triggered re-planning.
//!
//! Cache entries are invalidated when files are mutated (piggybacks on
//! `FileReadCache` mtime tracking via a generation counter).

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

/// Cache key: (agent_name, prompt_hash).
type CacheKey = (String, u64);

/// Shared sub-agent result cache.
///
/// Wrapped in `Arc<Mutex<>>` so parent and parallel sub-agents can share it.
#[derive(Clone, Debug)]
pub struct SubAgentCache {
    inner: Arc<Mutex<CacheInner>>,
}

#[derive(Debug)]
struct CacheInner {
    entries: HashMap<CacheKey, CachedResult>,
    /// Monotonically increasing counter bumped on every file mutation.
    /// Entries stored with a stale generation are considered invalid.
    generation: u64,
}

#[derive(Debug, Clone)]
struct CachedResult {
    response: String,
    generation: u64,
}

impl SubAgentCache {
    /// Create a new empty cache.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(CacheInner {
                entries: HashMap::new(),
                generation: 0,
            })),
        }
    }

    /// Look up a cached result for the given agent + prompt.
    ///
    /// Returns `Some(response)` on cache hit (and generation is current),
    /// `None` on miss or stale entry.
    pub fn get(&self, agent_name: &str, prompt: &str) -> Option<String> {
        let key = make_key(agent_name, prompt);
        let inner = self.inner.lock().ok()?;
        let entry = inner.entries.get(&key)?;
        if entry.generation == inner.generation {
            Some(entry.response.clone())
        } else {
            None
        }
    }

    /// Store a sub-agent result in the cache.
    pub fn put(&self, agent_name: &str, prompt: &str, response: &str) {
        let key = make_key(agent_name, prompt);
        if let Ok(mut inner) = self.inner.lock() {
            let current_gen = inner.generation;
            inner.entries.insert(
                key,
                CachedResult {
                    response: response.to_string(),
                    generation: current_gen,
                },
            );
        }
    }

    /// Invalidate all cache entries by bumping the generation counter.
    ///
    /// Call this when any file mutation occurs (Write, Edit, Delete, Bash)
    /// to ensure stale sub-agent results aren't reused.
    pub fn invalidate(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.generation += 1;
        }
    }

    /// Number of entries in the cache (for diagnostics/testing).
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .map(|inner| inner.entries.len())
            .unwrap_or(0)
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for SubAgentCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the cache key from agent name + hash of the prompt.
fn make_key(agent_name: &str, prompt: &str) -> CacheKey {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    prompt.hash(&mut hasher);
    (agent_name.to_string(), hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_hit_after_put() {
        let cache = SubAgentCache::new();
        cache.put("reviewer", "review this code", "looks good!");
        assert_eq!(
            cache.get("reviewer", "review this code"),
            Some("looks good!".to_string())
        );
    }

    #[test]
    fn cache_miss_different_prompt() {
        let cache = SubAgentCache::new();
        cache.put("reviewer", "review this code", "looks good!");
        assert_eq!(cache.get("reviewer", "review OTHER code"), None);
    }

    #[test]
    fn cache_miss_different_agent() {
        let cache = SubAgentCache::new();
        cache.put("reviewer", "review this", "looks good!");
        assert_eq!(cache.get("testgen", "review this"), None);
    }

    #[test]
    fn invalidation_clears_stale_entries() {
        let cache = SubAgentCache::new();
        cache.put("reviewer", "prompt", "result");
        assert!(cache.get("reviewer", "prompt").is_some());

        cache.invalidate();
        assert_eq!(cache.get("reviewer", "prompt"), None);
    }

    #[test]
    fn entries_after_invalidation_are_fresh() {
        let cache = SubAgentCache::new();
        cache.put("reviewer", "old prompt", "old result");
        cache.invalidate();
        cache.put("reviewer", "new prompt", "new result");

        // Old entry is stale
        assert_eq!(cache.get("reviewer", "old prompt"), None);
        // New entry is fresh
        assert_eq!(
            cache.get("reviewer", "new prompt"),
            Some("new result".to_string())
        );
    }

    #[test]
    fn len_tracks_entries() {
        let cache = SubAgentCache::new();
        assert!(cache.is_empty());
        cache.put("a", "p1", "r1");
        cache.put("b", "p2", "r2");
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn shared_across_clones() {
        let cache = SubAgentCache::new();
        let clone = cache.clone();
        cache.put("agent", "prompt", "result");
        assert_eq!(clone.get("agent", "prompt"), Some("result".to_string()));
    }
}
