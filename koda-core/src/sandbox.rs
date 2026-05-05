//! Process sandboxing for the Bash tool — thin shim over [`koda_sandbox`].
//!
//! Phase 0 of #934 lifted the actual sandbox logic into the standalone
//! `koda-sandbox` crate. This module preserves the in-tree API so the
//! existing call sites (`tools/shell.rs`, `tools/mod.rs`, `trust.rs`)
//! don't have to change.
//!
//! ## Public API
//!
//! - [`build`](crate::sandbox::build) — main entry point used by `tools/shell.rs`
//! - [`is_available`](crate::sandbox::is_available) — used by `trust.rs` to gate Auto → Safe downgrade
//! - [`is_fully_denied`](crate::sandbox::is_fully_denied) — used by `tools/mod.rs` for in-process Read/Edit
//!   parity with the kernel sandbox (#882, #898)
//!
//! ## Behavior
//!
//! All trust modes get the same kernel-enforced "strict" baseline today
//! (project sandbox + credential write-protection + full deny on
//! `~/.config/koda/db`). When the platform sandbox backend is missing
//! (e.g. `bwrap` not installed on Linux, unsupported OS), commands run
//! unsandboxed with a one-time warning — the sandbox is best-effort and
//! never blocks the user.
//!
//! Phase 1+ will let trust modes diverge by passing a richer
//! [`koda_sandbox::SandboxPolicy`] through the shim.

// `is_fully_denied` is intentionally pub(crate) but worth linking from
// these module docs for any koda-core developer reading them.
#![allow(rustdoc::private_intra_doc_links)]

use anyhow::Result;
use koda_sandbox::{
    DependencyReport, SandboxPolicy, SandboxRuntime, SandboxTransformRequest, ca_bundle_for_policy,
    current_runtime, is_available as ks_is_available, proxy_env_vars,
};

/// Re-export so callers don't have to add a direct dependency on
/// `koda-sandbox` just to read the report.
pub use koda_sandbox::DependencyReport as SandboxDependencyReport;
use std::path::Path;
use std::sync::OnceLock;
use tokio::process::Command;

/// Returns `true` if the platform sandbox backend is available.
///
/// Used by the trust layer to downgrade Auto → Safe when the sandbox
/// is unavailable, ensuring destructive ops still get a confirmation
/// prompt (#860). Cached after first probe.
pub fn is_available() -> bool {
    ks_is_available()
}

/// Detailed sandbox health report — backend identifier, availability,
/// and a human-readable reason when unavailable.
///
/// Consumed by:
///   - `parse_top_level_trust_mode` to enrich the Auto-refusal error
///     with a platform-specific install hint (#860 / #1259).
///   - `koda --version` for one-line sandbox state (bug-report aid).
///   - The TUI status bar's sandbox indicator.
///
/// This is a thin wrapper over the sandbox runtime's own health
/// check; it exists in `koda-core::sandbox` so callers don't need
/// to depend on `koda-sandbox` directly (DRY: one re-export point).
pub fn dependency_report() -> DependencyReport {
    current_runtime().check_dependencies()
}

/// Platform-specific install hint for an unavailable sandbox backend.
///
/// Returns a multi-line string describing how to install/enable the
/// named backend, or how to bypass via `--mode safe` when no install
/// path exists. Pure formatter — takes a backend identifier (from
/// [`DependencyReport::backend`]) and returns text. No I/O, no
/// platform probing.
///
/// Used by [`crate::trust::require_sandbox_for_auto`]'s caller in
/// `koda-cli` to attach actionable setup instructions to the Auto
/// refusal error (#1259 follow-up: replaced the dedicated
/// `koda doctor` subcommand with inline error enrichment because
/// the doctor's only substantive output was sandbox readiness).
///
/// Kept tiny on purpose: the real install instructions live in
/// `docs/src/sandbox.md` — this is the one-line nudge so a user
/// staring at a startup error knows what to type next.
///
/// # Examples
///
/// ```
/// use koda_core::sandbox::setup_hint;
///
/// let hint = setup_hint("bwrap");
/// assert!(hint.contains("apt install bubblewrap"));
/// assert!(hint.contains("--mode safe"));
/// ```
pub fn setup_hint(backend: &str) -> String {
    match backend {
        "bwrap" => "  Install bubblewrap:\n\
             Debian/Ubuntu:  sudo apt install bubblewrap\n\
             Fedora/RHEL:    sudo dnf install bubblewrap\n\
             Arch:           sudo pacman -S bubblewrap\n\
           Or run with `--mode safe` to keep the human in the approval loop.\n"
            .to_string(),
        "seatbelt" => "  Seatbelt is built into macOS. If it's reporting unavailable,\n\
           the `sandbox-exec` binary is missing from /usr/bin \u{2014} file an issue.\n\
           Workaround: run with `--mode safe`.\n"
            .to_string(),
        // `none` backend = unknown platform (Windows pre-sandbox-port,
        // exotic Unixes). No install path; only escape is `--mode safe`.
        _ => format!(
            "  No kernel sandbox backend exists for this platform ({}).\n\
           Run with `--mode safe` to keep the human in the approval loop.\n",
            std::env::consts::OS
        ),
    }
}

/// Re-export of [`koda_sandbox::is_fully_denied`] for in-process file tools.
///
/// "Fully denied" means both reads **and** writes are blocked. Currently
/// only `~/.config/koda/db` is fully denied — see the koda-sandbox docs
/// for the rationale and the `HOME=unset` defense-in-depth path (#898).
pub(crate) fn is_fully_denied(path: &Path) -> bool {
    koda_sandbox::is_fully_denied(path)
}

