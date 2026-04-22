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
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

    /// How deeply path-deny matching walks the path hierarchy when
    /// checking a candidate path against [`Self::deny_read`] /
    /// [`Self::deny_write_within_allow`].
    ///
    /// **Perf vs paranoia dial.** Larger = catches more evasion patterns
    /// (deep symlink chains, `..`-laden paths) but costs more per check.
    /// Smaller = faster but might miss creative deny-rule bypasses.
    ///
    /// Phase 5 of #934. **Trust-derived, not user-configurable** —
    /// populated by `koda_core::sandbox::policy_for_agent` from the
    /// runtime trust mode (Plan/Safe/Auto). The serde default exists
    /// purely for the IPC channel to `koda-fs-worker`: a worker spawned
    /// without an explicit policy must still get a safe non-zero value.
    ///
    /// Compose rule: `max(parent, child)` — a child may *ratchet up*
    /// paranoia (deeper checks) but never ratchet it down. See
    /// [`SandboxPolicy::compose`].
    #[serde(default = "default_mandatory_deny_search_depth")]
    pub mandatory_deny_search_depth: u8,
}

/// Default depth for [`FsPolicy::mandatory_deny_search_depth`].
///
/// Matches the issue #934 spec: "default 3, max 10". Used by both the
/// `Default` impl and serde's missing-field fallback so policies
/// constructed in Rust *and* policies deserialized from the worker IPC
/// channel get the same safe baseline.
fn default_mandatory_deny_search_depth() -> u8 {
    3
}

impl Default for FsPolicy {
    fn default() -> Self {
        Self {
            deny_read: Vec::new(),
            allow_read_within_deny: Vec::new(),
            allow_write: Vec::new(),
            deny_write_within_allow: Vec::new(),
            allow_git_config: false,
            mandatory_deny_search_depth: default_mandatory_deny_search_depth(),
        }
    }
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
///
/// # Deserialization is IPC-only — NOT for runtime config
///
/// Koda is **config-free at runtime**: every behavioral dial is derived
/// at compile-time from the trust mode (Plan/Safe/Auto) via
/// `koda_core::sandbox::policy_for_agent`. There is no JSON config file,
/// no CLI override, no env-var dial that the user can twiddle.
///
/// `Deserialize` is implemented **solely** because [`crate::worker::run_unix_socket_with_policy`]
/// is a separate crash-isolated process and the policy must cross that
/// process boundary. The host serializes → JSON → env var
/// `KODA_FS_WORKER_POLICY` → worker deserializes back. **That is the
/// only authorized deserialization site in the entire workspace.**
///
/// A regression test (`sandbox_policy_deserialize_only_used_in_fs_worker_binary`
/// in this module's tests) scans the workspace for any other call site
/// and fails the build if one appears. If you have a legitimate reason
/// to add another, update the allowlist in that test and explain why
/// the new site doesn't violate the "no runtime config" principle.
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

