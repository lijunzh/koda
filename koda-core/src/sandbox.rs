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
    SandboxPolicy, SandboxRuntime, SandboxTransformRequest, ca_bundle_for_policy, current_runtime,
    is_available as ks_is_available, proxy_env_vars,
};
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
    _trust: &crate::trust::TrustMode,
    proxy_port: Option<u16>,
    socks5_port: Option<u16>,
) -> Result<Command> {
    let runtime = current_runtime();
    warn_if_unavailable_once(runtime.as_ref());

    let policy = SandboxPolicy::strict_default();
    let req = SandboxTransformRequest {
        command,
        project_root,
        policy: &policy,
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

/// Emit a single warning per process if the active runtime can't enforce
/// kernel-level isolation. Without this users get silent unsandboxed
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
}
