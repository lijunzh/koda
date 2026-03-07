//! Thread-safe runtime environment for API keys and config.
//!
//! Replaces `unsafe { std::env::set_var() }` with a concurrent map
//! that is safe to read/write from any tokio task.
//!
//! Read priority: runtime map → process environment.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

static RUNTIME_ENV: OnceLock<RwLock<HashMap<String, String>>> = OnceLock::new();

fn env_map() -> &'static RwLock<HashMap<String, String>> {
    RUNTIME_ENV.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Set a runtime environment variable (thread-safe).
pub fn set(key: impl Into<String>, value: impl Into<String>) {
    env_map()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(key.into(), value.into());
}

/// Get a runtime variable, falling back to `std::env::var`.
/// Checks our runtime map first, then the real process environment.
pub fn get(key: &str) -> Option<String> {
    if let Some(val) = env_map()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(key)
    {
        return Some(val.clone());
    }
    std::env::var(key).ok()
}

/// Check if a runtime variable is set (in either runtime map or process env).
pub fn is_set(key: &str) -> bool {
    get(key).is_some()
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
    fn test_missing_key() {
        assert!(get("DEFINITELY_NOT_SET_12345").is_none());
        assert!(!is_set("DEFINITELY_NOT_SET_12345"));
    }
}
