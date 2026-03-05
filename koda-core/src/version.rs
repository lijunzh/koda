//! Version checker: non-blocking startup check for newer crate versions.
//!
//! Spawns a background task that queries crates.io for `koda-cli`.
//! If a newer version exists, prints a one-line hint after the banner.
//! Never blocks startup.

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
}
