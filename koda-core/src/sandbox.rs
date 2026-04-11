//! Process sandboxing for the Bash tool.
//!
//! Sandboxing is **opt-in** (`--sandbox project`; default: `none`) and
//! applied per-session.  The goal of "project" mode is to prevent the model
//! from accidentally — or adversarially — writing outside the current project
//! directory.
//!
//! ## Platform backends
//!
//! | Platform | Backend              | Fallback on unavailable |
//! |----------|----------------------|-------------------------|
//! | macOS    | `sandbox-exec -p`    | warn + run unsandboxed  |
//! | Linux    | `bwrap` (bubblewrap) | warn + run unsandboxed  |
//!
//! ## Modes
//!
//! - **`none`** (default) — no sandbox; full host access.
//! - **`project`** — reads everywhere; writes restricted to `{project_root}`,
//!   `/tmp`, `/var/tmp`, and common cache dirs (`~/.cargo`, `~/.npm`,
//!   `~/.cache`).  Network is unrestricted.
//!
//! `strict` mode (deny reads of sensitive dirs like `~/.ssh`, `~/.aws`) is
//! planned for a future release (#840 v2) and is deliberately left as a stub.
//!
//! ## Usage
//!
//! ```rust,ignore
//! let cmd = sandbox::build("cargo test", project_root, &SandboxMode::Project);
//! let child = cmd.stdout(Stdio::piped()).spawn()?;
//! ```

use std::path::Path;
use tokio::process::Command;

// ── Mode ─────────────────────────────────────────────────────────────────────

/// Which sandboxing level to apply to Bash tool invocations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SandboxMode {
    /// No sandbox — commands run with full host access. (default)
    #[default]
    None,
    /// Restrict writes to the project dir + /tmp + cache dirs.
    /// Reads and network are unrestricted.
    Project,
}

impl SandboxMode {
    /// Parse from a CLI / env string (case-insensitive).
    ///
    /// Unknown values produce a warning and fall back to `None`.
    pub fn parse(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "project" => Self::Project,
            "none" | "" => Self::None,
            other => {
                tracing::warn!(
                    "Unknown --sandbox value {:?} — defaulting to none. \
                     Valid values: none, project",
                    other
                );
                Self::None
            }
        }
    }

    /// `true` when sandboxing is active (i.e. not `None`).
    pub fn is_active(&self) -> bool {
        self != &Self::None
    }
}

impl std::fmt::Display for SandboxMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => f.write_str("none"),
            Self::Project => f.write_str("project"),
        }
    }
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Build a `tokio::process::Command` that runs `sh -c "{command}"` inside
/// the appropriate sandbox for `mode`.
///
/// When `mode` is [`SandboxMode::Project`] but the platform backend is
/// unavailable (e.g. `bwrap` not installed on Linux), a warning is logged
/// and an **unsandboxed** `Command` is returned — the caller does not need to
/// handle the failure case.
pub fn build(command: &str, project_root: &Path, mode: &SandboxMode) -> Command {
    match mode {
        SandboxMode::None => plain_sh(command, project_root),
        SandboxMode::Project => build_project(command, project_root),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn plain_sh(command: &str, project_root: &Path) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command).current_dir(project_root);
    cmd
}

/// Dispatch to the platform-specific "project" sandbox builder.
fn build_project(command: &str, project_root: &Path) -> Command {
    #[cfg(target_os = "macos")]
    return macos_project(command, project_root);

    #[cfg(target_os = "linux")]
    return linux_project(command, project_root);

    // Should never compile — koda-cli enforces unix-only at compile time.
    #[allow(unreachable_code)]
    plain_sh(command, project_root)
}

// ── macOS: sandbox-exec -p <profile string> ───────────────────────────────────