/// Build a `tokio::process::Command` that runs `sh -c "{command}"` inside
/// the appropriate sandbox.
///
/// The `_trust` argument is currently unused — all trust modes use the
/// same strict kernel-enforced baseline. Phase 1 of #934 will diverge
/// per-mode policies through this entry point.
///
/// The `proxy_port` argument is the loopback port of an in-process or
/// external HTTP CONNECT proxy spawned by [`crate::session::KodaSession`]
/// (Phase 3b of #934). When `Some`, the canonical env-var bouquet
/// (`HTTPS_PROXY`, `HTTP_PROXY`, `NO_PROXY`, lowercase variants) is
/// attached to the Command so well-behaved HTTP clients (curl, gh, npm,
/// pip, cargo, go, node, python) route their traffic through the proxy.
/// `None` preserves the pre-3b unfiltered behavior.
///
/// The `socks5_port` argument is the loopback port of the in-process
/// SOCKS5 proxy (Phase 3d.1 of #934). When `Some`, `ALL_PROXY` and
/// `all_proxy` are appended pointing at `socks5h://127.0.0.1:port` so
/// raw-TCP clients (git over ssh, gRPC tools) that ignore `HTTPS_PROXY`
/// route through hostname-filtered SOCKS5 instead of dialing direct.
/// `None` omits both vars; clients fall back to whatever they'd do
/// without `ALL_PROXY` (typically: dial direct, get filtered out by
/// the kernel-enforced sandbox layer where present).
///
/// Falls back to unsandboxed execution with a one-time warning when the
/// platform sandbox backend is unavailable. The sandbox is best-effort:
/// we never block the user just because the kernel enforcement layer is
/// missing.
pub fn build(
    command: &str,
    project_root: &Path,
    trust: &crate::trust::TrustMode,
    policy: &SandboxPolicy,
    proxy_port: Option<u16>,
    socks5_port: Option<u16>,
) -> Result<Command> {
    let runtime = current_runtime();
    warn_if_unavailable_once(runtime.as_ref());

    // Phase 5 of #934 (item 2 — `failIfUnavailable`): refuse to run
    // unsandboxed in Auto mode. Auto's whole value proposition is
    // "trust the kernel sandbox to contain auto-approved destructive
    // ops" — silently dropping that boundary at process startup
    // because `bwrap` happens to be missing is a sharp footgun.
    // #860's confirmation-prompt downgrade is a UX fallback, not a
    // security guarantee (the model can still read anywhere on the
    // FS in unsandboxed Auto).
    //
    // Safe and Plan keep the warn-and-fallback path: the user is
    // already in the approval loop (Safe) or the tool registry filters
    // writes (Plan), so sandbox is defense-in-depth there, not the
    // primary boundary. No env-var escape hatch exists by design —
    // if you don't want fail-if-unavailable, drop to Safe/Plan.
    if matches!(trust, crate::trust::TrustMode::Auto) && !is_available() {
        anyhow::bail!(
            "Kernel sandbox backend unavailable in Auto mode \u{2014} refusing to run \
             unsandboxed. Install the platform sandbox dependency (e.g. `bwrap` on \
             Linux) or switch to Safe/Plan mode (which keeps the user in the \
             approval loop)."
        );
    }

    // Phase 5 of #934 (PR-1): policy is now plumbed in from the caller
    // instead of synthesized here as `strict_default()`. This is the
    // first step in making per-agent / per-tool capability variation
    // possible. Today every caller still passes `strict_default()` so
    // behavior is byte-for-byte unchanged — PR-2 introduces real
    // construction logic; PR-3 adds resource-limit + compose() callers.
    let req = SandboxTransformRequest {
        command,
        project_root,
        policy,
        // Phase 3c: thread the proxy port into the kernel sandbox so
        // the seatbelt SBPL denies any TCP outbound that doesn't
        // target 127.0.0.1:proxy_port. Belt-and-suspenders alongside
        // the env-var bouquet (which catches well-behaved clients)
        // so even ill-behaved binaries that ignore `HTTPS_PROXY`
        // can't escape via direct TCP. On Linux this is a no-op
        // until 3c.1 ships kernel-enforcement (slirp4netns / similar).
        proxy_port,
    };

    let mut cmd = match runtime.transform(req) {
        Ok(exec) => exec.command,
        Err(e) => {
            tracing::warn!("Sandbox transform failed, running unsandboxed: {e}");
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(command).current_dir(project_root);
            cmd
        }
    };

    // Phase 5 PR-6b of #934: apply trust-derived resource limits via
    // setrlimit(2) in the child between fork and exec. Backstops the
    // wall-time ceiling for cases that wall-time can't catch (CPU
    // busy-loops that block signal delivery, malloc bombs that exhaust
    // memory before wall expires, fork bombs that exhaust FDs before
    // any wall-time tick fires). Applied on *both* the sandboxed and
    // unsandboxed-fallback paths above so a missing kernel sandbox
    // doesn't silently drop this layer too — defense in depth between
    // layers, not just inside one. No-op when no limits are set
    // (the common case today; only Auto trust mode populates them).
    koda_sandbox::rlimits::apply_to_command(&mut cmd, &policy.limits);

    // Attach the proxy env-var bouquet last so it overrides anything
    // the sandbox builder set (the builder doesn't touch HTTPS_PROXY,
    // but belt-and-suspenders). The CA bundle, when policy.net.mitm
    // is configured (Phase 3g of #934), advertises the corp PKI to
    // sandboxed subprocesses via SSL_CERT_FILE / NODE_EXTRA_CA_CERTS
    // / REQUESTS_CA_BUNDLE / CURL_CA_BUNDLE — see
    // [`koda_sandbox::proxy::env::proxy_env_vars`] for the full key
    // matrix and which runtime cares about which key.
    if let Some(port) = proxy_port {
        let ca = ca_bundle_for_policy(&policy.net);
        for (k, v) in proxy_env_vars(port, ca) {
            cmd.env(k, v);
        }
    }

    // 3d.2: append the SOCKS5 bouquet (ALL_PROXY + lowercase) when the
    // session has a SOCKS5 proxy spawned. Independent of `proxy_port` —
    // a session can have either, both, or neither, though in practice
    // [`crate::session::KodaSession`] spawns them as a pair.
    if let Some(port) = socks5_port {
        for (k, v) in koda_sandbox::socks5_env_vars(port) {
            cmd.env(k, v);
        }
    }

    Ok(cmd)
}

