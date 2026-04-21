//! Linux bwrap (bubblewrap) backend.
//!
//! Generates a `bwrap` command-line invocation that mounts the host
//! filesystem read-only, then re-binds the project root + caches as
//! read-write, and shadows koda's own secrets with `--tmpfs`.
//!
//! ## Phase 0
//!
//! Lifted verbatim from `koda-core/src/sandbox.rs::linux_*`. The
//! [`crate::policy::SandboxPolicy`] argument is currently *unused* — the
//! arg vector is built from the [`crate::defaults`] baseline, which
//! preserves pre-#934 byte-for-byte behavior.
//!
//! ## Phase 1
//!
//! `policy.fs.allow_write` becomes additional `--bind` args;
//! `policy.fs.deny_read` becomes `--tmpfs` overlays. The two-layer rules
//! map naturally onto bwrap's bind-then-overlay mechanism.

#![cfg(target_os = "linux")]

use crate::defaults::{CREDENTIAL_CONFIG_FULL_DENY, PROTECTED_PROJECT_SUBDIRS};
use crate::policy::SandboxPolicy;
use anyhow::Result;
use std::path::Path;
use tokio::process::Command;

/// Build a `bwrap` Command for the given command + project root.
///
/// Phase 0: `_policy` is accepted but ignored — the arg vector is the
/// hardcoded "strict" baseline that matches pre-#934 behavior.
pub fn build_command(
    command: &str,
    project_root: &Path,
    _policy: &SandboxPolicy,
) -> Result<Command> {
    if !is_available() {
        anyhow::bail!(
            "Sandbox requested but bwrap is not installed. \
             Install with: apt install bubblewrap  /  dnf install bubblewrap"
        );
    }

    let (mut cmd, home) = base_cmd(project_root);

    // Full deny (read+write) for koda-internal secrets only.
    // The base `--ro-bind / /` already write-protects everything else.
    for rel in CREDENTIAL_CONFIG_FULL_DENY {
        let p = format!("{home}/.config/{rel}");
        if Path::new(&p).exists() {
            cmd.args(["--tmpfs", &p]);
        }
    }

    cmd.args(["--", "sh", "-c", command])
        .current_dir(project_root);
    Ok(cmd)
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