#[cfg(target_os = "macos")]
fn macos_project(command: &str, project_root: &Path) -> Command {
    // Resolve symlinks so the seatbelt subpath matcher sees the canonical
    // path.  On macOS, /var is a symlink to /private/var; tempfile dirs land
    // under /var/folders/… which the kernel presents as /private/var/folders/….
    // Without canonicalization, `(subpath "/var/folders/…")` would never match.
    let canonical = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    let root = canonical.to_string_lossy();
    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users".into());

    // Seatbelt profile (Apple's Scheme-like DSL).
    //
    // Strategy: deny default (whitelist-only), then open reads everywhere,
    // restrict writes to project + temp + cache dirs, leave network open so
    // curl / cargo fetch / npm install work unmodified.
    //
    // Passing the profile via `-p` (inline string) avoids the need for a
    // temporary file and the race window around deleting it.
    let profile = format!(
        "(version 1)\n\
         (deny default)\n\
         (allow file-read*)\n\
         (allow file-write*\n\
           (subpath \"{root}\")\n\
           (subpath \"/private/tmp\")\n\
           (subpath \"/tmp\")\n\
           (subpath \"{home}/.cargo\")\n\
           (subpath \"{home}/.npm\")\n\
           (subpath \"{home}/.cache\")\n\
           (literal \"/dev/null\")\n\
           (literal \"/dev/stderr\")\n\
           (literal \"/dev/stdout\")\n\
           (literal \"/dev/urandom\"))\n\
         (allow network*)\n\
         (allow process-exec*)\n\
         (allow process-fork)\n\
         (allow sysctl-read)\n\
         (allow ipc-posix*)\n\
         (allow mach*)\n"
    );

    let mut cmd = Command::new("sandbox-exec");
    cmd.arg("-p")
        .arg(profile)
        .arg("sh")
        .arg("-c")
        .arg(command)
        .current_dir(project_root);
    cmd
}

// ── Linux: bwrap (bubblewrap) ─────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn linux_project(command: &str, project_root: &Path) -> Command {
    // Detect bwrap once — subsequent calls reuse the cached result.
    use std::sync::OnceLock;
    static BWRAP_AVAILABLE: OnceLock<bool> = OnceLock::new();

    let available = BWRAP_AVAILABLE.get_or_init(|| {
        std::process::Command::new("bwrap")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
    });

    if !*available {
        tracing::warn!(
            "sandbox=project requested but bwrap is not installed. \
             Running without sandbox. \
             Install with: apt install bubblewrap  /  dnf install bubblewrap"
        );
        return plain_sh(command, project_root);
    }

    let root = project_root.to_string_lossy();
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());

    // Strategy: bind the whole root filesystem read-only, then selectively
    // add read-write binds for project + temp + common caches.
    let mut cmd = Command::new("bwrap");

    // Whole root: read-only
    cmd.args(["--ro-bind", "/", "/"]);

    // Project dir: read-write
    cmd.args(["--bind", root.as_ref(), root.as_ref()]);

    // Temp dirs: read-write
    cmd.args(["--bind", "/tmp", "/tmp"]);
    if Path::new("/var/tmp").exists() {
        cmd.args(["--bind", "/var/tmp", "/var/tmp"]);
    }

    // Common package caches — avoids re-downloading every run
    for subdir in &[".cargo", ".npm", ".cache"] {
        let p = format!("{home}/{subdir}");
        if Path::new(&p).exists() {
            cmd.args(["--bind", p.as_str(), p.as_str()]);
        }
    }

    // Device and proc pseudo-filesystems
    cmd.args(["--dev", "/dev"]).args(["--proc", "/proc"]);

    // The actual command
    cmd.args(["--", "sh", "-c", command])
        .current_dir(project_root);

    cmd
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Unit: enum behaviour ───────────────────────────────────────────────

    #[test]
    fn parse_roundtrip() {
        assert_eq!(SandboxMode::parse("none"), SandboxMode::None);
        assert_eq!(SandboxMode::parse("project"), SandboxMode::Project);
        assert_eq!(SandboxMode::parse("PROJECT"), SandboxMode::Project);
        assert_eq!(SandboxMode::parse(""), SandboxMode::None);
        // Unknown value → None (warning is logged, not an error)
        assert_eq!(SandboxMode::parse("strict"), SandboxMode::None);
    }

    #[test]
    fn display_roundtrip() {
        assert_eq!(SandboxMode::None.to_string(), "none");
        assert_eq!(SandboxMode::Project.to_string(), "project");
    }

    #[test]
    fn default_is_none() {
        assert_eq!(SandboxMode::default(), SandboxMode::None);
    }

    #[test]
    fn is_active() {
        assert!(!SandboxMode::None.is_active());
        assert!(SandboxMode::Project.is_active());
    }

    // ── Integration: kernel-level enforcement ──────────────────────────────
    //
    // These tests spawn real child processes through the sandbox and verify
    // that the kernel actually enforces the policy — not just that we built
    // the right Command.

    /// `SandboxMode::None` must never block anything.
    #[tokio::test]
    async fn none_mode_runs_echo() {
        let dir = tempfile::tempdir().unwrap();
        let status = build("echo ok", dir.path(), &SandboxMode::None)
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
        let status = build("touch sandbox_canary", dir.path(), &SandboxMode::Project)
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
            &SandboxMode::Project,
        )
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
        let status = build("cat /etc/hosts > /dev/null", dir.path(), &SandboxMode::Project)
            .status()
            .await
            .unwrap();
        assert!(status.success(), "reads outside project must be allowed");
    }
}
