//! Metadata collector for debug bundles.
//!
//! Produces `metadata.json` — a structured snapshot of session and runtime
//! context that a debugger (human or LLM) needs to reason about the bundle
//! without parsing the conversation. Per RFC #1167, this is intentionally
//! distinct from `messages.json` (which is the raw DB dump): metadata
//! answers "what was this session" while messages answer "what happened
//! in it."

use serde_json::{Value, json};

/// Snapshot of session and runtime context. Constructed at bundle-write
/// time, serialized to `metadata.json` inside the tarball.
///
/// Field choices follow RFC #1167:
///
/// - `session_*`, `model`, `provider`, `context_window`: session identity
/// - `totals`: at-a-glance shape of the conversation
/// - `koda_version`, `git_sha`: build provenance
/// - `started_at`, `captured_at`: timeline anchors (RFC 3339)
/// - `platform`: portable details for cross-machine reproduction
#[derive(Debug, Clone)]
pub(super) struct Metadata {
    pub session_id: String,
    pub session_title: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub context_window: Option<u64>,
    pub totals: Totals,
    pub koda_version: String,
    pub git_sha: Option<String>,
    pub started_at: Option<String>,
    pub captured_at: String,
    pub platform: Platform,
}

/// At-a-glance counts. Computed by walking the message slice once at
/// bundle-write time so the bundle reader doesn't need to re-derive them.
#[derive(Debug, Clone, Default)]
pub(super) struct Totals {
    pub user_msgs: u64,
    pub assistant_msgs: u64,
    pub tool_calls: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

/// Portable platform details. `term` is included specifically because
/// terminal-rendering bugs are a major reason to file a debug bundle.
#[derive(Debug, Clone)]
pub(super) struct Platform {
    pub os: String,
    pub arch: String,
    pub term: Option<String>,
}

impl Metadata {
    /// Render to `serde_json::Value` ready for `to_string_pretty`.
    ///
    /// Field ordering is deliberate (most-identifying first) so that even
    /// raw `cat metadata.json` is scannable.
    pub(super) fn to_json(&self) -> Value {
        json!({
            "session_id": self.session_id,
            "session_title": self.session_title,
            "model": self.model,
            "provider": self.provider,
            "context_window": self.context_window,
            "totals": {
                "user_msgs": self.totals.user_msgs,
                "assistant_msgs": self.totals.assistant_msgs,
                "tool_calls": self.totals.tool_calls,
                "tokens_in": self.totals.tokens_in,
                "tokens_out": self.totals.tokens_out,
            },
            "koda_version": self.koda_version,
            "git_sha": self.git_sha,
            "started_at": self.started_at,
            "captured_at": self.captured_at,
            "platform": {
                "os": self.platform.os,
                "arch": self.platform.arch,
                "term": self.platform.term,
            },
        })
    }
}

/// Build [`Platform`] from the running process's compile-time + env hints.
///
/// `os` and `arch` come from `std::env::consts` (build-time constants from
/// the toolchain — no runtime cost). `term` comes from `$TERM`.
pub(super) fn current_platform() -> Platform {
    Platform {
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        term: std::env::var("TERM").ok(),
    }
}

/// Compile-time koda version (`CARGO_PKG_VERSION` of the bundling crate).
///
/// This is pinned at build time so the version inside the bundle matches
/// the binary that wrote it — which is exactly what a debugger wants.
pub(super) fn current_koda_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Compile-time git SHA, if `KODA_GIT_SHA` was set during build. Optional
/// because dev builds may not have this. The release pipeline sets it.
pub(super) fn current_git_sha() -> Option<String> {
    option_env!("KODA_GIT_SHA").map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_metadata() -> Metadata {
        Metadata {
            session_id: "abc-123".to_string(),
            session_title: Some("test session".to_string()),
            model: Some("claude-sonnet-4-6".to_string()),
            provider: Some("anthropic".to_string()),
            context_window: Some(200_000),
            totals: Totals {
                user_msgs: 5,
                assistant_msgs: 5,
                tool_calls: 12,
                tokens_in: 1234,
                tokens_out: 567,
            },
            koda_version: "0.2.25".to_string(),
            git_sha: Some("deadbeef".to_string()),
            started_at: Some("2026-04-30T10:00:00Z".to_string()),
            captured_at: "2026-04-30T11:00:00Z".to_string(),
            platform: Platform {
                os: "macos".to_string(),
                arch: "aarch64".to_string(),
                term: Some("xterm-256color".to_string()),
            },
        }
    }

    #[test]
    fn json_round_trip_preserves_all_fields() {
        let meta = fixture_metadata();
        let value = meta.to_json();
        // Verify every documented field is present and equals fixture.
        assert_eq!(value["session_id"], "abc-123");
        assert_eq!(value["session_title"], "test session");
        assert_eq!(value["model"], "claude-sonnet-4-6");
        assert_eq!(value["provider"], "anthropic");
        assert_eq!(value["context_window"], 200_000);
        assert_eq!(value["totals"]["user_msgs"], 5);
        assert_eq!(value["totals"]["assistant_msgs"], 5);
        assert_eq!(value["totals"]["tool_calls"], 12);
        assert_eq!(value["totals"]["tokens_in"], 1234);
        assert_eq!(value["totals"]["tokens_out"], 567);
        assert_eq!(value["koda_version"], "0.2.25");
        assert_eq!(value["git_sha"], "deadbeef");
        assert_eq!(value["started_at"], "2026-04-30T10:00:00Z");
        assert_eq!(value["captured_at"], "2026-04-30T11:00:00Z");
        assert_eq!(value["platform"]["os"], "macos");
        assert_eq!(value["platform"]["arch"], "aarch64");
        assert_eq!(value["platform"]["term"], "xterm-256color");
    }

    #[test]
    fn json_handles_optional_nulls() {
        let mut meta = fixture_metadata();
        meta.session_title = None;
        meta.model = None;
        meta.provider = None;
        meta.context_window = None;
        meta.git_sha = None;
        meta.started_at = None;
        meta.platform.term = None;

        let value = meta.to_json();
        // Required fields still populated.
        assert_eq!(value["session_id"], "abc-123");
        assert_eq!(value["koda_version"], "0.2.25");
        assert_eq!(value["captured_at"], "2026-04-30T11:00:00Z");
        // Optionals serialize as JSON null (not as missing keys), which
        // is what `serde_json::json!` does for `Option::None`. This is
        // the contract the bundle README documents.
        assert!(value["session_title"].is_null());
        assert!(value["model"].is_null());
        assert!(value["provider"].is_null());
        assert!(value["context_window"].is_null());
        assert!(value["git_sha"].is_null());
        assert!(value["started_at"].is_null());
        assert!(value["platform"]["term"].is_null());
    }

    #[test]
    fn current_koda_version_is_workspace_version() {
        // We can't hardcode the version here (would break every release),
        // but we can sanity-check the shape: 3 dot-separated numbers.
        let v = current_koda_version();
        let parts: Vec<&str> = v.split('.').collect();
        assert_eq!(parts.len(), 3, "expected MAJOR.MINOR.PATCH, got {v}");
        for part in parts {
            assert!(
                part.chars().all(|c| c.is_ascii_digit()),
                "non-numeric version part: {part}"
            );
        }
    }

    #[test]
    fn current_platform_populates_required_fields() {
        let p = current_platform();
        // os/arch always present from std::env::consts (compile-time).
        assert!(!p.os.is_empty());
        assert!(!p.arch.is_empty());
        // term may or may not be set (e.g. in CI without TTY) — both OK.
    }
}
