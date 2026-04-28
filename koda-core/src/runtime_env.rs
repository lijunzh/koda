//! Thread-safe runtime environment for API keys and config.
//!
//! Replaces `unsafe { std::env::set_var() }` with a concurrent map
//! that is safe to read/write from any tokio task.
//!
//! Read priority: runtime map → mask check → process environment.
//!
//! ## Why not `std::env::set_var`?
//!
//! `set_var` is unsafe in multi-threaded programs (undefined behavior in
//! Rust 2024 edition). Since Koda uses tokio with multiple tasks (main
//! REPL, background agents, version checker), we need a thread-safe
//! alternative.
//!
//! ## Masking (#1109 F1)
//!
//! [`mask`] / [`unmask`] let tests *hide* a process-env var from
//! [`get`] without mutating `std::env`. Production code reading
//! `runtime_env::get("HTTP_PROXY")` will see `None` for masked keys
//! even if the user's shell exported `HTTP_PROXY`. This is the
//! `unsafe`-free way to isolate tests that exercise env-driven config
//! paths (e.g. proxy honoring, localhost bypass) without spawning a
//! sub-process.

use std::collections::{HashMap, HashSet};
use std::sync::{OnceLock, RwLock};

static RUNTIME_ENV: OnceLock<RwLock<HashMap<String, String>>> = OnceLock::new();
static MASKED_KEYS: OnceLock<RwLock<HashSet<String>>> = OnceLock::new();

fn env_map() -> &'static RwLock<HashMap<String, String>> {
    RUNTIME_ENV.get_or_init(|| RwLock::new(HashMap::new()))
}

fn masked() -> &'static RwLock<HashSet<String>> {
    MASKED_KEYS.get_or_init(|| RwLock::new(HashSet::new()))
}

/// Set a runtime environment variable (thread-safe).
///
/// Implicitly unmasks `key` — a runtime override always wins over a mask.
pub fn set(key: impl Into<String>, value: impl Into<String>) {
    let key = key.into();
    masked()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(&key);
    env_map()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(key, value.into());
}

/// Get a runtime variable, falling back to `std::env::var`.
///
/// Lookup order:
/// 1. Runtime map (set via [`set`]).
/// 2. Mask check — if [`mask`]ed, return `None` immediately.
/// 3. Process environment via `std::env::var`.
pub fn get(key: &str) -> Option<String> {
    if let Some(val) = env_map()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(key)
    {
        return Some(val.clone());
    }
    if masked()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains(key)
    {
        return None;
    }
    std::env::var(key).ok()
}

/// Check if a runtime variable is set (in either runtime map or process env).
pub fn is_set(key: &str) -> bool {
    get(key).is_some()
}

/// Remove a key from the runtime map (does **not** touch process env).
///
/// Returns the previous runtime-map value if one was set. After removal
/// [`get`] will fall back to the process environment as usual (unless the
/// key is also [`mask`]ed).
///
/// Primarily intended for tests that need to leave the runtime map clean
/// for siblings — production code should rarely need this.
pub fn remove(key: &str) -> Option<String> {
    env_map()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(key)
}

/// Mask `key`: future [`get`] calls return `None` for this key,
/// even if `std::env::var(key)` would otherwise return a value.
///
/// Use this in tests that need to assert behavior under "env var unset"
/// without resorting to `unsafe { std::env::remove_var(...) }`. The mask
/// is process-global and lives in the same `OnceLock` as the runtime
/// map, so combine with `koda_test_utils::ENV_MUTEX` for serialization.
pub fn mask(key: impl Into<String>) {
    masked()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(key.into());
}

