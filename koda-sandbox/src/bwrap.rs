//! Linux bwrap (bubblewrap) backend.
//!
//! Generates a `bwrap` command-line invocation that mounts the host
//! filesystem read-only, then re-binds the project root + caches as
//! read-write, and shadows koda's own secrets with `--tmpfs`.
//!
//! ## Layering
//!
//! Same conceptual three-layer overlay as the seatbelt backend:
//!
//!   1. Base view (`base_cmd`)             — `--ro-bind /` then writable carve-outs
//!   2. Hardcoded full-deny tmpfs overlays — koda secrets
//!   3. Policy overlay (`policy_overlay_args`) — caller-supplied rules
//!
//! Empty policy → behavior byte-identical to pre-#934 (Phase 0 invariant).
//!
//! ## Kernel-enforced egress (Phase 3c.1)
//!
//! When the caller supplies a proxy port AND a UDS bridge socket
//! (built by [`crate::bwrap_proxy::spawn_uds_bridge`]) is reachable,
//! [`build_command`] hands off to [`build_command_with_proxy`] which
//! adds:
//!
//!   - `--unshare-net --unshare-user --unshare-pid` — fresh net/user/pid
//!     namespaces. The fresh netns has no routes to anywhere except
//!     its own (initially-down) loopback. **This is the kernel
//!     enforcement**: direct TCP from the sandbox to anywhere
//!     fails at `connect()` with ENETUNREACH.
//!   - `--bind <uds> <uds>` — make the host UDS bridge socket
//!     reachable inside the sandbox.
//!   - Replaces `-- sh -c <cmd>` with
//!     `-- /path/to/koda-sandbox-stage2 sh -c <cmd>` so the in-netns
//!     `lo`-bring-up + TCP↔UDS bridge runs before the user command.
//!
//! ## Mapping policy → bwrap flags
//!
//! | Policy field                  | bwrap rendering                       |
//! |-------------------------------|---------------------------------------|
//! | `fs.deny_read` (subpath)      | `--tmpfs <path>` (shadows the dir)    |
//! | `fs.allow_read_within_deny`   | `--ro-bind <path> <path>` (post-shadow)|
//! | `fs.allow_write`              | `--bind <path> <path>` (writable)     |
//! | `fs.deny_write_within_allow`  | `--ro-bind <path> <path>` (post-bind) |
//!
//! bwrap is **first-match wins** for a given mountpoint, but mounts at
//! deeper paths override shallower mounts that contain them. So we emit
//! the broad rules first, then the narrower carve-outs.

#![cfg(target_os = "linux")]

use crate::defaults::{CREDENTIAL_CONFIG_FULL_DENY, PROTECTED_PROJECT_SUBDIRS};
use crate::policy::SandboxPolicy;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::process::Command;

/// Env-var override for the [`stage2_binary`] discovery (mirrors
/// `KODA_FS_WORKER_BIN`). Used by the bwrap-runtime e2e tests.
pub const STAGE2_BIN_ENV_KEY: &str = "KODA_SANDBOX_STAGE2_BIN";

/// Comma-separated list of env vars stage 2 will rewrite in-netns.
/// Kept in lockstep with [`crate::proxy::env::proxy_env_vars`] — every
/// `*_PROXY` key that function emits must appear here so stage 2's
/// rewriter visits all of them.
const STAGE2_PROXY_REWRITE_KEYS: &str =
    "HTTPS_PROXY,HTTP_PROXY,ALL_PROXY,https_proxy,http_proxy,all_proxy";

/// Build a `bwrap` Command for the given command + project root.
///
/// See module docs for layering. An empty `policy` produces the same
/// arg vector as pre-#934.
pub fn build_command(
    command: &str,
    project_root: &Path,
    policy: &SandboxPolicy,
) -> Result<Command> {
    if !is_available() {
        anyhow::bail!(
            "Sandbox requested but bwrap is not installed. \
             Install with: apt install bubblewrap  /  dnf install bubblewrap"
        );
    }

    let (mut cmd, home) = base_cmd(project_root);
    apply_credential_denies(&mut cmd, &home);
    apply_policy_overlay(&mut cmd, policy)?;
    cmd.args(["--", "sh", "-c", command])
        .current_dir(project_root);
    Ok(cmd)
}

