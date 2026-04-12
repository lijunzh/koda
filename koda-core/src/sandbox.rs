//! Process sandboxing for the Bash tool.
//!
//! Sandboxing is **opt-in** (`--sandbox project`; default: `none`) and
//! applied per-session.  The goal is to prevent the model from accidentally —
//! or adversarially — reading credentials or writing outside the project.
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
//! - **`strict`** — everything from `project` mode, plus explicit read+write
//!   denies for sensitive credential directories (`~/.ssh`, `~/.aws`, …).
//!   Inspired by:
//!   - Claude Code's `denyRead[]` list (src/utils/sandbox/sandbox-adapter.ts)
//!   - Gemini CLI's `forbiddenPaths` (packages/core/src/services/sandboxManager.ts)
//!   - Codex's `FileSystemAccessMode::None` (codex-rs/protocol/src/permissions.rs)
//!
//! ## Usage
//!
//! ```rust,ignore
//! let cmd = sandbox::build("cargo test", project_root, &SandboxMode::Strict);
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
    /// Everything from `Project`, plus read+write denies for sensitive
    /// credential directories (`~/.ssh`, `~/.aws`, `~/.gnupg`, …).
    ///
    /// Inspired by Claude Code's `denyRead[]` and Gemini CLI's
    /// `forbiddenPaths` — both place explicit deny rules after broad allows so
    /// that the last-match-wins semantics of the underlying sandbox engine
    /// (seatbelt on macOS, bwrap `--tmpfs` shadow on Linux) take precedence.
    Strict,
}

