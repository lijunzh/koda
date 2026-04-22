//! Sandbox policy data types — the schema callers use to describe the
//! capabilities a sandbox slot should have.
//!
//! Schema mirrors the design in <https://github.com/lijunzh/koda/issues/934>
//! §4.2. The *structure* is in place from Phase 0 onward; the *enforcement*
//! of individual fields lands incrementally across phases:
//!
//! | Phase | Fields enforced                                                  |
//! |-------|------------------------------------------------------------------|
//! | 0     | (none — seatbelt/bwrap builders use built-in defaults)           |
//! | 1     | `fs.deny_read`, `fs.allow_write`, `fs.deny_write_within_allow`   |
//! | 3     | `net.*`                                                          |
//! | 5     | `limits.*`, `trust`                                              |
//!
//! Until a phase lands, the corresponding fields are *parsed and stored*
//! but ignored by the runtime. This intentional "schema first, enforcement
//! second" sequencing keeps wire formats stable across phase rollouts.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Path pattern for FS policy rules.
///
/// Phase 0–1: plain absolute path, exact-prefix matching.
/// Phase 2+: may grow glob/regex variants behind an enum once the
/// requirement is real (YAGNI).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PathPattern(pub PathBuf);

impl PathPattern {
    /// Construct a new pattern from any path-convertible value.
    pub fn new(p: impl Into<PathBuf>) -> Self {
        Self(p.into())
    }

    /// Borrow the underlying path.
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

impl<P: Into<PathBuf>> From<P> for PathPattern {
    fn from(p: P) -> Self {
        Self::new(p)
    }
}

/// Domain pattern for network policy rules.
///
/// Phase 0–2: raw string, exact match.
/// Phase 3: wildcard support (`*.npmjs.org`) — semantics TBD with the
/// proxy implementation so the consumer can compile the pattern once.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DomainPattern(pub String);

impl DomainPattern {
    /// Construct a new pattern from any string-convertible value.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

/// Filesystem policy. Two-layer rules per Claude Code's pattern: an outer
/// allow/deny plus a same-direction inner exception list (e.g. allow writes
/// to `~/work` *except* `~/work/.secrets`).
///
/// Phase 0 ignores all fields — the seatbelt/bwrap builders use the
/// hardcoded defaults from [`crate::defaults`]. Phase 1 wires them in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct FsPolicy {
    /// Paths whose reads are denied at the kernel layer.
    pub deny_read: Vec<PathPattern>,

    /// Carve-outs *inside* `deny_read` that remain readable.
    pub allow_read_within_deny: Vec<PathPattern>,

    /// Paths whose writes are permitted. Anything not in this list is
    /// write-denied (allowlist semantics — matches Codex `writable_roots`).
    pub allow_write: Vec<PathPattern>,

    /// Carve-outs *inside* `allow_write` that remain write-denied
    /// (e.g. `.koda/agents` inside the project root).
    pub deny_write_within_allow: Vec<PathPattern>,

    /// Whether `git config` writes are permitted. Off by default to
    /// prevent agents from registering `core.fsmonitor` hooks → RCE.
    pub allow_git_config: bool,
}

/// Network egress policy. All fields are Phase-3 territory — the runtime
/// ignores them in Phase 0–2.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct NetPolicy {
    /// Domain allowlist. Empty + `denied_domains` empty == allow-all
    /// (Phase 0 behavior). Empty + `denied_domains` nonempty == deny-list
    /// semantics. Nonempty == allow-list semantics, denies override.
    pub allowed_domains: Vec<DomainPattern>,

    /// Always-denied domains (overrides `allowed_domains`).
    pub denied_domains: Vec<DomainPattern>,

    /// Whether sandboxed processes may bind to local ports (e.g. dev servers).
    pub allow_local_binding: bool,

    /// Optional MITM proxy chaining (corporate CA support). When `Some`,
    /// outbound TLS is decrypted and re-encrypted via the configured CA.
    pub mitm: Option<MitmConfig>,

    /// macOS-only: relax network sandboxing to permit Apple `trustd`
    /// callbacks and Go-binary TLS verification. Off by default.
    pub weaker_macos_isolation: bool,
}

/// Corporate MITM proxy configuration.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MitmConfig {
    /// Path to the trusted CA bundle (Zscaler / corp PKI).
    pub ca_bundle: PathBuf,

    /// Per-domain Unix socket map: traffic to `domain` is forwarded to
    /// the specified socket instead of the default proxy.
    #[serde(default)]
    pub socket_map: Vec<(String, PathBuf)>,
}