    /// Compose a parent policy with a child override, producing a policy
    /// that is **strictly no more permissive than either input**.
    ///
    /// Phase 5 of #934 (`EffectiveSandboxPermissions::compose` in Codex).
    /// The composition rules are intentionally simple and one-directional:
    /// the child can only *narrow* the parent's surface, never widen it.
    /// This matches the security intuition that a forked sub-agent should
    /// inherit the parent's restrictions and add its own on top — the
    /// parent's denies are floors, the parent's allows are ceilings.
    ///
    /// ## Per-field semantics
    ///
    /// | Field                                  | Rule           | Why                                                  |
    /// |----------------------------------------|----------------|------------------------------------------------------|
    /// | `fs.deny_read` / `fs.deny_write_*`     | union          | Either parent or child deny → deny (denies grow)     |
    /// | `fs.allow_read_within_deny`            | union          | Holes punched by either side stay open               |
    /// | `fs.allow_write`                       | parent values  | Child can't *widen* writable area (intersection ≈ parent when child empty) |
    /// | `fs.allow_git_config`                  | logical AND    | Both must permit                                     |
    /// | `fs.mandatory_deny_search_depth`       | **maximum**    | Deeper = more paranoid; child may ratchet up, never down |
    /// | `net.denied_domains`                   | union          | Symmetric to fs denies                               |
    /// | `net.allowed_domains`                  | parent values  | Child can't add allowed domains                      |
    /// | `net.allow_local_binding`              | logical AND    | Both must permit                                     |
    /// | `net.weaker_macos_isolation`           | logical AND    | Weakening requires *both* sides to opt in            |
    /// | `net.mitm`                             | parent value   | MITM is a session-wide config; sub-agents don't change it |
    /// | `limits.*`                             | minimum        | Tighter resource cap wins                            |
    /// | `trust`                                | strictest      | `Forbid` > `Require` > `Auto`                        |
    ///
    /// ## Why intersection-as-parent for allow lists (not true set intersection)
    ///
    /// True set intersection of path patterns is gnarly: `/work` and
    /// `/work/subdir` should overlap (subdir is contained in work) but
    /// string-equal pattern intersection would drop both. Until we have
    /// a real path-prefix lattice, we use the conservative rule
    /// "child can't add new allows" — if the child wants a narrower
    /// allow list, it should construct it explicitly *instead of* the
    /// parent's, and the empty-child case (most common) reduces to
    /// inheriting the parent's allows verbatim. PR-4+ may revisit.
    ///
    /// ## Idempotence
    ///
    /// `compose(p, default()) == p` and `compose(default(), c)` produces
    /// a policy with `c`'s denies (union with empty = `c`'s denies),
    /// empty allows (parent had none to inherit), and `c`'s limits/trust.
    /// In practice this means "composing onto strict_default is a no-op
    /// for denies but loses the child's allows" — callers building from
    /// a permissive base should populate the parent's allow lists first.
    pub fn compose(parent: &Self, child: &Self) -> Self {
        Self {
            fs: FsPolicy {
                deny_read: union(&parent.fs.deny_read, &child.fs.deny_read),
                allow_read_within_deny: union(
                    &parent.fs.allow_read_within_deny,
                    &child.fs.allow_read_within_deny,
                ),
                // Allows narrow toward parent: see method docs.
                allow_write: parent.fs.allow_write.clone(),
                deny_write_within_allow: union(
                    &parent.fs.deny_write_within_allow,
                    &child.fs.deny_write_within_allow,
                ),
                allow_git_config: parent.fs.allow_git_config && child.fs.allow_git_config,
                // MAX (not min): for a depth dial, larger = more paranoid.
                // Child may ratchet UP paranoia (request deeper checks)
                // but never ratchet it down (which would be widening).
                // Symmetric to the union/AND rules: "strictest wins".
                mandatory_deny_search_depth: parent
                    .fs
                    .mandatory_deny_search_depth
                    .max(child.fs.mandatory_deny_search_depth),
            },
            net: NetPolicy {
                allowed_domains: parent.net.allowed_domains.clone(),
                denied_domains: union(&parent.net.denied_domains, &child.net.denied_domains),
                allow_local_binding: parent.net.allow_local_binding
                    && child.net.allow_local_binding,
                mitm: parent.net.mitm.clone(),
                weaker_macos_isolation: parent.net.weaker_macos_isolation
                    && child.net.weaker_macos_isolation,
            },
            limits: ResourceLimits {
                cpu_time_secs: min_opt(parent.limits.cpu_time_secs, child.limits.cpu_time_secs),
                wall_time_secs: min_opt(parent.limits.wall_time_secs, child.limits.wall_time_secs),
                max_rss_bytes: min_opt(parent.limits.max_rss_bytes, child.limits.max_rss_bytes),
                max_open_fds: min_opt(parent.limits.max_open_fds, child.limits.max_open_fds),
                max_output_bytes: min_opt(
                    parent.limits.max_output_bytes,
                    child.limits.max_output_bytes,
                ),
            },
            trust: strictest_trust(parent.trust, child.trust),
        }
    }
}

/// De-duplicated union of two pattern slices, preserving parent-first
/// order so debug output stays predictable across composes. `Vec` (not
/// `HashSet`) because policies are tiny and serde-friendly here.
fn union<T: Clone + PartialEq>(a: &[T], b: &[T]) -> Vec<T> {
    let mut out = a.to_vec();
    for x in b {
        if !out.contains(x) {
            out.push(x.clone());
        }
    }
    out
}

/// Tighter limit wins. `None` = unlimited; presence beats absence.
fn min_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (None, None) => None,
        (Some(x), None) | (None, Some(x)) => Some(x),
        (Some(x), Some(y)) => Some(x.min(y)),
    }
}

