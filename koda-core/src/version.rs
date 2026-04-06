//! Version checker — non-blocking startup check for newer crate versions.
//!
//! Spawns a background task that queries crates.io for `koda-cli`.
//! If a newer version exists, prints a one-line hint after the banner.
//! Never blocks startup — the REPL is ready immediately.
//!
//! ## Behavior
//!
//! - Runs once per session at startup
//! - Timeout: 3 seconds (fails silently on slow networks)
//! - Compares `semver::Version` of current binary vs latest on crates.io
//! - Shows: `📦 Update available: 0.2.1 → 0.3.0 (cargo install koda-cli)`

use std::time::Duration;

const CRATE_NAME: &str = "koda-cli";
const CRATES_IO_URL: &str = "https://crates.io/api/v1/crates/koda-cli";
const CHECK_TIMEOUT: Duration = Duration::from_secs(3);

/// Spawn a background version check. Returns a handle that can be awaited.
pub fn spawn_version_check() -> tokio::task::JoinHandle<Option<String>> {
    tokio::spawn(async move { check_latest_version().await })
}

/// Check whether `latest` is newer than the current version.
/// Returns `Some((current, latest))` if an update is available.
pub fn update_available(latest: &str) -> Option<(&'static str, String)> {
    let current = env!("CARGO_PKG_VERSION");
    if latest != current && is_newer(latest, current) {
        Some((current, latest.to_string()))
    } else {
        None
    }
}

/// The crate name, useful for building install commands.
pub fn crate_name() -> &'static str {
    CRATE_NAME
}

/// Query crates.io for the latest version.
async fn check_latest_version() -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(CHECK_TIMEOUT)
        .build()
        .ok()?;

    let resp = client
        .get(CRATES_IO_URL)
        .header(
            "User-Agent",
            format!("Koda/{} (version-check)", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    let body: serde_json::Value = resp.json().await.ok()?;
    body.get("crate")?
        .get("max_version")?
        .as_str()
        .map(|s| s.to_string())
}

/// Simple semver comparison: is `a` newer than `b`?
fn is_newer(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> { s.split('.').filter_map(|p| p.parse().ok()).collect() };
    let va = parse(a);
    let vb = parse(b);
    va > vb
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_newer() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
    }

    #[test]
    fn test_is_newer_same_version() {
        assert!(!is_newer("0.1.0", "0.1.0"));
    }

    #[test]
    fn test_is_newer_single_component() {
        assert!(is_newer("2", "1"));
        assert!(!is_newer("1", "2"));
    }

    #[test]
    fn test_is_newer_patch_only() {
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(!is_newer("0.1.9", "0.1.10"));
    }

    #[test]
    fn test_crate_name() {
        assert_eq!(crate_name(), "koda-cli");
    }

    #[test]
    fn test_update_available_same_version_returns_none() {
        let current = env!("CARGO_PKG_VERSION");
        // Same version → no update
        assert!(update_available(current).is_none());
    }

    #[test]
    fn test_update_available_older_version_returns_none() {
        // "0.0.1" is almost certainly older than any real build
        assert!(update_available("0.0.1").is_none());
    }

    #[test]
    fn test_update_available_future_version_returns_some() {
        // "999.0.0" is always newer than any real build
        let result = update_available("999.0.0");
        assert!(result.is_some());
        let (current, latest) = result.unwrap();
        assert_eq!(latest, "999.0.0");
        assert_eq!(current, env!("CARGO_PKG_VERSION"));
    }
}
