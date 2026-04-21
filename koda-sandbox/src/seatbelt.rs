//! macOS Seatbelt (`sandbox-exec`) backend.
//!
//! Generates a Scheme-syntax sandbox profile and spawns commands via
//! `sandbox-exec -p <profile> sh -c <cmd>`.
//!
//! ## Phase 0
//!
//! Lifted verbatim from `koda-core/src/sandbox.rs::macos_*`. The
//! [`crate::policy::SandboxPolicy`] argument is currently *unused* — the
//! profile is built from the [`crate::defaults`] baseline, which preserves
//! pre-#934 byte-for-byte behavior.
//!
//! ## Phase 1
//!
//! The two-layer rule support (deny + allow-within / allow + deny-within)
//! lands here, plus `<sandbox_violations>` annotations parsed from the
//! macOS unified log.
//!
//! ## Profile-passing strategy
//!
//! Passing the profile via `-p` (inline) avoids a tempfile and its
//! associated race window — a lesson from Gemini CLI's earlier
//! implementation which used `-f <tempfile>` and had to clean up on every
//! command.

#![cfg(target_os = "macos")]

use crate::defaults::{
    CREDENTIAL_CONFIG_FULL_DENY, CREDENTIAL_CONFIG_SUBDIRS, CREDENTIAL_FILES, CREDENTIAL_SUBDIRS,
    PROTECTED_PROJECT_SUBDIRS,
};
use crate::policy::SandboxPolicy;
use anyhow::Result;
use std::path::Path;
use tokio::process::Command;

/// Build a `sandbox-exec` Command for the given command + project root.
///
/// The profile is composed in three layers, last-match-wins:
///   1. Baseline (`build_profile_string`)         — broad reads + project writes
///   2. Hardcoded denies (subdirs, credential dirs) — protect known sensitive
///   3. Policy overlay (`policy_overlay_rules`)   — caller-supplied rules
///
/// An empty `policy` (e.g. [`SandboxPolicy::strict_default`]) produces a
/// profile byte-identical to pre-#934 behavior.
pub fn build_command(
    command: &str,
    project_root: &Path,
    policy: &SandboxPolicy,
) -> Result<Command> {
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
    let mut profile = build_profile_string(&root, &home);
    profile.push_str(&protected_subdir_deny_rules(&root));
    profile.push_str(&credential_deny_rules(&home));
    profile.push_str(&policy_overlay_rules(policy)?);

    let mut cmd = Command::new("sandbox-exec");
    cmd.arg("-p")
        .arg(profile)
        .arg("sh")
        .arg("-c")
        .arg(command)
        .current_dir(project_root);
    Ok(cmd)
}

/// Probe whether the macOS Seatbelt backend is functional. Cached.
pub fn is_available() -> bool {
    use std::sync::OnceLock;
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        build_command(
            "true",
            Path::new("/tmp"),
            &crate::policy::SandboxPolicy::strict_default(),
        )
        .is_ok()
    })
}

/// Reject paths containing characters that could break seatbelt S-expression
/// syntax.  A crafted `project_root` with `"` or `(` could inject arbitrary
/// seatbelt rules into the profile string, completely defeating the sandbox.
///
/// We reject rather than escape because legitimate filesystem paths should
/// never contain these characters, and escaping adds subtle semantic risk.
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
pub(crate) fn home_path(home: &str, rel: &str) -> String {
    let p = Path::new(home).join(rel);
    p.canonicalize().unwrap_or(p).to_string_lossy().into_owned()
}

