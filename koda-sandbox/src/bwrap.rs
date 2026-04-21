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
use anyhow::Result;
use std::path::Path;
use tokio::process::Command;

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

    // Layer 2: full deny (read+write) for koda-internal secrets only.
    // The base `--ro-bind / /` already write-protects everything else.
    for rel in CREDENTIAL_CONFIG_FULL_DENY {
        let p = format!("{home}/.config/{rel}");
        if Path::new(&p).exists() {
            cmd.args(["--tmpfs", &p]);
        }
    }

    // Layer 3: policy overlay (Phase 1b of #934).
    for arg_pair in policy_overlay_args(policy)? {
        cmd.args(&arg_pair);
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
}