/// Like [`build_command`] but also wires up the kernel-enforced egress
/// proxy via the stage 2 helper (Phase 3c.1.d).
///
/// `proxy_port` is the host-side TCP port the proxy is listening on
/// (used purely as a debug breadcrumb in the resulting Command —
/// stage 2 reads the in-netns ephemeral port from its own listener).
///
/// `uds_path` is the UDS bridge socket the host bridge
/// ([`crate::bwrap_proxy::spawn_uds_bridge`]) is listening on. Must
/// exist; we bind-mount it into the sandbox so stage 2 can
/// `connect()` to it.
///
/// `stage2_bin` is the absolute path to the `koda-sandbox-stage2`
/// helper binary (located via [`stage2_binary`]). The host filesystem
/// is bind-mounted read-only into the sandbox, so the same path is
/// reachable from inside.
pub fn build_command_with_proxy(
    command: &str,
    project_root: &Path,
    policy: &SandboxPolicy,
    proxy_port: u16,
    uds_path: &Path,
    stage2_bin: &Path,
) -> Result<Command> {
    if !is_available() {
        anyhow::bail!(
            "Sandbox requested but bwrap is not installed. \
             Install with: apt install bubblewrap  /  dnf install bubblewrap"
        );
    }

    let (mut cmd, home) = base_cmd(project_root);
    apply_credential_denies(&mut cmd, &home);
    apply_policy_overlay(&mut cmd, policy)?;

    // Network isolation. The fresh netns has no routes anywhere
    // except its own (initially-down) loopback — that's the kernel
    // enforcement. `--unshare-user` is required for `--unshare-net`
    // when bwrap runs unprivileged. `--unshare-pid` keeps the bridge
    // child from leaking PIDs into the host PID space (and gives stage
    // 2's PR_SET_PDEATHSIG a clean parent to track).
    cmd.args(["--unshare-net", "--unshare-user", "--unshare-pid"]);

    // Bind-mount the UDS bridge socket so stage 2 can connect() to
    // it. The base `--bind /tmp /tmp` already covers our default UDS
    // location, but emit the explicit pair so the dependency is
    // visible in snapshots and survives future base layout changes.
    let uds_str = uds_path.to_string_lossy().into_owned();
    cmd.args(["--bind", &uds_str, &uds_str]);

    // Tell stage 2 (a) where the UDS lives and (b) which env keys to
    // rewrite. Setting on the parent Command propagates to the child
    // because bwrap doesn't `--clearenv` by default.
    cmd.env(crate::bwrap_proxy::STAGE2_UDS_ENV_KEY, &uds_str);
    cmd.env(
        crate::bwrap_proxy::STAGE2_REWRITE_KEYS_ENV_KEY,
        STAGE2_PROXY_REWRITE_KEYS,
    );
    // Debug breadcrumb — never read by stage 2 itself.
    cmd.env("KODA_SANDBOX_PROXY_PORT_DEBUG", proxy_port.to_string());

    // Replace the usual `-- sh -c <cmd>` terminator with the stage 2
    // helper. Stage 2 sets up the in-netns bridge then execvp's
    // ["sh", "-c", command].
    let stage2_str = stage2_bin.to_string_lossy().into_owned();
    cmd.args(["--", &stage2_str, "sh", "-c", command])
        .current_dir(project_root);
    Ok(cmd)
}

/// Apply the credential full-deny tmpfs overlays (layer 2). Shared
/// between the proxied and unproxied build paths so they stay in
/// lockstep.
fn apply_credential_denies(cmd: &mut Command, home: &str) {
    for rel in CREDENTIAL_CONFIG_FULL_DENY {
        let p = format!("{home}/.config/{rel}");
        if Path::new(&p).exists() {
            cmd.args(["--tmpfs", &p]);
        }
    }
}

/// Apply the policy overlay (layer 3). Shared between the proxied and
/// unproxied build paths.
fn apply_policy_overlay(cmd: &mut Command, policy: &SandboxPolicy) -> Result<()> {
    for arg_pair in policy_overlay_args(policy)? {
        cmd.args(&arg_pair);
    }
    Ok(())
}