/// Construct the [`SandboxPolicy`] that should govern a given agent's
/// tool invocations.
///
/// Phase 5 PR-2 of #934 established the constructor and its call sites.
/// PR-3 starts populating it: every trust mode now ships with a
/// non-empty `limits.wall_time_secs` so the Bash dispatch path picks up
/// a per-agent default deadline (precedence: explicit `timeout` arg >
/// policy default > legacy hardcoded fallback). Other resource limits
/// (CPU, RSS, FDs, output) stay `None` until the runtime grows actual
/// enforcement code — the policy field exists, but no setter populates
/// it yet (YAGNI: don't promise a limit we can't enforce).
///
/// ## Per-trust defaults (today, intentionally conservative)
///
/// | Trust | wall_time_secs | rationale                                                  |
/// |-------|----------------|------------------------------------------------------------|
/// | Plan  | 60             | Read-only — 60s is more than enough for greps/reads.       |
/// | Safe  | 60             | Matches pre-PR `DEFAULT_TIMEOUT_SECS`. Byte-for-byte parity. |
/// | Auto  | 60             | Same as Safe today. Future PR may bump for build/test workloads once telemetry justifies it. |
///
/// Tuning per-trust differently is deliberately deferred — PR-3's job
/// is the *lift* (make limits policy-driven, not hardcoded), not the
/// retune. We change values once we have data; we change *where the
/// values live* now so the change is one-line later.
pub fn policy_for_agent(trust: crate::trust::TrustMode, project_root: &Path) -> SandboxPolicy {
    let mut policy = SandboxPolicy::strict_default();
    policy.limits.wall_time_secs = Some(match trust {
        crate::trust::TrustMode::Plan
        | crate::trust::TrustMode::Safe
        | crate::trust::TrustMode::Auto => 60,
    });
    // Phase 5 PR-5 of #934: deny-rule traversal depth, derived from
    // trust mode (NOT a runtime config knob — koda is config-free).
    //
    // The security argument: the more permissive the trust mode, the
    // less human gating exists, so the more paranoid the sandbox
    // should be. Plan mode is read-only (lowest blast radius, perf
    // matters because grep/read are hot); Auto has no human gate
    // (highest blast radius, max paranoia). Safe sits in between.
    //
    // Bounds match the issue #934 spec: "default 3, max 10".
    policy.fs.mandatory_deny_search_depth = match trust {
        crate::trust::TrustMode::Plan => 3,  // read-only — perf-sensitive
        crate::trust::TrustMode::Safe => 5,  // user gate exists, balanced
        crate::trust::TrustMode::Auto => 10, // no human gate — max paranoia
    };
    // Gap 3 of #1072 (PR-4 of #934): seed allow_write with the canonical
    // project root so the composition lattice accurately reflects what the
    // kernel baseline already grants.  Without this, compose()'s intersection
    // rule for allow_write is vacuously correct but has no observable effect:
    // intersect([], []) == [] regardless of what the baseline sandbox does.
    //
    // Canonicalize so symlink-based project roots don't produce a different
    // key than their realpath counterpart.  Falls back to the raw path on
    // non-existent roots — sub-agent dispatch can build policy before its
    // workspace is materialized (the test "does not panic on nonexistent
    // project root" pins this contract).
    //
    // Plan mode is intentionally included: reads dominate in Plan but writes
    // ARE permitted within the project (e.g. writing scratch files).  The
    // higher-layer tool registry is what enforces read-only semantics for
    // Plan, not the kernel sandbox write-allow list.
    let canonical_root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    policy.fs.allow_write = vec![canonical_root.clone().into()];

    // SEC-001 (v0.2.21 release audit): Gap 1 of #1072 was silently
    // neutralized on Seatbelt because the pre-overlay
    // `git_config_deny_rules` were emitted BEFORE the policy overlay's
    // `allow_write (subpath ROOT)`, and SBPL is last-match-wins. So the
    // canonical resolution for `{ROOT}/.git/config` was
    // `allow → deny → allow` → write permitted.
    //
    // Fix: seed `deny_write_within_allow` with the same git paths.
    // Both backends emit `deny_write_within_allow` AFTER `allow_write`
    // in their overlay (seatbelt: policy_overlay_rules, bwrap: Layer 4),
    // so these denies WIN the last-match resolution and actually enforce
    // the protection claimed by #1073.
    //
    // The pre-overlay `git_config_deny_rules` / `apply_git_config_deny`
    // calls are now redundant on the deny side but kept because the bwrap
    // variant has a side effect (pre-creating `.git/hooks` to close the
    // SEC-002 TOCTOU window). Cleanup of the redundant deny emission is a
    // separate refactor — security fix stays minimal.
    if !policy.fs.allow_git_config {
        policy.fs.deny_write_within_allow = vec![
            canonical_root.join(".git/hooks").into(),
            canonical_root.join(".git/config").into(),
        ];
    }

    policy
}

