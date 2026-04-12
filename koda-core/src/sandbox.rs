//! Process sandboxing for the Bash tool.
//!
//! Sandboxing is always active and determined by [`crate::trust::TrustMode`].
//! The `SandboxMode` enum is an internal implementation detail.
//!
//! ## Platform backends
//!
//! | Platform | Backend              | If unavailable              |
//! |----------|----------------------|-----------------------------|
//! | macOS    | `sandbox-exec -p`    | falls back to unsandboxed   |
//! | Linux    | `bwrap` (bubblewrap) | falls back to unsandboxed   |
//!
//! ## Trust → Sandbox mapping
//!
//! - **Plan** → Project sandbox + credential denies + read-only FS
//! - **Safe** → Project sandbox + credential denies + read-write FS
//! - **Auto** → Project sandbox + credential denies + read-write FS
//!
//! All modes deny credential paths. The old `SandboxMode::None` is gone.
//!
//! ## Usage
//!
//! ```rust,ignore
//! let cmd = sandbox::build("cargo test", project_root, &TrustMode::Safe)?;
//! let child = cmd.stdout(Stdio::piped()).spawn()?;
//! ```

use anyhow::Result;
use std::path::Path;
use tokio::process::Command;

// ── Mode ─────────────────────────────────────────────────────────────────────