/// Per-process resource limits. Phase 0 placeholder; enforcement lands in
/// Phase 5 per the issue's roadmap. Using `Option<u64>` so absent ==
/// "no limit" without needing a magic sentinel value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourceLimits {
    /// Max CPU time (seconds). `None` = unlimited.
    pub cpu_time_secs: Option<u64>,
    /// Max wall-clock time (seconds). `None` = unlimited.
    pub wall_time_secs: Option<u64>,
    /// Max resident set size (bytes). `None` = unlimited.
    pub max_rss_bytes: Option<u64>,
    /// Max open file descriptors. `None` = unlimited.
    pub max_open_fds: Option<u64>,
    /// Max stdout/stderr bytes per process. `None` = unlimited.
    pub max_output_bytes: Option<u64>,
}

/// Codex-style trust preference. Orthogonal to the FS/net policy: this
/// controls *whether the user is asked*, not *what is allowed*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustPreference {
    /// Auto-approve any tool call the policy permits.
    #[default]
    Auto,
    /// Always require explicit user confirmation, even when allowed.
    Require,
    /// Never run; reject regardless of policy verdict.
    Forbid,
}

/// Top-level sandbox policy. One per slot; sub-agent slots inherit and
/// `restrict()` from the parent (Phase 5 — `EffectiveSandboxPermissions::compose`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxPolicy {
    /// Filesystem rules (read/write deny + allow).
    pub fs: FsPolicy,
    /// Network egress rules.
    pub net: NetPolicy,
    /// Per-process resource limits.
    pub limits: ResourceLimits,
    /// Whether to ask the user before running (Codex pattern).
    pub trust: TrustPreference,
}

impl SandboxPolicy {
    /// Phase 0 sentinel: the policy used by [`crate::current_runtime`]'s
    /// transform when called via the legacy `koda-core::sandbox::build()`
    /// shim. All fields empty/default — runtime falls back to the
    /// hardcoded defaults in [`crate::defaults`], which preserves the
    /// pre-#934 Strict-mode behavior byte-for-byte.
    ///
    /// Phase 1+ callers should construct policies explicitly.
    pub fn strict_default() -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_is_empty_and_auto() {
        let p = SandboxPolicy::default();
        assert!(p.fs.deny_read.is_empty());
        assert!(p.fs.allow_write.is_empty());
        assert!(p.net.allowed_domains.is_empty());
        assert_eq!(p.trust, TrustPreference::Auto);
    }

    #[test]
    fn strict_default_matches_default() {
        // Phase 0 invariant: strict_default() is just default(). Future
        // phases may diverge.
        assert_eq!(SandboxPolicy::strict_default(), SandboxPolicy::default());
    }

    #[test]
    fn path_pattern_serde_is_transparent() {
        let p = PathPattern::new("/etc/passwd");
        let json = serde_json::to_string(&p).unwrap();
        // Transparent serde — serializes as the raw path string, not
        // a wrapped object. Keeps wire format ergonomic.
        assert_eq!(json, "\"/etc/passwd\"");
        let parsed: PathPattern = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, p);
    }

    #[test]
    fn domain_pattern_serde_is_transparent() {
        let d = DomainPattern::new("github.com");
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(json, "\"github.com\"");
    }

    #[test]
    fn trust_preference_lowercase_serde() {
        assert_eq!(
            serde_json::to_string(&TrustPreference::Auto).unwrap(),
            "\"auto\""
        );
        assert_eq!(
            serde_json::to_string(&TrustPreference::Require).unwrap(),
            "\"require\""
        );
        assert_eq!(
            serde_json::to_string(&TrustPreference::Forbid).unwrap(),
            "\"forbid\""
        );
    }

    #[test]
    fn policy_round_trips_through_json() {
        let p = SandboxPolicy {
            fs: FsPolicy {
                deny_read: vec!["/etc/shadow".into()],
                allow_write: vec!["/tmp".into(), "/work".into()],
                allow_git_config: true,
                ..Default::default()
            },
            net: NetPolicy {
                allowed_domains: vec![DomainPattern::new("github.com")],
                ..Default::default()
            },
            ..Default::default()
        };
        let json = serde_json::to_string(&p).unwrap();
        let parsed: SandboxPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, p);
    }

    #[test]
    fn policy_accepts_partial_json_via_serde_default() {
        // Wire-format stability: callers can omit any subtree and get
        // sensible defaults. Critical for forward-compat as new fields
        // are added across phases.
        let json = r#"{"fs": {"allow_write": ["/tmp"]}}"#;
        let p: SandboxPolicy = serde_json::from_str(json).unwrap();
        assert_eq!(p.fs.allow_write, vec![PathPattern::new("/tmp")]);
        assert_eq!(p.trust, TrustPreference::Auto);
        assert!(p.net.allowed_domains.is_empty());
    }
}