/// Build the seatbelt profile for the project-mode baseline.
///
/// Strategy: deny-by-default (allowlist), then open reads everywhere and
/// restrict writes to project + temp + cache dirs.  Network left
/// unrestricted so `curl` / `cargo fetch` / `npm install` work without
/// modification (Phase 3 adds the proxy-based egress filter).
pub(crate) fn build_profile_string(root: &str, home: &str) -> String {
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
pub(crate) fn protected_subdir_deny_rules(root: &str) -> String {
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

/// Generate seatbelt deny rules for credential paths.
///
/// Two tiers:
/// - **Write-only deny** for most paths — lets CLI tools read their own
///   credentials while preventing sandboxed commands from modifying them.
/// - **Full read+write deny** for `koda/db` only — koda's own API keys
///   should never be accessible from inside the sandbox (#847).
///
/// Rules are placed *after* the broad `(allow file-read*)` so that
/// seatbelt's last-match-wins semantics make them take precedence.
pub(crate) fn credential_deny_rules(home: &str) -> String {
    let mut rules = String::from("; ── strict: write-protect credential dirs (reads allowed) ──\n");

    // Tier 1 — write-only deny (CLI tools can still read).
    for rel in CREDENTIAL_SUBDIRS {
        let p = home_path(home, rel);
        rules.push_str(&format!("(deny file-write* (subpath \"{p}\"))\n"));
    }
    for rel in CREDENTIAL_CONFIG_SUBDIRS {
        let p = home_path(home, &format!(".config/{rel}"));
        rules.push_str(&format!("(deny file-write* (subpath \"{p}\"))\n"));
    }
    for rel in CREDENTIAL_FILES {
        let p = home_path(home, rel);
        rules.push_str(&format!("(deny file-write* (literal \"{p}\"))\n"));
    }

    // Tier 2 — full read+write deny (koda's own secrets).
    rules.push_str("; ── strict: full deny for koda-internal secrets ─────────────\n");
    for rel in CREDENTIAL_CONFIG_FULL_DENY {
        let p = home_path(home, &format!(".config/{rel}"));
        rules.push_str(&format!(
            "(deny file-read* file-write* (subpath \"{p}\"))\n"
        ));
    }

    rules
}

/// Phase 1b of #934: render `policy.fs.*` into seatbelt rules.
///
/// Layered to match the issue's two-rule semantics. Order matters in
/// seatbelt (last match wins), so within each layer we emit denies
/// *before* their carve-out allows:
///
///   1. `deny_read`                 — carve denies into broad allow.
///   2. `allow_read_within_deny`    — carve allows back into the denies.
///   3. `allow_write`               — widen the writable set.
///   4. `deny_write_within_allow`   — protect spots inside the writes.
///
/// Each path is validated through [`validate_seatbelt_path`] to prevent
/// S-expression injection — `deny_read: vec!["foo\")(allow file-write*"]`
/// would otherwise let a caller punch through their own sandbox.
///
/// Returns the empty string for an all-default policy, which keeps the
/// pre-#934 byte-identical behavior promised by [`build_command`].
pub(crate) fn policy_overlay_rules(policy: &SandboxPolicy) -> Result<String> {
    let fs = &policy.fs;
    if fs.deny_read.is_empty()
        && fs.allow_read_within_deny.is_empty()
        && fs.allow_write.is_empty()
        && fs.deny_write_within_allow.is_empty()
    {
        return Ok(String::new());
    }

    let mut rules =
        String::from("; \u{2500}\u{2500} policy overlay (Phase 1b of #934) \u{2500}\u{2500}\n");

    for p in &fs.deny_read {
        let s = p.as_path().to_string_lossy();
        validate_seatbelt_path(&s)?;
        rules.push_str(&format!("(deny file-read* (subpath \"{s}\"))\n"));
    }
    for p in &fs.allow_read_within_deny {
        let s = p.as_path().to_string_lossy();
        validate_seatbelt_path(&s)?;
        rules.push_str(&format!("(allow file-read* (subpath \"{s}\"))\n"));
    }
    for p in &fs.allow_write {
        let s = p.as_path().to_string_lossy();
        validate_seatbelt_path(&s)?;
        rules.push_str(&format!("(allow file-write* (subpath \"{s}\"))\n"));
    }
    for p in &fs.deny_write_within_allow {
        let s = p.as_path().to_string_lossy();
        validate_seatbelt_path(&s)?;
        rules.push_str(&format!("(deny file-write* (subpath \"{s}\"))\n"));
    }
    Ok(rules)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Profile structure: deny rules present ──────────────────────────────

    #[test]
    fn strict_profile_write_protects_ssh_dir() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/test".into());
        let rules = credential_deny_rules(&home);
        let ssh = home_path(&home, ".ssh");
        assert!(
            rules.contains(&format!("(deny file-write* (subpath \"{ssh}\"))")),
            "strict profile must write-protect ~/.ssh"
        );
        // Reads should NOT be denied — CLI tools need credential access (#855).
        assert!(
            !rules.contains(&format!(
                "(deny file-read* file-write* (subpath \"{ssh}\"))"
            )),
            "strict profile must NOT read-deny ~/.ssh (breaks ssh/git)"
        );
    }

    #[test]
    fn strict_profile_write_protects_aws_dir() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/test".into());
        let rules = credential_deny_rules(&home);
        let aws = home_path(&home, ".aws");
        assert!(
            rules.contains(&format!("(deny file-write* (subpath \"{aws}\"))")),
            "strict profile must write-protect ~/.aws"
        );
        assert!(
            !rules.contains(&format!(
                "(deny file-read* file-write* (subpath \"{aws}\"))"
            )),
            "strict profile must NOT read-deny ~/.aws (breaks aws CLI)"
        );
    }

    #[test]
    fn strict_profile_write_protects_gh_dir() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/test".into());
        let rules = credential_deny_rules(&home);
        let gh = home_path(&home, ".config/gh");
        assert!(
            rules.contains(&format!("(deny file-write* (subpath \"{gh}\"))")),
            "strict profile must write-protect ~/.config/gh"
        );
        assert!(
            !rules.contains(&format!("(deny file-read* file-write* (subpath \"{gh}\"))")),
            "strict profile must NOT read-deny ~/.config/gh (breaks gh CLI, #855)"
        );
    }

    #[test]
    fn strict_profile_write_protects_claude_dir() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/test".into());
        let rules = credential_deny_rules(&home);
        let claude = home_path(&home, ".claude");
        assert!(
            rules.contains(&format!("(deny file-write* (subpath \"{claude}\"))")),
            "strict profile must write-protect ~/.claude"
        );
    }

    #[test]
    fn strict_profile_write_protects_android_dir() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/test".into());
        let rules = credential_deny_rules(&home);
        let android = home_path(&home, ".android");
        assert!(
            rules.contains(&format!("(deny file-write* (subpath \"{android}\"))")),
            "strict profile must write-protect ~/.android"
        );
    }

    #[test]
    fn strict_profile_write_protects_netlify_dir() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/test".into());
        let rules = credential_deny_rules(&home);
        let netlify = home_path(&home, ".config/netlify");
        assert!(
            rules.contains(&format!("(deny file-write* (subpath \"{netlify}\"))")),
            "strict profile must write-protect ~/.config/netlify"
        );
    }

    #[test]
    fn strict_profile_write_protects_vercel_dir() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/test".into());
        let rules = credential_deny_rules(&home);
        let vercel = home_path(&home, ".config/vercel");
        assert!(
            rules.contains(&format!("(deny file-write* (subpath \"{vercel}\"))")),
            "strict profile must write-protect ~/.config/vercel"
        );
    }

    #[test]
    fn strict_profile_fully_denies_koda_db() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/test".into());
        let rules = credential_deny_rules(&home);
        let koda_db = home_path(&home, ".config/koda/db");
        assert!(
            rules.contains(&format!(
                "(deny file-read* file-write* (subpath \"{koda_db}\"))"
            )),
            "strict profile must fully deny ~/.config/koda/db (plaintext API keys, #847)"
        );
    }

    #[test]
    fn strict_profile_write_protects_credential_files() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/test".into());
        let rules = credential_deny_rules(&home);
        let netrc = home_path(&home, ".netrc");
        assert!(
            rules.contains(&format!("(deny file-write* (literal \"{netrc}\"))")),
            "strict profile must write-protect ~/.netrc"
        );
        assert!(
            !rules.contains(&format!(
                "(deny file-read* file-write* (literal \"{netrc}\"))"
            )),
            "strict profile must NOT read-deny ~/.netrc (breaks curl/wget)"
        );
    }

    #[test]
    fn strict_deny_rules_come_after_broad_allow() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/test".into());
        let project = Path::new("/tmp/test-project");
        let root = project.to_string_lossy().into_owned();
        let profile = build_profile_string(&root, &home);
        let deny_rules = credential_deny_rules(&home);
        // Simulate what build_command does: project profile then deny rules.
        let full = format!("{profile}{deny_rules}");
        let allow_pos = full.find("(allow file-read*)").unwrap();
        // Full deny (koda/db) must come after broad allow.
        let deny_pos = full.find("(deny file-read* file-write*").unwrap();
        assert!(
            deny_pos > allow_pos,
            "deny rules must appear after the broad allow (last-match-wins)"
        );
        // Write-only deny must also come after broad allow.
        let write_deny_pos = full.find("(deny file-write*").unwrap();
        assert!(
            write_deny_pos > allow_pos,
            "write-deny rules must appear after the broad allow"
        );
    }

    // ── Path validation ────────────────────────────────────────────────────

    #[test]
    fn validate_seatbelt_path_rejects_quote() {
        assert!(validate_seatbelt_path("/tmp/evil\"').rs").is_err());
    }

    #[test]
    fn validate_seatbelt_path_rejects_paren() {
        assert!(validate_seatbelt_path("/tmp/(evil)").is_err());
    }

    #[test]
    fn validate_seatbelt_path_accepts_normal() {
        assert!(validate_seatbelt_path("/Users/me/Projects/koda").is_ok());
    }

    // ── Phase 1b of #934: policy overlay rendering ───────────────────────

    #[test]
    fn policy_overlay_rules_empty_for_default_policy() {
        // Phase 0/1a invariant: empty policy must produce empty overlay so
        // the existing baseline behavior stays byte-identical.
        let overlay = policy_overlay_rules(&SandboxPolicy::strict_default()).unwrap();
        assert_eq!(overlay, "", "default policy must add zero rules");
    }

    #[test]
    fn policy_overlay_rules_emit_deny_read_first_then_allow_within() {
        // Two-layer semantics: deny rules must precede their carve-out
        // allows so seatbelt's last-match-wins makes the allows win.
        let mut policy = SandboxPolicy::strict_default();
        policy.fs.deny_read = vec!["/secrets".into()];
        policy.fs.allow_read_within_deny = vec!["/secrets/public".into()];
        let overlay = policy_overlay_rules(&policy).unwrap();

        let deny_pos = overlay
            .find(r#"(deny file-read* (subpath "/secrets"))"#)
            .expect("deny rule expected");
        let allow_pos = overlay
            .find(r#"(allow file-read* (subpath "/secrets/public"))"#)
            .expect("allow rule expected");
        assert!(
            deny_pos < allow_pos,
            "deny must come before allow-within: {overlay}"
        );
    }

    #[test]
    fn policy_overlay_rules_emit_allow_write_then_deny_within() {
        let mut policy = SandboxPolicy::strict_default();
        policy.fs.allow_write = vec!["/work".into()];
        policy.fs.deny_write_within_allow = vec!["/work/.git/config".into()];
        let overlay = policy_overlay_rules(&policy).unwrap();

        let allow_pos = overlay
            .find(r#"(allow file-write* (subpath "/work"))"#)
            .expect("allow rule expected");
        let deny_pos = overlay
            .find(r#"(deny file-write* (subpath "/work/.git/config"))"#)
            .expect("deny rule expected");
        assert!(
            allow_pos < deny_pos,
            "allow must come before deny-within: {overlay}"
        );
    }

    #[test]
    fn policy_overlay_rules_validates_paths_to_prevent_injection() {
        // Hostile policy with S-expression-injection attempt must error
        // before producing a profile string. Otherwise a caller could
        // punch through their own sandbox via crafted paths.
        let mut policy = SandboxPolicy::strict_default();
        policy.fs.deny_read = vec![r#"/foo")(allow file-write*"#.into()];
        let result = policy_overlay_rules(&policy);
        assert!(result.is_err(), "injection-y path must be rejected");
    }

    #[test]
    fn policy_overlay_rules_is_appended_after_baseline_in_build_command() {
        // Behavioral check: the overlay rules must come *after* the
        // hardcoded credential denies so the user's policy can override
        // (e.g. user explicitly allow_read_within_deny on ~/.ssh would
        // override the credential read-deny if we ever had one).
        let mut policy = SandboxPolicy::strict_default();
        policy.fs.allow_read_within_deny = vec!["/etc/koda-marker".into()];

        let cmd = build_command("true", Path::new("/tmp"), &policy).unwrap();
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let profile = &args[1];
        let overlay_pos = profile
            .find("policy overlay (Phase 1b of #934)")
            .expect("overlay header missing");
        let credential_pos = profile
            .find("strict: write-protect credential dirs")
            .expect("credential header missing");
        assert!(
            credential_pos < overlay_pos,
            "policy overlay must come after credential rules"
        );
    }
}