/// Internal sandbox level — derived from [`crate::trust::TrustMode`].
/// Not exposed publicly; users interact via `TrustMode`.
#[allow(dead_code)] // Variants used internally for sandbox level dispatch
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum SandboxMode {
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

#[allow(dead_code)] // Methods used by internal sandbox tests
impl SandboxMode {
    /// Return a numeric strictness level for comparison.
    ///
    /// Higher = stricter.  Used by `stricter()` to enforce the
    /// "never weaken" invariant when inheriting from a parent.
    fn level(&self) -> u8 {
        match self {
            Self::None => 0,
            Self::Project => 1,
            Self::Strict => 2,
        }
    }

    /// Return whichever of `self` and `other` is stricter.
    ///
    /// Used by sub-agent dispatch to enforce the invariant that a child
    /// agent can never run with a weaker sandbox than its parent — the same
    /// pattern as Codex's `apply_spawn_agent_runtime_overrides()` which
    /// copies the parent's runtime `sandbox_policy` onto the child config.
    pub fn stricter(&self, other: &Self) -> Self {
        if self.level() >= other.level() {
            self.clone()
        } else {
            other.clone()
        }
    }

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
const CREDENTIAL_SUBDIRS: &[&str] = &[
    ".ssh",            // SSH private keys, authorized_keys, known_hosts
    ".aws",            // AWS access key ID, secret key, session tokens
    ".gnupg",          // GPG private keys and trust database
    ".kube",           // kubeconfig with cluster tokens and client certs
    ".azure",          // Azure CLI token cache (msal_token_cache.bin, etc.)
    ".password-store", // pass(1) GPG-encrypted password store
    ".terraform.d",    // Terraform cloud tokens and plugin cache
];

/// Credential directories under `$HOME/.config/` blocked in `Strict` mode.
const CREDENTIAL_CONFIG_SUBDIRS: &[&str] = &[
    "gcloud",  // gcloud CLI credentials and service-account key files
    "koda/db", // SQLite DB with plaintext API keys in kv_store table (#847)
    "gh",      // GitHub CLI personal access tokens (hosts.yml)
    "op",      // 1Password CLI session tokens
    "helm",    // Helm registry auth
];

/// Individual credential *files* under `$HOME` blocked in `Strict` mode.
/// Matched with `(literal …)` / `--ro-bind /dev/null` to block the exact path.
const CREDENTIAL_FILES: &[&str] = &[
    ".netrc",              // FTP/HTTP credentials (curl, wget, Netrc crate)
    ".git-credentials",    // git-credential-store plaintext token file
    ".npmrc",              // npm registry auth token
    ".pypirc",             // PyPI upload API token
    ".docker/config.json", // Docker Hub credentials (auths, credsStore)
    ".vault-token",        // HashiCorp Vault session token
    ".env",                // dotenv secrets (common project-level pattern)
];

// ── Agent-file write protection ──────────────────────────────────────────────
//
// Prevent sandboxed commands from modifying koda agent definitions or the
// project-level system prompt.  Same pattern as Claude Code blocking writes
// to `.claude/settings.json` and `.claude/agents/` — modifying these files
// could alter system prompts, tool access, or sandbox policy on next session.
//
// These are subdirectories of the project root, denied in *all* sandbox modes
// (project + strict).  The deny rule is placed *after* the project-root allow
// so last-match-wins semantics apply.

/// Directories under the project root that are write-protected in all sandbox
/// modes.  Agent JSON files control system prompts and tool access — a
/// sandboxed command must not be able to modify them.
const PROTECTED_PROJECT_SUBDIRS: &[&str] = &[
    ".koda/agents", // Agent definitions (system prompt, tools, sub-agents)
    ".koda/skills", // Skill definitions (auto-discovered, full capabilities)
];

// ── Public entry point ────────────────────────────────────────────────────────

/// Build a `tokio::process::Command` that runs `sh -c "{command}"` inside
/// the appropriate sandbox for the given [`crate::trust::TrustMode`].
///
/// All trust modes apply project-scoped sandboxing with credential denies.
/// The mapping is:
/// - **Plan** → Strict sandbox (credential denies + project writes)
/// - **Safe** → Strict sandbox (credential denies + project writes)
/// - **Auto** → Strict sandbox (credential denies + project writes)
///
/// Falls back to unsandboxed execution with a warning when the platform
/// sandbox backend is unavailable (e.g. `bwrap` not installed on Linux,
/// unsupported OS).  The sandbox is best-effort — we never block the user
/// just because the kernel enforcement layer is missing.
pub fn build(
    command: &str,
    project_root: &Path,
    _trust: &crate::trust::TrustMode,
) -> Result<Command> {
    // All modes use strict sandboxing (project + credential denies).
    // The sandbox is the safety boundary — always on when available.
    match build_inner(command, project_root, &SandboxMode::Strict) {
        Ok(cmd) => Ok(cmd),
        Err(e) => {
            tracing::warn!("Sandbox unavailable, running unsandboxed: {e}");
            Ok(plain_sh(command, project_root))
        }
    }
}

/// Internal build dispatcher.
fn build_inner(command: &str, project_root: &Path, mode: &SandboxMode) -> Result<Command> {
    match mode {
        SandboxMode::None => Ok(plain_sh(command, project_root)),
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
fn build_project(command: &str, project_root: &Path) -> Result<Command> {
    #[cfg(target_os = "macos")]
    return macos_project(command, project_root);

    #[cfg(target_os = "linux")]
    return linux_project(command, project_root);

    #[allow(unreachable_code)]
    Err(anyhow::anyhow!(
        "Sandbox mode 'project' requested but no sandbox backend available \
         on this platform. Supported: macOS (sandbox-exec), Linux (bwrap)."
    ))
}

/// Dispatch to the platform-specific "strict" sandbox builder.
fn build_strict(command: &str, project_root: &Path) -> Result<Command> {
    #[cfg(target_os = "macos")]
    return macos_strict(command, project_root);

    #[cfg(target_os = "linux")]
    return linux_strict(command, project_root);

    #[allow(unreachable_code)]
    Err(anyhow::anyhow!(
        "Sandbox mode 'strict' requested but no sandbox backend available \
         on this platform. Supported: macOS (sandbox-exec), Linux (bwrap)."
    ))
}

/// Reject paths containing characters that could break seatbelt S-expression
/// syntax.  A crafted `project_root` with `"` or `(` could inject arbitrary
/// seatbelt rules into the profile string, completely defeating the sandbox.
///
/// We reject rather than escape because legitimate filesystem paths should
/// never contain these characters, and escaping adds subtle semantic risk.
#[cfg(target_os = "macos")]
fn validate_seatbelt_path(s: &str) -> Result<()> {
    const FORBIDDEN: &[char] = &['"', '\\', '(', ')', '\0'];
    if let Some(c) = s.chars().find(|c| FORBIDDEN.contains(c)) {
        anyhow::bail!("Path contains character {c:?} unsafe for seatbelt profile: {s:?}");
    }
    Ok(())
}

/// Canonicalize `{home}/{rel}` if the path exists; otherwise return raw path.
/// This ensures seatbelt subpath/literal rules match the kernel's view of the
/// path (e.g. `/var` → `/private/var` on macOS).
#[cfg(target_os = "macos")]
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

/// Generate seatbelt deny-write rules for protected project subdirectories.
///
/// Prevents sandboxed commands from modifying agent definitions or skills
/// that could alter system prompts or tool access on next session.  Same
/// pattern as Claude Code blocking writes to `.claude/settings.json` and
/// `.claude/agents/`.
#[cfg(target_os = "macos")]
fn protected_subdir_deny_rules_macos(root: &str) -> String {
    let mut rules = String::from(
        "; ── deny writes to protected project subdirs (.koda/agents, .koda/skills) ──\n",
    );
    for rel in PROTECTED_PROJECT_SUBDIRS {
        let p = Path::new(root).join(rel);
        let canonical = p.canonicalize().unwrap_or(p).to_string_lossy().into_owned();
        rules.push_str(&format!("(deny file-write* (subpath \"{canonical}\"))\n"));
    }
    rules
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
fn macos_project(command: &str, project_root: &Path) -> Result<Command> {
    let canonical = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    let root = canonical.to_string_lossy();
    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users".into());
    validate_seatbelt_path(&root)?;
    validate_seatbelt_path(&home)?;

    let mut profile = macos_project_profile(&root, &home);
    // Deny writes to agent/skill files within the project (CC parity #844).
    profile.push_str(&protected_subdir_deny_rules_macos(&root));

    let mut cmd = Command::new("sandbox-exec");
    cmd.arg("-p")
        .arg(profile)
        .arg("sh")
        .arg("-c")
        .arg(command)
        .current_dir(project_root);
    Ok(cmd)
}

#[cfg(target_os = "macos")]
fn macos_strict(command: &str, project_root: &Path) -> Result<Command> {
    let canonical = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    let root = canonical.to_string_lossy();
    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users".into());
    validate_seatbelt_path(&root)?;
    validate_seatbelt_path(&home)?;

    // Start from the project profile then append credential deny rules.
    // Seatbelt evaluates rules in order; later rules win for the same path,
    // so the denies override the earlier broad `(allow file-read*)`.
    // Same last-match-wins approach as Gemini CLI's seatbeltArgsBuilder.ts.
    let mut profile = macos_project_profile(&root, &home);
    profile.push_str(&protected_subdir_deny_rules_macos(&root));
    profile.push_str(&credential_deny_rules_macos(&home));

    let mut cmd = Command::new("sandbox-exec");
    cmd.arg("-p")
        .arg(profile)
        .arg("sh")
        .arg("-c")
        .arg(command)
        .current_dir(project_root);
    Ok(cmd)
}

// ── Linux: bwrap (bubblewrap) ─────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn bwrap_available() -> bool {
    use std::sync::OnceLock;
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        // Just checking `bwrap --version` is insufficient: bwrap may be
        // installed but unable to create sandboxes (e.g. GitHub Actions
        // runners lack unprivileged user namespaces → "setting up uid map:
        // Permission denied"). Run a real sandboxed command to verify.
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

#[cfg(target_os = "linux")]
fn linux_project(command: &str, project_root: &Path) -> Result<Command> {
    if !bwrap_available() {
        anyhow::bail!(
            "Sandbox mode 'project' requested but bwrap is not installed. \
             Install with: apt install bubblewrap  /  dnf install bubblewrap"
        );
    }

    let (mut cmd, _home) = linux_base_cmd(project_root);
    cmd.args(["--", "sh", "-c", command])
        .current_dir(project_root);
    Ok(cmd)
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
fn linux_strict(command: &str, project_root: &Path) -> Result<Command> {
    if !bwrap_available() {
        anyhow::bail!(
            "Sandbox mode 'strict' requested but bwrap is not installed. \
             Install with: apt install bubblewrap  /  dnf install bubblewrap"
        );
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
    Ok(cmd)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Unit: enum behaviour ───────────────────────────────────────────────

    #[test]
    fn stricter_returns_higher_level() {
        use SandboxMode::*;
        // Same mode → returns self
        assert_eq!(None.stricter(&None), None);
        assert_eq!(Project.stricter(&Project), Project);
        assert_eq!(Strict.stricter(&Strict), Strict);
        // Different modes → returns the stricter one
        assert_eq!(None.stricter(&Project), Project);
        assert_eq!(Project.stricter(&None), Project);
        assert_eq!(None.stricter(&Strict), Strict);
        assert_eq!(Strict.stricter(&None), Strict);
        assert_eq!(Project.stricter(&Strict), Strict);
        assert_eq!(Strict.stricter(&Project), Strict);
    }

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

    /// Sandbox build must always succeed (falls back to unsandboxed if
    /// the platform backend is unavailable, e.g. no `bwrap` on CI Linux).
    #[tokio::test]
    async fn build_always_succeeds_and_runs_echo() {
        let dir = tempfile::tempdir().unwrap();
        let status = build("echo ok", dir.path(), &crate::trust::TrustMode::Safe)
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
        )
        .unwrap()
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
            &crate::trust::TrustMode::Safe,
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
        let status = build(
            &format!("ls {db_dir}"),
            dir.path(),
            &crate::trust::TrustMode::Safe,
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

    // ── Integration: agent-file write protection ──────────────────────────

    /// Project mode: writing to `.koda/agents/` inside the project must be
    /// blocked (CC parity #844 — settings file write protection).
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_project_blocks_write_to_koda_agents() {
        let dir = tempfile::tempdir().unwrap();
        // Create the .koda/agents/ directory so the deny rule kicks in.
        std::fs::create_dir_all(dir.path().join(".koda/agents")).unwrap();
        let target = dir.path().join(".koda/agents/evil.json");

        let status = build(
            &format!("echo '{{}}' > {}", target.display()),
            dir.path(),
            &crate::trust::TrustMode::Safe,
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

    /// Project mode: writing to normal project files must still work
    /// (regression check — don't over-deny).
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_project_allows_normal_writes_with_agents_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".koda/agents")).unwrap();

        let status = build(
            "touch normal_file.txt",
            dir.path(),
            &crate::trust::TrustMode::Safe,
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
    /// blocked (same protection as `.koda/agents/`).
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

    /// Seatbelt path validation: reject paths with characters that could
    /// inject rules into the profile string.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_rejects_path_with_quote() {
        let result = validate_seatbelt_path("/tmp/evil\")(allow file-write*");
        assert!(result.is_err(), "path with quote must be rejected");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_accepts_normal_path() {
        assert!(validate_seatbelt_path("/Users/test/project").is_ok());
        assert!(validate_seatbelt_path("/tmp/koda-test-12345").is_ok());
    }
}