/// Compute a sub-agent's effective policy by composing the parent's
/// active policy with the sub-agent's per-trust derivation.
///
/// Phase 5 PR-4 of #934. Convenience wrapper over
/// [`policy_for_agent`] + [`SandboxPolicy::compose`] so the dispatch
/// site stays a one-liner and the policy-derivation logic is unit-
/// testable in isolation (the full async dispatch path is awkward to
/// drive from a unit test).
///
/// Parent-policy passing convention: callers without a meaningful
/// parent (top-level invocations, bg-spawned agents whose parent has
/// already returned) pass [`SandboxPolicy::strict_default`] — that's
/// the algebraic identity for the union/AND/min rules in `compose`,
/// so it has the effect of "child policy wins".
///
/// Returns a policy whose surface is **strictly no more permissive
/// than the parent**: see `compose` rustdoc for the per-field rules.
pub fn compose_child_policy(
    parent: &SandboxPolicy,
    sub_trust: crate::trust::TrustMode,
    project_root: &Path,
) -> SandboxPolicy {
    let child = policy_for_agent(sub_trust, project_root);
    SandboxPolicy::compose(parent, &child)
}
/// execution on, e.g., Linux without `bwrap` installed — surprising
/// behavior that the previous `build_inner()` path warned about explicitly.
fn warn_if_unavailable_once(runtime: &dyn SandboxRuntime) {
    static WARNED: OnceLock<()> = OnceLock::new();
    WARNED.get_or_init(|| {
        let report = runtime.check_dependencies();
        if !report.available {
            tracing::warn!(
                "Sandbox backend {:?} unavailable — commands run unsandboxed. {}",
                report.backend,
                report.reason.as_deref().unwrap_or("")
            );
        }
    });
}

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Phase 3b: proxy env-var injection ──────────────────────────────────

    /// `proxy_port = Some(N)` must attach the canonical env-var bouquet
    /// to the spawned Command. We verify by running `echo $HTTPS_PROXY`
    /// inside the sandbox and reading stdout — platform-agnostic and
    /// independent of the kernel sandbox availability.
    #[tokio::test]
    async fn build_attaches_proxy_env_when_port_set() {
        let dir = tempfile::tempdir().unwrap();
        let out = build(
            "echo \"$HTTPS_PROXY\"",
            dir.path(),
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            Some(31415),
            None,
        )
        .unwrap()
        .output()
        .await
        .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("http://127.0.0.1:31415"),
            "HTTPS_PROXY must be set, got stdout={stdout:?}"
        );
    }

    /// `proxy_port = None` must not set any of the bouquet vars —
    /// behavioral parity with pre-3b. Regression guard.
    #[tokio::test]
    async fn build_omits_proxy_env_when_port_none() {
        let dir = tempfile::tempdir().unwrap();
        let out = build(
            "echo \"[$HTTPS_PROXY]\"",
            dir.path(),
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
        )
        .unwrap()
        .output()
        .await
        .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        // The shell expands an unset var to the empty string; we should
        // see literal "[]" with nothing in between.
        assert!(
            stdout.contains("[]"),
            "HTTPS_PROXY must be unset, got stdout={stdout:?}"
        );
    }

    /// Phase 3d.2: `socks5_port = Some(N)` injects ALL_PROXY +
    /// all_proxy with the `socks5h://` scheme. Counterpart to
    /// `build_attaches_proxy_env_when_port_set`.
    #[tokio::test]
    async fn build_attaches_socks5_env_when_port_set() {
        let dir = tempfile::tempdir().unwrap();
        let out = build(
            "echo \"$ALL_PROXY|$all_proxy\"",
            dir.path(),
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            Some(27182),
        )
        .unwrap()
        .output()
        .await
        .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("socks5h://127.0.0.1:27182|socks5h://127.0.0.1:27182"),
            "ALL_PROXY/all_proxy must be set, got stdout={stdout:?}"
        );
    }

    /// Phase 3d.2: `socks5_port = None` must not set ALL_PROXY — some
    /// dev tools change behaviour wildly when ALL_PROXY appears (e.g.
    /// boto3 routes signing requests through it). Regression guard.
    #[tokio::test]
    async fn build_omits_socks5_env_when_port_none() {
        let dir = tempfile::tempdir().unwrap();
        let out = build(
            "echo \"[$ALL_PROXY]\"",
            dir.path(),
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
        )
        .unwrap()
        .output()
        .await
        .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("[]"),
            "ALL_PROXY must be unset, got stdout={stdout:?}"
        );
    }

    /// Phase 3d.2: HTTP and SOCKS5 are independent — setting one must
    /// not clobber or interfere with the other.
    #[tokio::test]
    async fn build_attaches_both_proxy_and_socks5_when_both_set() {
        let dir = tempfile::tempdir().unwrap();
        let out = build(
            "echo \"$HTTPS_PROXY|$ALL_PROXY\"",
            dir.path(),
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            Some(8080),
            Some(1080),
        )
        .unwrap()
        .output()
        .await
        .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("http://127.0.0.1:8080|socks5h://127.0.0.1:1080"),
            "both vars must be set with their distinct schemes, got stdout={stdout:?}"
        );
    }

    /// All six lower/UPPER proxy vars + NO_PROXY must reach the child.
    /// Defensive test: if a sed-of-bouquet refactor ever drops a var,
    /// the corresponding language ecosystem (Go reads UPPER, Python httpx
    /// reads lower) would silently bypass the proxy. Better to know.
    #[tokio::test]
    async fn build_attaches_all_proxy_var_keys() {
        let dir = tempfile::tempdir().unwrap();
        // Print each var on its own line so we can assert presence
        // without relying on shell escape order.
        let cmd = r#"
            for v in HTTPS_PROXY https_proxy HTTP_PROXY http_proxy NO_PROXY no_proxy; do
                eval "echo $v=\$$v"
            done
        "#;
        let out = build(
            cmd,
            dir.path(),
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            Some(8080),
            None,
        )
        .unwrap()
        .output()
        .await
        .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        for v in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"] {
            assert!(
                stdout.contains(&format!("{v}=http://127.0.0.1:8080")),
                "{v} missing from child env, got: {stdout:?}"
            );
        }
        assert!(stdout.contains("NO_PROXY="), "NO_PROXY missing: {stdout:?}");
        assert!(stdout.contains("no_proxy="), "no_proxy missing: {stdout:?}");
    }

    /// Sandbox build must always succeed (falls back to unsandboxed if
    /// the platform backend is unavailable, e.g. no `bwrap` on CI Linux).
    #[tokio::test]
    async fn build_always_succeeds_and_runs_echo() {
        let dir = tempfile::tempdir().unwrap();
        let status = build(
            "echo ok",
            dir.path(),
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
        )
        .unwrap()
        .status()
        .await
        .unwrap();
        assert!(status.success());
    }

    /// Project mode: writes *inside* the project root must succeed.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_allows_write_inside_project() {
        let dir = tempfile::tempdir().unwrap();
        let status = build(
            "touch sandbox_canary",
            dir.path(),
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
        )
        .unwrap()
        .status()
        .await
        .unwrap();
        assert!(status.success(), "write inside project must succeed");
        assert!(dir.path().join("sandbox_canary").exists());
    }

    /// Project mode: writes *outside* the project root must be blocked.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_blocks_write_outside_project() {
        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("evil.txt");

        let status = build(
            &format!("echo pwned > {}", target.display()),
            project.path(),
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
        )
        .unwrap()
        .status()
        .await
        .unwrap();

        assert!(!status.success(), "write outside project must be blocked");
        assert!(!target.exists(), "file must not have been created");
    }

    /// Project mode: reading outside the project must still work.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_allows_read_outside_project() {
        let dir = tempfile::tempdir().unwrap();
        // /etc/hosts is a stable readable file on every macOS system.
        let status = build(
            "cat /etc/hosts > /dev/null",
            dir.path(),
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
        )
        .unwrap()
        .status()
        .await
        .unwrap();
        assert!(status.success(), "reads outside project must be allowed");
    }

    /// Strict mode: writes inside the project must still succeed.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_strict_allows_write_inside_project() {
        let dir = tempfile::tempdir().unwrap();
        let status = build(
            "touch strict_canary",
            dir.path(),
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
        )
        .unwrap()
        .status()
        .await
        .unwrap();
        assert!(
            status.success(),
            "strict: writes inside project must succeed"
        );
        assert!(dir.path().join("strict_canary").exists());
    }

    /// Strict mode: writes outside the project must still be blocked.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_strict_blocks_write_outside_project() {
        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("evil.txt");

        let status = build(
            &format!("echo pwned > {}", target.display()),
            project.path(),
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
        )
        .unwrap()
        .status()
        .await
        .unwrap();

        assert!(!status.success(), "write outside project must be blocked");
        assert!(!target.exists(), "file must not have been created");
    }

    /// Strict mode: reads to non-sensitive paths must still work.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_strict_allows_reads_outside_sensitive() {
        let dir = tempfile::tempdir().unwrap();
        let status = build(
            "cat /etc/hosts > /dev/null",
            dir.path(),
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
        )
        .unwrap()
        .status()
        .await
        .unwrap();
        assert!(
            status.success(),
            "strict: reads to /etc/hosts must still be allowed"
        );
    }

    /// Strict mode: reading `~/.config/koda/db/` must be blocked (#847).
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_strict_blocks_koda_db_read() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/test".into());
        let db_dir = format!("{home}/.config/koda/db");
        if !Path::new(&db_dir).exists() {
            eprintln!("skip: {db_dir} does not exist");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let status = build(
            &format!("ls {db_dir}"),
            dir.path(),
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
        )
        .unwrap()
        .status()
        .await
        .unwrap();
        assert!(
            !status.success(),
            "strict: reading ~/.config/koda/db/ must be blocked"
        );
    }

    /// Strict mode: reading `~/.ssh/` must now be allowed (#855).
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_strict_allows_ssh_read() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/test".into());
        let ssh_dir = format!("{home}/.ssh");
        if !Path::new(&ssh_dir).exists() {
            eprintln!("skip: {ssh_dir} does not exist");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let status = build(
            &format!("ls {ssh_dir}"),
            dir.path(),
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
        )
        .unwrap()
        .status()
        .await
        .unwrap();
        assert!(
            status.success(),
            "strict: reading ~/.ssh/ must be allowed (CLI tools need credential access, #855)"
        );
    }

    /// Strict mode: writing to credential dirs must still be blocked.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_strict_blocks_ssh_write() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/test".into());
        let ssh_dir = format!("{home}/.ssh");
        if !Path::new(&ssh_dir).exists() {
            eprintln!("skip: {ssh_dir} does not exist");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let canary = format!("{ssh_dir}/sandbox_canary_test");
        let status = build(
            &format!("touch {canary}"),
            dir.path(),
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
        )
        .unwrap()
        .status()
        .await
        .unwrap();
        assert!(
            !status.success(),
            "strict: writing to ~/.ssh/ must be blocked"
        );
        assert!(!Path::new(&canary).exists());
    }

    // ── Integration: agent-file write protection ──────────────────────────

    /// Project mode: writing to `.koda/agents/` inside the project must be
    /// blocked (CC parity #844 — settings file write protection).
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_project_blocks_write_to_koda_agents() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".koda/agents")).unwrap();
        let target = dir.path().join(".koda/agents/evil.json");

        let status = build(
            &format!("echo '{{}}' > {}", target.display()),
            dir.path(),
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
        )
        .unwrap()
        .status()
        .await
        .unwrap();

        assert!(
            !status.success(),
            "project: writes to .koda/agents/ must be blocked"
        );
        assert!(!target.exists(), "agent file must not have been created");
    }

    /// Strict mode: same protection for `.koda/agents/`.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_strict_blocks_write_to_koda_agents() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".koda/agents")).unwrap();
        let target = dir.path().join(".koda/agents/evil.json");

        let status = build(
            &format!("echo '{{}}' > {}", target.display()),
            dir.path(),
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
        )
        .unwrap()
        .status()
        .await
        .unwrap();

        assert!(
            !status.success(),
            "strict: writes to .koda/agents/ must be blocked"
        );
        assert!(!target.exists());
    }

    /// Project mode: writing to normal project files must still work.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_project_allows_normal_writes_with_agents_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".koda/agents")).unwrap();

        let status = build(
            "touch normal_file.txt",
            dir.path(),
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
        )
        .unwrap()
        .status()
        .await
        .unwrap();

        assert!(
            status.success(),
            "project: normal writes must still work alongside agent protection"
        );
        assert!(dir.path().join("normal_file.txt").exists());
    }

    /// Project mode: writing to `.koda/skills/` inside the project must be
    /// blocked.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_project_blocks_write_to_koda_skills() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".koda/skills")).unwrap();
        let target = dir.path().join(".koda/skills/evil.md");

        let status = build(
            &format!("echo '# evil' > {}", target.display()),
            dir.path(),
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
        )
        .unwrap()
        .status()
        .await
        .unwrap();

        assert!(
            !status.success(),
            "project: writes to .koda/skills/ must be blocked"
        );
        assert!(!target.exists(), "skill file must not have been created");
    }

    // ── Integration: Linux bwrap credential enforcement ────────────────────

    /// Strict mode on Linux: `cat ~/.ssh/known_hosts` must succeed.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_strict_allows_ssh_read() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
        let ssh_dir = format!("{home}/.ssh");
        if !Path::new(&ssh_dir).exists() {
            eprintln!("skip: {ssh_dir} does not exist");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let status = build(
            &format!("ls {ssh_dir}"),
            dir.path(),
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
        )
        .unwrap()
        .status()
        .await
        .unwrap();
        assert!(
            status.success(),
            "linux strict: reading ~/.ssh/ must be allowed (CLI tools need credential access, #855)"
        );
    }

    /// Strict mode on Linux: `touch ~/.ssh/canary` must fail.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_strict_blocks_ssh_write() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
        let ssh_dir = format!("{home}/.ssh");
        if !Path::new(&ssh_dir).exists() {
            eprintln!("skip: {ssh_dir} does not exist");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let canary = format!("{ssh_dir}/bwrap_canary_test");
        let status = build(
            &format!("touch {canary}"),
            dir.path(),
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
        )
        .unwrap()
        .status()
        .await
        .unwrap();
        assert!(
            !status.success(),
            "linux strict: writing to ~/.ssh/ must be blocked"
        );
        assert!(!Path::new(&canary).exists());
    }

    /// Strict mode on Linux: `cat ~/.aws/credentials` must succeed.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_strict_allows_aws_read() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
        let aws_dir = format!("{home}/.aws");
        if !Path::new(&aws_dir).exists() {
            eprintln!("skip: {aws_dir} does not exist");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let status = build(
            &format!("ls {aws_dir}"),
            dir.path(),
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
        )
        .unwrap()
        .status()
        .await
        .unwrap();
        assert!(
            status.success(),
            "linux strict: reading ~/.aws/ must be allowed (aws CLI needs credentials)"
        );
    }

    // ── Phase 5 of #934 (item 2): Auto-mode hard-fail when sandbox missing ──
    //
    // Behavior matrix is intentionally tiny:
    //
    //   TrustMode::Auto  + sandbox unavailable  →  build() returns Err
    //   TrustMode::Safe  + sandbox unavailable  →  build() returns Ok (warn-and-fallback)
    //   TrustMode::Plan  + sandbox unavailable  →  build() returns Ok (warn-and-fallback)
    //   *                + sandbox available    →  build() returns Ok
    //
    // No env-var override exists by design: if you don't want
    // fail-if-unavailable, drop to Safe/Plan. Keeps the surface area
    // crisp and the security posture predictable across hosts.
    //
    // The unavailable-host tests skip on macOS (seatbelt always
    // present) and Linux-with-bwrap. The available-host test runs
    // everywhere is_available() is true.

    #[tokio::test]
    async fn build_errors_in_auto_mode_when_sandbox_unavailable() {
        if is_available() {
            eprintln!("skip: kernel sandbox is available on this host");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let result = build(
            "echo hi",
            dir.path(),
            &crate::trust::TrustMode::Auto,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
        );
        assert!(
            result.is_err(),
            "Auto mode without kernel sandbox must hard-error \u{2014} the whole \
             point of Auto is the kernel boundary"
        );
        let err = format!("{}", result.unwrap_err());
        assert!(
            err.contains("Auto"),
            "error message must name the offending mode so the user knows what to change: {err}"
        );
    }

    #[tokio::test]
    async fn build_falls_back_in_safe_mode_when_sandbox_unavailable() {
        if is_available() {
            eprintln!("skip: kernel sandbox is available on this host");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let result = build(
            "echo hi",
            dir.path(),
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
        );
        assert!(
            result.is_ok(),
            "Safe mode must keep the warn-and-fallback path \u{2014} the user is \
             already in the approval loop: {result:?}"
        );
    }

    #[tokio::test]
    async fn build_falls_back_in_plan_mode_when_sandbox_unavailable() {
        if is_available() {
            eprintln!("skip: kernel sandbox is available on this host");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let result = build(
            "echo hi",
            dir.path(),
            &crate::trust::TrustMode::Plan,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
        );
        assert!(
            result.is_ok(),
            "Plan mode must keep the warn-and-fallback path \u{2014} the tool registry \
             filters writes already: {result:?}"
        );
    }

    #[tokio::test]
    async fn build_succeeds_in_all_modes_when_sandbox_available() {
        if !is_available() {
            eprintln!("skip: kernel sandbox not available on this host");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        for mode in [
            crate::trust::TrustMode::Plan,
            crate::trust::TrustMode::Safe,
            crate::trust::TrustMode::Auto,
        ] {
            let result = build(
                "echo hi",
                dir.path(),
                &mode,
                &koda_sandbox::SandboxPolicy::strict_default(),
                None,
                None,
            );
            assert!(
                result.is_ok(),
                "{mode:?} must succeed when sandbox is available: {result:?}"
            );
        }
    }

    /// Phase 5 PR-6b of #934: load-bearing test that
    /// [`build()`] threads `policy.limits` into the spawned child via
    /// the rlimits `pre_exec` hook. Spawns `ulimit -n` through the
    /// returned Command and verifies the policy-supplied FD cap is
    /// observed by the child. Pins the integration between
    /// `koda_core::sandbox::build` and `koda_sandbox::rlimits` —
    /// without this test, a future refactor could silently drop the
    /// `apply_to_command` call and only the existing rlimits-module
    /// tests would catch it (which they wouldn't, since they bypass
    /// `build()`).
    #[cfg(unix)]
    #[tokio::test]
    async fn build_applies_resource_limits_to_spawned_command() {
        let dir = tempfile::tempdir().unwrap();
        let mut policy = koda_sandbox::SandboxPolicy::strict_default();
        policy.limits.max_open_fds = Some(64);

        let mut cmd = build(
            "ulimit -n",
            dir.path(),
            &crate::trust::TrustMode::Safe,
            &policy,
            None,
            None,
        )
        .expect("build must succeed (Safe mode falls back if sandbox unavailable)");

        let out = cmd.output().await.expect("spawn ok");
        assert!(out.status.success(), "child should succeed: {out:?}");
        let reported: u64 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("ulimit -n prints a number");
        assert_eq!(
            reported, 64,
            "build() must apply policy.limits.max_open_fds to the child"
        );
    }
    //
    // PR-2 left the constructor returning `strict_default()` for every
    // input. PR-3 starts populating `limits.wall_time_secs` so the
    // Bash dispatch path picks up a policy-driven default deadline.
    // PR-6b wires CPU/RSS/FD enforcement via setrlimit, but
    // `policy_for_agent` still leaves those `None` — per-trust-mode
    // values for them are a separate design call (what's a sensible
    // default RSS cap for Plan vs Auto?). When that decision lands,
    // the rlimits enforcement is already waiting.

    #[test]
    fn policy_for_agent_sets_wall_time_for_all_trust_modes() {
        let dir = tempfile::tempdir().unwrap();
        for mode in [
            crate::trust::TrustMode::Plan,
            crate::trust::TrustMode::Safe,
            crate::trust::TrustMode::Auto,
        ] {
            let policy = policy_for_agent(mode, dir.path());
            assert_eq!(
                policy.limits.wall_time_secs,
                Some(60),
                "PR-3 contract: every trust mode ships with a wall_time default \
                 so the Bash dispatch path stops needing a hardcoded fallback. \
                 Mode under test: {mode:?}"
            );
        }
    }

    #[test]
    fn policy_for_agent_leaves_other_limits_unlimited() {
        // Defensive: the runtime doesn't enforce CPU/RSS/FD/output
        // limits yet. Setting them here would lie about enforcement.
        // When PR-? wires up enforcement, update this test to assert
        // the new defaults; until then, presence-without-enforcement
        // is worse than absence (silent unlimited > silent ignored).
        let dir = tempfile::tempdir().unwrap();
        let policy = policy_for_agent(crate::trust::TrustMode::Safe, dir.path());
        assert_eq!(policy.limits.cpu_time_secs, None);
        assert_eq!(policy.limits.max_rss_bytes, None);
        assert_eq!(policy.limits.max_open_fds, None);
        assert_eq!(policy.limits.max_output_bytes, None);
    }

    #[test]
    fn policy_for_agent_does_not_panic_on_nonexistent_project_root() {
        // Defensive: the constructor must be safe to call before the
        // project root is materialized on disk (sub-agent dispatch can
        // build the policy before its workspace exists).
        let _ = policy_for_agent(
            crate::trust::TrustMode::Safe,
            std::path::Path::new("/nonexistent/path/that/should/not/exist"),
        );
    }

    // ── Phase 5 PR-5 of #934: trust-derived deny-rule traversal depth ──

    #[test]
    fn policy_for_agent_plan_mode_uses_shallow_depth() {
        // Plan is read-only (lowest blast radius). Perf > paranoia.
        let dir = tempfile::tempdir().unwrap();
        let policy = policy_for_agent(crate::trust::TrustMode::Plan, dir.path());
        assert_eq!(policy.fs.mandatory_deny_search_depth, 3);
    }

    #[test]
    fn policy_for_agent_safe_mode_uses_balanced_depth() {
        // Safe has a user gate — middle of the road.
        let dir = tempfile::tempdir().unwrap();
        let policy = policy_for_agent(crate::trust::TrustMode::Safe, dir.path());
        assert_eq!(policy.fs.mandatory_deny_search_depth, 5);
    }

    #[test]
    fn policy_for_agent_auto_mode_uses_max_depth() {
        // Auto has NO human gate — max paranoia. The security argument
        // is the load-bearing rationale for the per-trust derivation.
        let dir = tempfile::tempdir().unwrap();
        let policy = policy_for_agent(crate::trust::TrustMode::Auto, dir.path());
        assert_eq!(
            policy.fs.mandatory_deny_search_depth, 10,
            "Auto mode runs without a user gate — deep deny-rule checking is the \
             defense-in-depth that prevents creative path-evasion bypasses"
        );
    }

    #[test]
    fn policy_for_agent_depth_is_strictly_monotone_with_permissiveness() {
        // Architectural invariant: more permissive trust mode → deeper
        // (more paranoid) deny checking. If anyone changes the per-trust
        // values to violate this, they're breaking the security argument
        // documented in `policy_for_agent`. This test names the invariant
        // so the violation shows up in `git log` clearly.
        let dir = tempfile::tempdir().unwrap();
        let plan = policy_for_agent(crate::trust::TrustMode::Plan, dir.path())
            .fs
            .mandatory_deny_search_depth;
        let safe = policy_for_agent(crate::trust::TrustMode::Safe, dir.path())
            .fs
            .mandatory_deny_search_depth;
        let auto = policy_for_agent(crate::trust::TrustMode::Auto, dir.path())
            .fs
            .mandatory_deny_search_depth;
        assert!(
            plan < safe && safe < auto,
            "trust permissiveness must imply paranoia depth: \
             Plan({plan}) < Safe({safe}) < Auto({auto})"
        );
    }

    // ── Phase 5 PR-4 of #934: `compose_child_policy` wiring ──
    //
    // PR-3 added `SandboxPolicy::compose` as a pure function with its
    // own tests in koda-sandbox. PR-4 wires it into sub-agent dispatch
    // through `compose_child_policy`. These tests pin that the wrapper
    // *actually calls compose* (not just `policy_for_agent`) and that
    // the parent's restrictions are honored end-to-end.

    #[test]
    fn compose_child_policy_with_strict_default_parent_is_just_child_policy() {
        // Identity case for *most* fields: when there's no meaningful parent
        // (top-level invocation, bg-spawned agent), `strict_default()` acts
        // as the algebraic identity — the composed output matches what
        // `policy_for_agent` would have returned alone.
        //
        // Exception — `allow_write`: compose uses parent-wins semantics
        // ("allows narrow toward parent"), and `strict_default()`'s empty
        // allow list wins over the child's `[project_root]`.  This is
        // intentional: a zero-opinion parent shouldn’t grant write permissions
        // that the parent never explicitly approved.  In practice, top-level
        // agents never go through `compose_child_policy`; they use
        // `policy_for_agent` directly.  Sub-agents that DO go through compose
        // will have a real parent whose `allow_write: [parent_root]` gives
        // the child's root access via the parent-wins rule, provided the
        // parent and child share the same project root (the common case).
        let dir = tempfile::tempdir().unwrap();
        let parent = SandboxPolicy::strict_default();
        let composed = compose_child_policy(&parent, crate::trust::TrustMode::Safe, dir.path());
        let child_alone = policy_for_agent(crate::trust::TrustMode::Safe, dir.path());
        // All fields other than allow_write must match the child's policy.
        assert_eq!(composed.fs.deny_read, child_alone.fs.deny_read);
        assert_eq!(
            composed.fs.mandatory_deny_search_depth,
            child_alone.fs.mandatory_deny_search_depth
        );
        assert_eq!(
            composed.fs.allow_git_config,
            child_alone.fs.allow_git_config
        );
        assert_eq!(composed.limits, child_alone.limits);
        assert_eq!(composed.trust, child_alone.trust);
        // allow_write: parent-wins on the empty strict_default list — result is
        // empty regardless of what the child wanted.  Document as the known
        // non-identity exception, not a bug.
        assert!(
            composed.fs.allow_write.is_empty(),
            "parent-wins: strict_default's empty allow_write beats child's [root]; \
             top-level agents should NOT go through compose (use policy_for_agent \
             directly) — #1072 Gap 3 comment"
        );
    }

    // ── Gap 3 of #1072: policy_for_agent seeds allow_write ───────────────

    #[test]
    fn policy_for_agent_seeds_allow_write_with_canonical_root() {
        // Verifies #1072 Gap 3: the composition lattice only makes sense
        // if allow_write reflects what the kernel baseline actually grants.
        // Before this fix, allow_write was always [], making
        // compose()'s intersection vacuously correct but meaningless.
        let dir = tempfile::tempdir().unwrap();
        for mode in [
            crate::trust::TrustMode::Plan,
            crate::trust::TrustMode::Safe,
            crate::trust::TrustMode::Auto,
        ] {
            let policy = policy_for_agent(mode, dir.path());
            assert_eq!(
                policy.fs.allow_write.len(),
                1,
                "policy_for_agent must seed allow_write with the project root \
                 (one entry) for all trust modes, got {} entries for {mode:?}",
                policy.fs.allow_write.len()
            );
            let want = dir.path().canonicalize().unwrap();
            assert_eq!(
                policy.fs.allow_write[0].as_path(),
                want.as_path(),
                "seeded allow_write path must be the canonicalized project root"
            );
        }
    }

    #[test]
    fn policy_for_agent_allow_write_survives_nonexistent_root() {
        // Regression guard: sub-agent dispatch calls policy_for_agent
        // before the workspace is materialized on disk.  canonicalize()
        // fails on missing paths; the fallback to raw path must not panic.
        let path = std::path::Path::new("/nonexistent/project/root/xyz");
        for mode in [
            crate::trust::TrustMode::Plan,
            crate::trust::TrustMode::Safe,
            crate::trust::TrustMode::Auto,
        ] {
            let policy = policy_for_agent(mode, path);
            assert_eq!(
                policy.fs.allow_write.len(),
                1,
                "even with a missing root, allow_write must have one entry"
            );
        }
    }

    // ── SEC-001 (v0.2.21 release audit) ──────────────────────────

    #[test]
    fn policy_for_agent_seeds_git_deny_when_allow_git_config_is_false() {
        // SEC-001: without this seed, the pre-overlay git denies in
        // build_command_inner are last-match-overridden by the overlay's
        // allow_write, neutralizing the entire #1073 protection.
        // strict_default() sets allow_git_config = false, so every trust
        // mode must seed the deny pair.
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        for mode in [
            crate::trust::TrustMode::Plan,
            crate::trust::TrustMode::Safe,
            crate::trust::TrustMode::Auto,
        ] {
            let policy = policy_for_agent(mode, dir.path());
            assert!(
                !policy.fs.allow_git_config,
                "strict_default precondition: allow_git_config must be false"
            );
            let denies: Vec<&std::path::Path> = policy
                .fs
                .deny_write_within_allow
                .iter()
                .map(|p| p.as_path())
                .collect();
            assert!(
                denies.contains(&canonical.join(".git/hooks").as_path()),
                "{mode:?}: deny_write_within_allow must contain .git/hooks, got {denies:?}"
            );
            assert!(
                denies.contains(&canonical.join(".git/config").as_path()),
                "{mode:?}: deny_write_within_allow must contain .git/config, got {denies:?}"
            );
        }
    }

    #[test]
    fn compose_child_policy_inherits_parent_denies() {
        // Parent restriction that the child doesn't know about must
        // survive into the composed policy. This is the load-bearing
        // contract: a parent that bans /etc/secrets can't be widened
        // by a child that has no opinion on /etc/secrets.
        let dir = tempfile::tempdir().unwrap();
        let mut parent = SandboxPolicy::strict_default();
        parent
            .fs
            .deny_read
            .push(koda_sandbox::PathPattern::new("/etc/secrets"));
        let composed = compose_child_policy(&parent, crate::trust::TrustMode::Safe, dir.path());
        assert!(
            composed
                .fs
                .deny_read
                .contains(&koda_sandbox::PathPattern::new("/etc/secrets")),
            "parent's deny_read must survive composing with the child"
        );
    }

    #[test]
    fn compose_child_policy_takes_tighter_wall_time() {
        // When parent has wall_time=10 and child policy_for_agent says
        // 60, the composed wall_time must be 10 (min). This proves
        // compose's `min_opt` rule is reachable through the wrapper.
        let dir = tempfile::tempdir().unwrap();
        let mut parent = SandboxPolicy::strict_default();
        parent.limits.wall_time_secs = Some(10);
        let composed = compose_child_policy(&parent, crate::trust::TrustMode::Safe, dir.path());
        assert_eq!(
            composed.limits.wall_time_secs,
            Some(10),
            "parent's tighter wall_time must beat the child's looser default"
        );
    }

    #[test]
    fn compose_child_policy_takes_strictest_trust() {
        // Parent: Forbid (most restrictive). Child: Auto (least). Result: Forbid.
        let dir = tempfile::tempdir().unwrap();
        let mut parent = SandboxPolicy::strict_default();
        parent.trust = koda_sandbox::TrustPreference::Forbid;
        let composed = compose_child_policy(&parent, crate::trust::TrustMode::Auto, dir.path());
        assert_eq!(
            composed.trust,
            koda_sandbox::TrustPreference::Forbid,
            "strictest trust wins regardless of which side it came from"
        );
    }
}