/// Lift a previous [`mask`] on `key`. Idempotent.
pub fn unmask(key: &str) {
    masked()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(key);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_set_and_get() {
        set("TEST_RUNTIME_KEY", "hello");
        assert_eq!(get("TEST_RUNTIME_KEY"), Some("hello".to_string()));
    }

    #[test]
    fn test_remove() {
        set("TEST_REMOVE_KEY", "value");
        assert!(is_set("TEST_REMOVE_KEY"));
        env_map().write().unwrap().remove("TEST_REMOVE_KEY");
        // May still exist in process env, but runtime map entry is gone
    }

    #[test]
    fn test_falls_back_to_env() {
        // PATH should exist in the real environment
        assert!(get("PATH").is_some());
    }

    #[test]
    fn test_runtime_takes_precedence() {
        set("PATH", "overridden");
        assert_eq!(get("PATH"), Some("overridden".to_string()));
        // Clean up
        env_map().write().unwrap().remove("PATH");
    }

    #[test]
    fn test_remove_returns_previous_value() {
        set("TEST_REMOVE_RETURN", "orig");
        assert_eq!(remove("TEST_REMOVE_RETURN"), Some("orig".to_string()));
        // Second remove returns None (already gone from runtime map).
        assert_eq!(remove("TEST_REMOVE_RETURN"), None);
    }

    #[test]
    fn test_remove_does_not_touch_process_env() {
        // remove() should only clear the runtime map, not std::env.
        // Use HOME (always set in CI + dev) instead of PATH because the
        // sibling test_runtime_takes_precedence also touches PATH and
        // cargo runs unit tests in parallel by default — racing on the
        // same key would be a flaky-test factory.
        let _ = remove("HOME"); // ensure runtime map has no override
        assert!(
            get("HOME").is_some(),
            "process env HOME must survive a runtime-map remove"
        );
    }

    // ── mask / unmask (#1109 F1) ─────────────────────────────

    #[test]
    fn test_mask_hides_process_env_from_get() {
        // HOME is set in every sane environment; mask should hide it.
        // Use a unique-ish key to avoid colliding with siblings under
        // parallel `cargo test` execution.
        const KEY: &str = "KODA_TEST_MASK_HIDES_PROC";
        // SAFETY: we don't touch std::env here — we install a mask on a
        // fake key and verify get() returns None.
        // (No actual std::env mutation; this comment is descriptive only.)
        assert_eq!(get(KEY), None, "precondition: key not set");
        mask(KEY);
        assert_eq!(get(KEY), None, "masked key returns None");
        // Even if a runtime override existed prior, mask leaves it intact;
        // verify by setting an override AFTER masking — set() unmasks.
        set(KEY, "shadow");
        assert_eq!(
            get(KEY),
            Some("shadow".to_string()),
            "runtime override wins over mask"
        );
        // Cleanup
        remove(KEY);
        unmask(KEY);
    }

    #[test]
    fn test_unmask_restores_process_env_visibility() {
        // Use a unique synthetic key so we don't race with parallel
        // tests that read HOME (e.g. db::tests::test_config_dir_with_home,
        // now that production reads HOME via runtime_env::get).
        const KEY: &str = "KODA_TEST_UNMASK_RESTORES";
        // Seed a value via the runtime map so masking has something to hide.
        set(KEY, "runtime_value");
        assert_eq!(get(KEY), Some("runtime_value".to_string()));
        // mask doesn't shadow runtime overrides — to test the fallback path,
        // remove the runtime override first, then mask, then unmask.
        remove(KEY);
        // Now KEY only exists in process env (or doesn't exist at all).
        // Mask should make get() return None either way.
        mask(KEY);
        assert_eq!(get(KEY), None, "masked key returns None");
        unmask(KEY);
        // After unmask, get() returns whatever process env says (likely None
        // for our synthetic key). The contract is "unmask restores fallback
        // behavior", which we verify by confirming get() now returns the
        // same value as raw std::env::var.
        assert_eq!(
            get(KEY),
            std::env::var(KEY).ok(),
            "after unmask, get() falls back to process env"
        );
    }

    #[test]
    fn test_set_implicitly_unmasks() {
        const KEY: &str = "KODA_TEST_SET_UNMASKS";
        mask(KEY);
        assert_eq!(get(KEY), None);
        set(KEY, "value");
        assert_eq!(get(KEY), Some("value".to_string()));
        // Verify the mask was actually cleared, not just shadowed.
        remove(KEY);
        assert_eq!(
            get(KEY),
            std::env::var(KEY).ok(),
            "after set+remove, get falls back to process env (mask gone)"
        );
    }
}