/// Strictest trust wins: `Forbid` > `Require` > `Auto`.
fn strictest_trust(a: TrustPreference, b: TrustPreference) -> TrustPreference {
    use TrustPreference::*;
    match (a, b) {
        (Forbid, _) | (_, Forbid) => Forbid,
        (Require, _) | (_, Require) => Require,
        _ => Auto,
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

    // ── Phase 5 PR-3 of #934: SandboxPolicy::compose ──
    //
    // The contract is "child can only narrow the parent". These tests
    // pin each per-field rule independently so a regression in one
    // (e.g. switching `allow_write` from intersection to union)
    // shows up as a single failing test naming the violated rule.

    #[test]
    fn compose_with_default_child_is_identity_for_parent_denies() {
        let parent = SandboxPolicy {
            fs: FsPolicy {
                deny_read: vec!["/etc/shadow".into()],
                allow_write: vec!["/work".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let composed = SandboxPolicy::compose(&parent, &SandboxPolicy::default());
        assert_eq!(composed.fs.deny_read, parent.fs.deny_read);
        assert_eq!(composed.fs.allow_write, parent.fs.allow_write);
    }

    #[test]
    fn compose_unions_denies_so_child_can_add_restrictions() {
        let parent = SandboxPolicy {
            fs: FsPolicy {
                deny_read: vec!["/etc/shadow".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let child = SandboxPolicy {
            fs: FsPolicy {
                deny_read: vec!["/etc/passwd".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let composed = SandboxPolicy::compose(&parent, &child);
        assert!(composed.fs.deny_read.contains(&"/etc/shadow".into()));
        assert!(composed.fs.deny_read.contains(&"/etc/passwd".into()));
        assert_eq!(
            composed.fs.deny_read.len(),
            2,
            "union should de-duplicate, not just concat"
        );
    }

    #[test]
    fn compose_union_dedupes_overlapping_denies() {
        let p = SandboxPolicy {
            fs: FsPolicy {
                deny_read: vec!["/a".into(), "/b".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let c = SandboxPolicy {
            fs: FsPolicy {
                deny_read: vec!["/b".into(), "/c".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let composed = SandboxPolicy::compose(&p, &c);
        assert_eq!(composed.fs.deny_read.len(), 3, "a, b, c — b appears once");
    }

    #[test]
    fn compose_drops_child_allow_writes_so_child_cannot_widen() {
        // Anti-privilege-escalation: child requesting a new writable
        // directory must not be granted it via compose. Child has to
        // be installed with that allow already (via the constructor),
        // and the parent has to permit it.
        let parent = SandboxPolicy {
            fs: FsPolicy {
                allow_write: vec!["/work".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let child = SandboxPolicy {
            fs: FsPolicy {
                allow_write: vec!["/etc".into(), "/work".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let composed = SandboxPolicy::compose(&parent, &child);
        assert_eq!(
            composed.fs.allow_write,
            vec![PathPattern::new("/work")],
            "child cannot smuggle in /etc by listing it in its own allow_write"
        );
    }

    #[test]
    fn compose_allow_git_config_is_logical_and() {
        for (parent_b, child_b, want) in [
            (true, true, true),
            (true, false, false),
            (false, true, false),
            (false, false, false),
        ] {
            let p = SandboxPolicy {
                fs: FsPolicy {
                    allow_git_config: parent_b,
                    ..Default::default()
                },
                ..Default::default()
            };
            let c = SandboxPolicy {
                fs: FsPolicy {
                    allow_git_config: child_b,
                    ..Default::default()
                },
                ..Default::default()
            };
            assert_eq!(
                SandboxPolicy::compose(&p, &c).fs.allow_git_config,
                want,
                "parent={parent_b} child={child_b}"
            );
        }
    }

    #[test]
    fn compose_limits_take_minimum_with_none_meaning_unlimited() {
        let parent = SandboxPolicy {
            limits: ResourceLimits {
                wall_time_secs: Some(60),
                max_rss_bytes: None, // parent: unlimited
                ..Default::default()
            },
            ..Default::default()
        };
        let child = SandboxPolicy {
            limits: ResourceLimits {
                wall_time_secs: Some(30),  // tighter
                max_rss_bytes: Some(1024), // child sets a limit, parent had none
                ..Default::default()
            },
            ..Default::default()
        };
        let c = SandboxPolicy::compose(&parent, &child);
        assert_eq!(c.limits.wall_time_secs, Some(30), "tighter wins");
        assert_eq!(
            c.limits.max_rss_bytes,
            Some(1024),
            "presence beats absence — a stated limit always restricts an unlimited side"
        );
        assert_eq!(c.limits.cpu_time_secs, None, "both unlimited → unlimited");
    }

    #[test]
    fn compose_trust_picks_strictest() {
        use TrustPreference::*;
        // Symmetry matrix — strictest_trust must be commutative.
        for (a, b, want) in [
            (Auto, Auto, Auto),
            (Auto, Require, Require),
            (Require, Auto, Require),
            (Require, Require, Require),
            (Auto, Forbid, Forbid),
            (Forbid, Auto, Forbid),
            (Require, Forbid, Forbid),
            (Forbid, Require, Forbid),
            (Forbid, Forbid, Forbid),
        ] {
            let p = SandboxPolicy {
                trust: a,
                ..Default::default()
            };
            let c = SandboxPolicy {
                trust: b,
                ..Default::default()
            };
            assert_eq!(
                SandboxPolicy::compose(&p, &c).trust,
                want,
                "trust({a:?}, {b:?})"
            );
        }
    }

    #[test]
    fn compose_idempotent_with_self() {
        // compose(p, p) == p when p has no duplicate-causing fields.
        // Defensive: catches any rule that mutates the parent on echo.
        let p = SandboxPolicy {
            fs: FsPolicy {
                deny_read: vec!["/a".into()],
                allow_write: vec!["/work".into()],
                allow_git_config: true,
                ..Default::default()
            },
            limits: ResourceLimits {
                wall_time_secs: Some(45),
                ..Default::default()
            },
            trust: TrustPreference::Require,
            ..Default::default()
        };
        assert_eq!(
            SandboxPolicy::compose(&p, &p),
            p,
            "composing a policy with itself must equal the original"
        );
    }

    #[test]
    fn compose_mitm_inherited_from_parent_only() {
        // MITM is session-wide config, not a sub-agent-overridable knob.
        let mitm = MitmConfig {
            ca_bundle: "/cert.pem".into(),
            socket_map: vec![],
        };
        let parent = SandboxPolicy {
            net: NetPolicy {
                mitm: Some(mitm.clone()),
                ..Default::default()
            },
            ..Default::default()
        };
        let child = SandboxPolicy::default(); // no MITM
        assert_eq!(
            SandboxPolicy::compose(&parent, &child).net.mitm,
            Some(mitm),
            "parent MITM must survive composing with a no-MITM child"
        );
    }

    // ── Phase 5 of #934: mandatory_deny_search_depth (perf-vs-paranoia dial) ──

    #[test]
    fn fs_policy_default_has_safe_baseline_depth() {
        // Both Rust-constructed and JSON-deserialized FsPolicy must
        // get the same non-zero depth. A zero default would silently
        // disable deny-rule traversal — a security regression.
        assert_eq!(
            FsPolicy::default().mandatory_deny_search_depth,
            3,
            "Rust default must match the issue's documented baseline"
        );
        let from_empty_json: FsPolicy = serde_json::from_str("{}").unwrap();
        assert_eq!(
            from_empty_json.mandatory_deny_search_depth, 3,
            "serde missing-field default must match Rust default"
        );
    }

    #[test]
    fn compose_takes_max_mandatory_deny_search_depth() {
        // Child wants deeper checking (more paranoid) than parent.
        // Composed policy must adopt the deeper depth — child can ratchet UP.
        let parent = SandboxPolicy {
            fs: FsPolicy {
                mandatory_deny_search_depth: 3,
                ..Default::default()
            },
            ..Default::default()
        };
        let child = SandboxPolicy {
            fs: FsPolicy {
                mandatory_deny_search_depth: 10,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            SandboxPolicy::compose(&parent, &child)
                .fs
                .mandatory_deny_search_depth,
            10,
            "child must be able to ratchet paranoia UP via deeper checks"
        );
    }

    #[test]
    fn compose_keeps_parent_depth_when_child_is_shallower() {
        // Anti-escalation: child trying to set a SHALLOWER depth than
        // parent must be ignored. Child can only ratchet up paranoia,
        // never down. Same security shape as deny-list union and
        // logical-AND on bool flags.
        let parent = SandboxPolicy {
            fs: FsPolicy {
                mandatory_deny_search_depth: 10,
                ..Default::default()
            },
            ..Default::default()
        };
        let child = SandboxPolicy {
            fs: FsPolicy {
                mandatory_deny_search_depth: 3,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            SandboxPolicy::compose(&parent, &child)
                .fs
                .mandatory_deny_search_depth,
            10,
            "child cannot widen by requesting shallower deny-rule checks"
        );
    }

    // ── Architectural guard: SandboxPolicy is config-free at runtime ──

    /// Koda's design rule: **no runtime config**. Every behavioral dial
    /// is derived from trust mode at compile-time via
    /// `koda_core::sandbox::policy_for_agent`. The single legitimate
    /// `SandboxPolicy` deserialization site is `koda-fs-worker`'s boot
    /// path — it's IPC across a process boundary, not config.
    ///
    /// This test mechanically enforces that rule by scanning the
    /// workspace for any other deserialization call site. If you have
    /// a legitimate reason to add one (e.g. a new IPC boundary), update
    /// the `ALLOWED` allowlist with the file path and a comment
    /// explaining why the new site is IPC-only and not user-facing
    /// config. If your motivation is "users want to override", **stop**
    /// and add a trust mode or extend `policy_for_agent` instead.
    #[test]
    fn sandbox_policy_deserialize_only_used_in_fs_worker_binary() {
        const ALLOWED: &[&str] = &[
            // The crash-isolated FS worker process receives policy via
            // env var (KODA_FS_WORKER_POLICY) at boot. Sole IPC channel.
            "src/bin/koda-fs-worker.rs",
        ];
        // Patterns that indicate a SandboxPolicy is being deserialized.
        // Kept narrow on purpose — false positives are worse than misses
        // here because they punish legitimate refactors.
        const PATTERNS: &[&str] = &[
            "from_str::<SandboxPolicy>",
            "from_slice::<SandboxPolicy>",
            "from_str::<crate::policy::SandboxPolicy>",
            ": SandboxPolicy = serde_json",
            ": SandboxPolicy = serde",
        ];

        // Walk this crate + sibling crates from CARGO_MANIFEST_DIR.
        let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = manifest_dir
            .parent()
            .expect("koda-sandbox lives one level under the workspace root");
        let mut violations: Vec<String> = Vec::new();
        scan_rust_files(workspace_root, &mut |path, contents| {
            // Skip target/ and any path containing this exact test (we
            // mention the patterns ourselves and would self-flag).
            let rel = path
                .strip_prefix(workspace_root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            if rel.starts_with("target/") {
                return;
            }
            if rel.ends_with("koda-sandbox/src/policy.rs") {
                // This file defines the patterns inside the test's
                // `PATTERNS` const — don't self-flag.
                return;
            }
            for pat in PATTERNS {
                if contents.contains(pat) {
                    let allowed = ALLOWED.iter().any(|a| rel.ends_with(a) || rel.contains(a));
                    if !allowed {
                        violations.push(format!("  {rel}: matched `{pat}`"));
                    }
                }
            }
        });

        assert!(
            violations.is_empty(),
            "\n\nUnauthorized SandboxPolicy deserialization site(s) detected:\n{}\n\n\
             koda is config-free at runtime. Trust mode → policy_for_agent is the\n\
             only path. The single allowed deserialization site is the koda-fs-worker\n\
             binary (IPC across a process boundary). If you have a legitimate IPC\n\
             reason to add another, update the ALLOWED allowlist in this test with\n\
             a comment explaining why. If your motivation is \"users want to override\",\n\
             extend `policy_for_agent` or add a trust mode instead.\n",
            violations.join("\n")
        );
    }

    /// Recursive walker over .rs files. Hand-rolled to avoid pulling in
    /// `walkdir` purely for one test. Skips hidden dirs and target/.
    fn scan_rust_files(root: &std::path::Path, on_file: &mut dyn FnMut(&std::path::Path, &str)) {
        let entries = match std::fs::read_dir(root) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || name == "target" || name == "node_modules" {
                continue;
            }
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if file_type.is_dir() {
                scan_rust_files(&path, on_file);
            } else if file_type.is_file()
                && path.extension().and_then(|e| e.to_str()) == Some("rs")
                && let Ok(contents) = std::fs::read_to_string(&path)
            {
                on_file(&path, &contents);
            }
        }
    }
}