impl SandboxMode {
    /// Parse from a CLI / env string (case-insensitive).
    ///
    /// Unknown values produce a warning and fall back to `None`.
    pub fn parse(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "project" => Self::Project,
            "strict" => Self::Strict,
            "none" | "" => Self::None,
            other => {
                tracing::warn!(
                    "Unknown --sandbox value {:?} — defaulting to none. \
                     Valid values: none, project, strict",
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
            Self::Strict => f.write_str("strict"),
        }
    }
}

// ── Sensitive credential paths (Strict mode) ─────────────────────────────────
//
// Directories and files blocked from reads *and* writes in Strict mode.
// We follow Claude Code's `denyRead[]` and Gemini CLI's `forbiddenPaths`
// pattern: define a fixed set of well-known credential paths, then place
// explicit deny rules *after* the broad allow so last-match-wins semantics
// take effect (seatbelt / bwrap tmpfs shadow).
//
// Rationale for each entry:
//   .ssh            — private keys, authorized_keys, known_hosts
//   .aws            — access key ID, secret key, session tokens
//   .gnupg          — GPG private keys and trust database
//   .kube           — kubeconfig with cluster tokens and client certs
//   .azure          — Azure CLI token cache (msal_token_cache.bin, etc.)
// ~/.config/gcloud  — gcloud CLI credentials and service account keys
// ~/.config/koda/db — SQLite DB containing plaintext API keys in KV store (#847)

/// Credential *directories* under `$HOME` blocked in `Strict` mode.
/// Matched with `(subpath …)` / `--tmpfs` to cover the whole tree.
const CREDENTIAL_SUBDIRS: &[&str] = &[".ssh", ".aws", ".gnupg", ".kube", ".azure"];

/// Credential directories under `$HOME/.config/` blocked in `Strict` mode.
const CREDENTIAL_CONFIG_SUBDIRS: &[&str] = &[
    "gcloud",  // gcloud CLI credentials and service-account key files
    "koda/db", // SQLite DB with plaintext API keys in kv_store table (#847)
];

/// Individual credential *files* under `$HOME` blocked in `Strict` mode.
/// Matched with `(literal …)` / `--ro-bind /dev/null` to block the exact path.
const CREDENTIAL_FILES: &[&str] = &[
    ".netrc",              // FTP/HTTP credentials (curl, wget, Netrc crate)
    ".git-credentials",    // git-credential-store plaintext token file
    ".npmrc",              // npm registry auth token
    ".pypirc",             // PyPI upload API token
    ".docker/config.json", // Docker Hub credentials (auths, credsStore)
];

// ── Public entry point ────────────────────────────────────────────────────────

/// Build a `tokio::process::Command` that runs `sh -c "{command}"` inside
/// the appropriate sandbox for `mode`.
///
/// When `mode` is [`SandboxMode::Project`] or [`SandboxMode::Strict`] but the
/// platform backend is unavailable (e.g. `bwrap` not installed on Linux), a
/// warning is logged and an **unsandboxed** `Command` is returned — the caller
/// does not need to handle the failure case.
pub fn build(command: &str, project_root: &Path, mode: &SandboxMode) -> Command {
    match mode {
        SandboxMode::None => plain_sh(command, project_root),
        SandboxMode::Project => build_project(command, project_root),
        SandboxMode::Strict => build_strict(command, project_root),
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

    #[allow(unreachable_code)]
    plain_sh(command, project_root)
}

/// Dispatch to the platform-specific "strict" sandbox builder.
fn build_strict(command: &str, project_root: &Path) -> Command {
    #[cfg(target_os = "macos")]
    return macos_strict(command, project_root);

    #[cfg(target_os = "linux")]
    return linux_strict(command, project_root);

    #[allow(unreachable_code)]
    plain_sh(command, project_root)
}

/// Canonicalize `{home}/{rel}` if the path exists; otherwise return raw path.
/// This ensures seatbelt subpath/literal rules match the kernel's view of the
/// path (e.g. `/var` → `/private/var` on macOS).
fn home_path(home: &str, rel: &str) -> String {
    let p = Path::new(home).join(rel);
    p.canonicalize().unwrap_or(p).to_string_lossy().into_owned()
}

// ── macOS: sandbox-exec -p <profile string> ───────────────────────────────────

/// Build the seatbelt profile for `project` mode.
///
/// Strategy: deny-by-default (allowlist), then open reads everywhere and
/// restrict writes to project + temp + cache dirs.  Network left unrestricted
/// so `curl` / `cargo fetch` / `npm install` work without modification.
///
/// Passing the profile via `-p` (inline) avoids a tempfile and its associated
/// race window — a lesson from Gemini CLI's earlier implementation which used
/// `-f <tempfile>` and had to clean up on every command.
#[cfg(target_os = "macos")]
fn macos_project_profile(root: &str, home: &str) -> String {
    format!(
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
    )
}

/// Generate seatbelt deny rules for credential paths (Strict mode).
///
/// The rules are placed *after* the broad `(allow file-read*)` rule so that
/// seatbelt's last-match-wins semantics make them take precedence — the same
/// technique used by Gemini CLI's `buildSeatbeltProfile` (forbiddenPaths
/// section in packages/core/src/sandbox/macos/seatbeltArgsBuilder.ts).
#[cfg(target_os = "macos")]
fn credential_deny_rules_macos(home: &str) -> String {
    let mut rules =
        String::from("; ── strict: deny reads+writes to credential dirs ──────────────\n");

    for rel in CREDENTIAL_SUBDIRS {
        let p = home_path(home, rel);
        rules.push_str(&format!(
            "(deny file-read* file-write* (subpath \"{p}\"))\n"
        ));
    }
    for rel in CREDENTIAL_CONFIG_SUBDIRS {
        let p = home_path(home, &format!(".config/{rel}"));
        rules.push_str(&format!(
            "(deny file-read* file-write* (subpath \"{p}\"))\n"
        ));
    }
    for rel in CREDENTIAL_FILES {
        let p = home_path(home, rel);
        rules.push_str(&format!(
            "(deny file-read* file-write* (literal \"{p}\"))\n"
        ));
    }
    rules
}

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

    let profile = macos_project_profile(&root, &home);

    let mut cmd = Command::new("sandbox-exec");
    cmd.arg("-p")
        .arg(profile)
        .arg("sh")
        .arg("-c")
        .arg(command)
        .current_dir(project_root);
    cmd
}

#[cfg(target_os = "macos")]
fn macos_strict(command: &str, project_root: &Path) -> Command {
    let canonical = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    let root = canonical.to_string_lossy();
    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users".into());

    // Start from the project profile then append credential deny rules.
    // Seatbelt evaluates rules in order; later rules win for the same path,
    // so the denies override the earlier broad `(allow file-read*)`.
    // Same last-match-wins approach as Gemini CLI's seatbeltArgsBuilder.ts.
    let mut profile = macos_project_profile(&root, &home);
    profile.push_str(&credential_deny_rules_macos(&home));

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
fn bwrap_available() -> bool {
    use std::sync::OnceLock;
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::process::Command::new("bwrap")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
    })
}