/// Locate the `koda-sandbox-stage2` helper binary.
///
/// Resolution order (mirrors [`crate::worker_client`]'s discovery):
///
/// 1. `KODA_SANDBOX_STAGE2_BIN` env var — explicit override (e2e tests).
/// 2. `CARGO_BIN_EXE_koda-sandbox-stage2` — Cargo sets this for
///    integration tests.
/// 3. Sibling of the current executable (production install).
pub fn stage2_binary() -> Result<PathBuf> {
    if let Ok(p) = std::env::var(STAGE2_BIN_ENV_KEY) {
        return Ok(PathBuf::from(p));
    }
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_koda-sandbox-stage2") {
        return Ok(PathBuf::from(p));
    }
    let mut p = std::env::current_exe().context("locate koda executable")?;
    p.set_file_name("koda-sandbox-stage2");
    if p.exists() {
        return Ok(p);
    }
    anyhow::bail!(
        "koda-sandbox-stage2 not found next to {}; set {STAGE2_BIN_ENV_KEY} to override",
        p.display()
    )
}

/// Probe whether the bwrap backend is functional. Cached.
///
/// Just checking `bwrap --version` is insufficient: bwrap may be installed
/// but unable to create sandboxes (e.g. GitHub Actions runners lack
/// unprivileged user namespaces → "setting up uid map: Permission denied").
/// Run a real sandboxed command to verify.
pub fn is_available() -> bool {
    use std::sync::OnceLock;
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::process::Command::new("bwrap")
            .args(["--ro-bind", "/", "/", "--", "true"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

/// Build a bwrap `Command` with the base project-mode filesystem view.
///
/// Returns `(cmd, home)` with everything set up *except* the final
/// `-- sh -c command` terminator — callers add that (plus any extra mounts
/// for strict mode) before spawning.
fn base_cmd(project_root: &Path) -> (Command, String) {
    let root = project_root.to_string_lossy().into_owned();
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());

    // Strategy: bind the whole root filesystem read-only, then selectively
    // add read-write binds for project + temp + common caches.
    let mut cmd = Command::new("bwrap");
    cmd.args(["--ro-bind", "/", "/"]);
    cmd.args(["--bind", &root, &root]);
    cmd.args(["--bind", "/tmp", "/tmp"]);
    if Path::new("/var/tmp").exists() {
        cmd.args(["--bind", "/var/tmp", "/var/tmp"]);
    }
    // Common package caches — avoids re-downloading on every invocation.
    for subdir in &[".cargo", ".npm", ".cache"] {
        let p = format!("{home}/{subdir}");
        if Path::new(&p).exists() {
            cmd.args(["--bind", p.as_str(), p.as_str()]);
        }
    }
    cmd.args(["--dev", "/dev"]).args(["--proc", "/proc"]);

    // Deny writes to protected project subdirs (.koda/agents, .koda/skills).
    // Re-bind as read-only after the project-root writable bind (CC parity #844).
    // Pre-create if absent so bwrap has a mountpoint — otherwise a sandboxed
    // command could `mkdir -p` and write agent definitions.
    for rel in PROTECTED_PROJECT_SUBDIRS {
        let p = format!("{root}/{rel}");
        let _ = std::fs::create_dir_all(&p);
        cmd.args(["--ro-bind", &p, &p]);
    }

    (cmd, home)
}

/// Phase 1b of #934: render `policy.fs.*` into bwrap arg pairs.
///
/// Each returned `Vec<String>` is one bwrap option pair (e.g. `["--bind",
/// "/work", "/work"]`) that the caller appends to the Command. We return
/// them as a `Vec<Vec<String>>` rather than mutating a `Command` directly
/// so this function stays pure-ish and easily snapshot-testable.
///
/// Returns an empty vec for an all-default policy, preserving the
/// pre-#934 byte-identical baseline guaranteed by [`build_command`].
pub(crate) fn policy_overlay_args(policy: &SandboxPolicy) -> Result<Vec<Vec<String>>> {
    let fs = &policy.fs;
    if fs.deny_read.is_empty()
        && fs.allow_read_within_deny.is_empty()
        && fs.allow_write.is_empty()
        && fs.deny_write_within_allow.is_empty()
    {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();

    // Layer 1: deny reads — shadow with tmpfs so the contents disappear.
    for p in &fs.deny_read {
        let s = p.as_path().to_string_lossy().into_owned();
        out.push(vec!["--tmpfs".to_string(), s]);
    }
    // Layer 2: re-bind read-only for explicit carve-outs inside denies.
    for p in &fs.allow_read_within_deny {
        let s = p.as_path().to_string_lossy().into_owned();
        out.push(vec!["--ro-bind".to_string(), s.clone(), s]);
    }
    // Layer 3: widen the writable set.
    for p in &fs.allow_write {
        let s = p.as_path().to_string_lossy().into_owned();
        out.push(vec!["--bind".to_string(), s.clone(), s]);
    }
    // Layer 4: protect specific spots inside the writable set.
    for p in &fs.deny_write_within_allow {
        let s = p.as_path().to_string_lossy().into_owned();
        out.push(vec!["--ro-bind".to_string(), s.clone(), s]);
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_overlay_args_empty_for_default_policy() {
        let args = policy_overlay_args(&SandboxPolicy::strict_default()).unwrap();
        assert!(args.is_empty(), "default policy must add zero args");
    }

    #[test]
    fn policy_overlay_args_render_deny_read_as_tmpfs() {
        let mut policy = SandboxPolicy::strict_default();
        policy.fs.deny_read = vec!["/secrets".into()];
        let args = policy_overlay_args(&policy).unwrap();
        assert_eq!(args.len(), 1);
        assert_eq!(args[0], vec!["--tmpfs", "/secrets"]);
    }

    #[test]
    fn policy_overlay_args_render_allow_write_as_bind() {
        let mut policy = SandboxPolicy::strict_default();
        policy.fs.allow_write = vec!["/work".into()];
        let args = policy_overlay_args(&policy).unwrap();
        assert_eq!(args.len(), 1);
        assert_eq!(args[0], vec!["--bind", "/work", "/work"]);
    }

    #[test]
    fn policy_overlay_args_render_deny_write_within_as_ro_bind() {
        let mut policy = SandboxPolicy::strict_default();
        policy.fs.deny_write_within_allow = vec!["/work/.git/config".into()];
        let args = policy_overlay_args(&policy).unwrap();
        assert_eq!(args.len(), 1);
        assert_eq!(
            args[0],
            vec!["--ro-bind", "/work/.git/config", "/work/.git/config"]
        );
    }

    #[test]
    fn policy_overlay_args_emit_layers_in_correct_order() {
        // Mount-shadowing semantics: deeper / later mounts override the
        // earlier ones for the same prefix. So we emit broad rules first,
        // narrower carve-outs second.
        let mut policy = SandboxPolicy::strict_default();
        policy.fs.deny_read = vec!["/secrets".into()];
        policy.fs.allow_read_within_deny = vec!["/secrets/public".into()];
        policy.fs.allow_write = vec!["/work".into()];
        policy.fs.deny_write_within_allow = vec!["/work/.git".into()];

        let args = policy_overlay_args(&policy).unwrap();
        assert_eq!(args.len(), 4);
        assert_eq!(args[0][0], "--tmpfs"); // deny_read
        assert_eq!(args[1][0], "--ro-bind"); // allow_read_within_deny
        assert_eq!(args[2][0], "--bind"); // allow_write
        assert_eq!(args[3][0], "--ro-bind"); // deny_write_within_allow
    }

    /// Phase 3c.1.d contract: the proxied build path adds the network
    /// isolation flags, the UDS bind-mount, and replaces the `sh -c`
    /// terminator with the stage 2 helper.
    #[test]
    fn build_command_with_proxy_adds_unshare_and_stage2() {
        if !is_available() {
            eprintln!("bwrap not available; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let uds = dir.path().join("bridge.sock");
        std::fs::write(&uds, b"placeholder").unwrap();
        let stage2 = dir.path().join("koda-sandbox-stage2");
        std::fs::write(&stage2, b"#!/bin/sh\n").unwrap();

        let cmd = build_command_with_proxy(
            "echo hi",
            dir.path(),
            &SandboxPolicy::strict_default(),
            12345,
            &uds,
            &stage2,
        )
        .unwrap();

        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();

        // Network isolation flags must appear.
        assert!(
            args.iter().any(|a| a == "--unshare-net"),
            "--unshare-net missing"
        );
        assert!(
            args.iter().any(|a| a == "--unshare-user"),
            "--unshare-user missing"
        );
        assert!(
            args.iter().any(|a| a == "--unshare-pid"),
            "--unshare-pid missing"
        );

        // UDS bind-mount must reference the host path on both sides.
        let uds_str = uds.to_string_lossy().to_string();
        let bind_idx = args
            .windows(3)
            .position(|w| w[0] == "--bind" && w[1] == uds_str && w[2] == uds_str)
            .expect("--bind <uds> <uds> missing");
        // Should appear *after* --unshare-net so the path resolves
        // inside the new mount namespace bwrap creates.
        let unshare_idx = args.iter().position(|a| a == "--unshare-net").unwrap();
        assert!(
            bind_idx > unshare_idx,
            "--bind UDS must follow --unshare-net (got bind@{bind_idx} unshare@{unshare_idx})"
        );

        // Terminator: -- <stage2_bin> sh -c "echo hi".
        let dash_dash = args.iter().rposition(|a| a == "--").expect("-- missing");
        assert_eq!(args[dash_dash + 1], stage2.to_string_lossy());
        assert_eq!(args[dash_dash + 2], "sh");
        assert_eq!(args[dash_dash + 3], "-c");
        assert_eq!(args[dash_dash + 4], "echo hi");
    }

    /// Companion: env vars are set on the parent Command so they
    /// propagate to the bwrap child (and through to stage 2).
    #[test]
    fn build_command_with_proxy_sets_stage2_env_vars() {
        if !is_available() {
            eprintln!("bwrap not available; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let uds = dir.path().join("bridge.sock");
        std::fs::write(&uds, b"placeholder").unwrap();
        let stage2 = dir.path().join("koda-sandbox-stage2");
        std::fs::write(&stage2, b"#!/bin/sh\n").unwrap();

        let cmd = build_command_with_proxy(
            "true",
            dir.path(),
            &SandboxPolicy::strict_default(),
            54321,
            &uds,
            &stage2,
        )
        .unwrap();

        let envs: std::collections::HashMap<String, String> = cmd
            .as_std()
            .get_envs()
            .filter_map(|(k, v)| {
                Some((
                    k.to_string_lossy().to_string(),
                    v?.to_string_lossy().to_string(),
                ))
            })
            .collect();

        assert_eq!(
            envs.get(crate::bwrap_proxy::STAGE2_UDS_ENV_KEY),
            Some(&uds.to_string_lossy().to_string()),
            "stage 2 must learn the UDS path via env"
        );
        let rewrite = envs
            .get(crate::bwrap_proxy::STAGE2_REWRITE_KEYS_ENV_KEY)
            .expect("stage 2 rewrite-keys env missing");
        // All six proxy env keys (upper + lower) must be in the list,
        // matching what proxy::env::proxy_env_vars emits. Otherwise
        // stage 2 would leave some keys pointing at the host port.
        for key in [
            "HTTPS_PROXY",
            "HTTP_PROXY",
            "ALL_PROXY",
            "https_proxy",
            "http_proxy",
            "all_proxy",
        ] {
            assert!(
                rewrite.split(',').any(|k| k == key),
                "rewrite key {key} missing from {rewrite}"
            );
        }
    }

    /// Pinned: stage 2 binary discovery honors the env override
    /// before falling back to current_exe sibling lookup. E2E tests
    /// rely on this.
    #[test]
    fn stage2_binary_respects_env_override() {
        // SAFETY: tests run single-threaded for env mutations; we
        // restore the original value after the assertion.
        let original = std::env::var(STAGE2_BIN_ENV_KEY).ok();
        unsafe { std::env::set_var(STAGE2_BIN_ENV_KEY, "/tmp/fake-stage2") };
        let p = stage2_binary().unwrap();
        assert_eq!(p, std::path::PathBuf::from("/tmp/fake-stage2"));
        unsafe {
            match original {
                Some(v) => std::env::set_var(STAGE2_BIN_ENV_KEY, v),
                None => std::env::remove_var(STAGE2_BIN_ENV_KEY),
            }
        }
    }
}