/// Build a bwrap `Command` with the base project-mode filesystem view.
///
/// Returns `(cmd, home)` with everything set up *except* the final
/// `-- sh -c command` terminator — callers add that (plus any extra mounts for
/// strict mode) before spawning.
#[cfg(target_os = "linux")]
fn linux_base_cmd(project_root: &Path) -> (Command, String) {
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
    (cmd, home)
}

#[cfg(target_os = "linux")]
fn linux_project(command: &str, project_root: &Path) -> Command {
    if !bwrap_available() {
        tracing::warn!(
            "sandbox=project requested but bwrap is not installed. \
             Running without sandbox. \
             Install with: apt install bubblewrap  /  dnf install bubblewrap"
        );
        return plain_sh(command, project_root);
    }

    let (mut cmd, _home) = linux_base_cmd(project_root);
    cmd.args(["--", "sh", "-c", command])
        .current_dir(project_root);
    cmd
}

/// Strict mode on Linux: project-mode base + tmpfs shadows over credential
/// dirs (hides them by mounting an empty tmpfs at each sensitive path).
///
/// Inspired by Codex's `--tmpfs` technique in codex-rs/linux-sandbox/src/bwrap.rs
/// and Claude Code's `denyRead[]` list in src/utils/sandbox/sandbox-adapter.ts.
///
/// For individual credential *files* we use `--ro-bind /dev/null <file>`,
/// which makes the file appear empty inside the container while leaving the
/// host untouched.  This is the same pattern Codex uses for sensitivity-scoped
/// file shadowing.
#[cfg(target_os = "linux")]
fn linux_strict(command: &str, project_root: &Path) -> Command {
    if !bwrap_available() {
        tracing::warn!(
            "sandbox=strict requested but bwrap is not installed. \
             Running without sandbox. \
             Install with: apt install bubblewrap  /  dnf install bubblewrap"
        );
        return plain_sh(command, project_root);
    }

    let (mut cmd, home) = linux_base_cmd(project_root);

    // Shadow credential directories with empty tmpfs mounts.
    for rel in CREDENTIAL_SUBDIRS {
        let p = format!("{home}/{rel}");
        if Path::new(&p).exists() {
            cmd.args(["--tmpfs", &p]);
        }
    }
    for rel in CREDENTIAL_CONFIG_SUBDIRS {
        let p = format!("{home}/.config/{rel}");
        if Path::new(&p).exists() {
            cmd.args(["--tmpfs", &p]);
        }
    }
    // Shadow individual credential files with /dev/null.
    for rel in CREDENTIAL_FILES {
        let p = format!("{home}/{rel}");
        if Path::new(&p).exists() {
            cmd.args(["--ro-bind", "/dev/null", &p]);
        }
    }

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
        assert_eq!(SandboxMode::parse("strict"), SandboxMode::Strict);
        assert_eq!(SandboxMode::parse("STRICT"), SandboxMode::Strict);
        assert_eq!(SandboxMode::parse(""), SandboxMode::None);
        // Unknown value → None (warning is logged, not an error)
        assert_eq!(SandboxMode::parse("banana"), SandboxMode::None);
    }

    #[test]
    fn display_roundtrip() {
        assert_eq!(SandboxMode::None.to_string(), "none");
        assert_eq!(SandboxMode::Project.to_string(), "project");
        assert_eq!(SandboxMode::Strict.to_string(), "strict");
    }

    #[test]
    fn default_is_none() {
        assert_eq!(SandboxMode::default(), SandboxMode::None);
    }

    #[test]
    fn is_active() {
        assert!(!SandboxMode::None.is_active());
        assert!(SandboxMode::Project.is_active());
        assert!(SandboxMode::Strict.is_active());
    }

    // ── Unit: strict profile contains deny rules ───────────────────────────
    //
    // We test the profile *string* rather than kernel enforcement to keep the
    // test hermetic — the kernel-enforcement tests below verify the enforcement
    // end-to-end for project mode (same underlying mechanism).

    #[cfg(target_os = "macos")]
    #[test]
    fn strict_profile_denies_ssh_dir() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/test".into());
        let rules = credential_deny_rules_macos(&home);
        let ssh = home_path(&home, ".ssh");
        assert!(
            rules.contains(&format!(
                "(deny file-read* file-write* (subpath \"{ssh}\"))"
            )),
            "strict profile must contain deny rule for ~/.ssh"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn strict_profile_denies_aws_dir() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/test".into());
        let rules = credential_deny_rules_macos(&home);
        let aws = home_path(&home, ".aws");
        assert!(
            rules.contains(&format!(
                "(deny file-read* file-write* (subpath \"{aws}\"))"
            )),
            "strict profile must contain deny rule for ~/.aws"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn strict_profile_denies_koda_db() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/test".into());
        let rules = credential_deny_rules_macos(&home);
        let koda_db = home_path(&home, ".config/koda/db");
        assert!(
            rules.contains(&format!(
                "(deny file-read* file-write* (subpath \"{koda_db}\"))"
            )),
            "strict profile must deny reads to ~/.config/koda/db (plaintext API keys in SQLite, #847)"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn strict_profile_denies_credential_files() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/test".into());
        let rules = credential_deny_rules_macos(&home);
        let netrc = home_path(&home, ".netrc");
        assert!(
            rules.contains(&format!(
                "(deny file-read* file-write* (literal \"{netrc}\"))"
            )),
            "strict profile must contain deny rule for ~/.netrc"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn strict_deny_rules_come_after_broad_allow() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/test".into());
        let project = Path::new("/tmp/test-project");
        let root = project.to_string_lossy().into_owned();
        let profile = macos_project_profile(&root, &home);
        let deny_rules = credential_deny_rules_macos(&home);
        // Simulate what macos_strict does: project profile then deny rules.
        let full = format!("{profile}{deny_rules}");
        let allow_pos = full.find("(allow file-read*)").unwrap();
        let deny_pos = full.find("(deny file-read* file-write*").unwrap();
        assert!(
            deny_pos > allow_pos,
            "deny rules must appear after the broad allow (last-match-wins)"
        );
    }

    // ── Integration: kernel-level enforcement ──────────────────────────────
    //
    // Spawn real child processes through the sandbox and verify that the
    // kernel actually enforces the policy.

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
        let status = build(
            "cat /etc/hosts > /dev/null",
            dir.path(),
            &SandboxMode::Project,
        )
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
        let status = build("touch strict_canary", dir.path(), &SandboxMode::Strict)
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
            &SandboxMode::Strict,
        )
        .status()
        .await
        .unwrap();

        assert!(
            !status.success(),
            "strict: write outside project must be blocked"
        );
        assert!(!target.exists());
    }

    /// Strict mode: reads to non-sensitive paths must still work.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_strict_allows_reads_outside_sensitive() {
        let dir = tempfile::tempdir().unwrap();
        let status = build(
            "cat /etc/hosts > /dev/null",
            dir.path(),
            &SandboxMode::Strict,
        )
        .status()
        .await
        .unwrap();
        assert!(
            status.success(),
            "strict: reads to /etc/hosts must still be allowed"
        );
    }

    /// Strict mode: reading `~/.config/koda/db/` must be blocked (#847).
    ///
    /// The koda SQLite DB contains plaintext API keys in the `kv_store` table.
    /// A sandboxed bash command must not be able to `sqlite3` it.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_strict_blocks_koda_db_read() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/test".into());
        let db_dir = format!("{home}/.config/koda/db");
        if !Path::new(&db_dir).exists() {
            // DB dir doesn't exist on this machine — skip but don't fail.
            eprintln!("skip: {db_dir} does not exist");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let status = build(&format!("ls {db_dir}"), dir.path(), &SandboxMode::Strict)
            .status()
            .await
            .unwrap();
        assert!(
            !status.success(),
            "strict: reading ~/.config/koda/db/ must be blocked"
        );
    }
}
