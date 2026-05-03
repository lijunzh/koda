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
    build_command_inner(command, project_root, policy, None)
}

/// Build a sandboxed `sandbox-exec` command with proxied egress.
///
/// Same as [`build_command`] but uses [`build_proxied_profile_string`]
/// instead of the open-network baseline: every TCP outbound is denied
/// except connections to `127.0.0.1:proxy_port` (the egress proxy).
/// Pair with [`crate::worker_client::WorkerClient::spawn_with_policy_and_proxy`] for
/// matching env-var injection.
///
/// `allow_local_binding` corresponds to `policy.net.allow_local_binding`
/// (when that field exists — today it's a hardcoded `false` until the
/// `NetPolicy` schema gains the field). Pass `false` for the strict default.
///
/// `weaker_macos_isolation` (Phase 3e of #934) corresponds to
/// `policy.net.weaker_macos_isolation`. When `true`, the proxied profile
/// permits mach-lookups to Apple's `trustd` and `trustd.agent` daemons
/// so Go-binary TLS handshakes (which validate certs out-of-process via
/// trustd, bypassing the rustls/openssl in-process path) succeed. Off
/// by default — leaks a small amount of metadata to Apple's daemons
/// but doesn't widen the network egress surface.
pub fn build_command_with_proxy(
    command: &str,
    project_root: &Path,
    policy: &SandboxPolicy,
    proxy_port: u16,
    allow_local_binding: bool,
    weaker_macos_isolation: bool,
) -> Result<Command> {
    build_command_inner(
        command,
        project_root,
        policy,
        Some((proxy_port, allow_local_binding, weaker_macos_isolation)),
    )
}

fn build_command_inner(
    command: &str,
    project_root: &Path,
    policy: &SandboxPolicy,
    proxy: Option<(u16, bool, bool)>,
) -> Result<Command> {
    let canonical = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    let root = canonical.to_string_lossy();
    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users".into());
    validate_seatbelt_path(&root)?;
    validate_seatbelt_path(&home)?;

    // Phase 4a of #934: cache the compiled SBPL by
    // (canonical_root, home, proxy, policy). The build closure runs
    // at most once per distinct key; on the warm path this is a
    // single HashMap::get + clone (sub-microsecond) instead of the
    // ~3-6 ms canonicalize + format!s + concatenations dance below.
    //
    // The .clone() of `policy` and `home` here is the cost of having
    // an owned cache key. It's a few hundred bytes for the worst
    // realistic policy and runs once per cache miss — negligible
    // next to the build cost it amortizes.
    let key = crate::seatbelt_cache::ProfileKey {
        canonical_root: canonical.clone(),
        home: home.clone(),
        proxy,
        policy: policy.clone(),
    };

    // Pre-compute the policy-overlay rules OUTSIDE the cache closure
    // because policy_overlay_rules returns Result and the cache API
    // takes an infallible closure. Validation is cheap (path char
    // checks) and idempotent, so doing it on every call is fine even
    // when the cache hits.
    let overlay = policy_overlay_rules(policy)?;

    let profile = crate::seatbelt_cache::get_or_compute(key, |k| {
        let root = k.canonical_root.to_string_lossy();
        let home = &k.home;
        // Start from the project profile then append credential deny
        // rules. Seatbelt evaluates rules in order; later rules win
        // for the same path, so the denies override the earlier broad
        // `(allow file-read*)`. Same last-match-wins approach as
        // Gemini CLI's seatbeltArgsBuilder.ts.
        let mut profile = match k.proxy {
            Some((port, allow_local_binding, weaker_macos_isolation)) => {
                build_proxied_profile_string(
                    &root,
                    home,
                    port,
                    allow_local_binding,
                    weaker_macos_isolation,
                )
            }
            None => build_profile_string(&root, home),
        };
        profile.push_str(&protected_subdir_deny_rules(&root));
        profile.push_str(&credential_deny_rules(home));
        // Gap 1 of #1072: enforce git hook + config write restrictions
        // when allow_git_config is false (the default). Emitted after
        // credential denies and before the caller-supplied policy overlay
        // so any future explicit allow_read_within_deny entries for git
        // paths still win. The flag is part of the cache key, so misses
        // are safe and hits are correct.
        if !k.policy.fs.allow_git_config {
            profile.push_str(&git_config_deny_rules(&root));
        }
        profile.push_str(&overlay);
        profile
    });

    let mut cmd = Command::new("sandbox-exec");
    cmd.arg("-p")
        .arg(profile)
        .arg("sh")
        .arg("-c")
        .arg(command)
        .current_dir(project_root);
    // #1228: scrub the LLM-bound shell's env down to the allowlist.
    // Seatbelt's SBPL profile gates filesystem access but does NOT touch
    // env vars — without this call, a prompt-injected `env | grep KEY`
    // exfiltrates OPENAI_API_KEY etc. straight into the LLM transcript.
    crate::env::scrub(&mut cmd, command);
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
/// restrict writes to project + temp + cache dirs. Network policy is
/// pluggable via `network_rules` so the same baseline serves both the
/// open-network mode (legacy) and the proxied mode (Phase 3a).
///
/// Use [`network_open_rules`] for the legacy behavior or
/// [`network_proxied_rules`] for deny-by-default + single-port hole.
pub(crate) fn build_profile_string_with_network(
    root: &str,
    home: &str,
    network_rules: &str,
) -> String {
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
         {network_rules}\
         (allow process-exec*)\n\
         (allow process-fork)\n\
         (allow sysctl-read)\n\
         (allow ipc-posix*)\n\
         (allow mach*)\n"
    )
}

/// Build the seatbelt profile for the project-mode baseline (open network).
///
/// Strategy: deny-by-default (allowlist), then open reads everywhere and
/// restrict writes to project + temp + cache dirs.  Network left
/// unrestricted so `curl` / `cargo fetch` / `npm install` work without
/// modification (Phase 3 adds the proxy-based egress filter).
pub(crate) fn build_profile_string(root: &str, home: &str) -> String {
    build_profile_string_with_network(root, home, &network_open_rules())
}

/// Network rules for the legacy unrestricted mode.
///
/// Equivalent to pre-Phase-3a behavior: every connect / bind / inbound is
/// allowed.
pub(crate) fn network_open_rules() -> String {
    "(allow network*)\n".to_string()
}

/// Network rules for the Phase 3a proxied mode.
///
/// Deny-by-default network with these holes:
///
/// - **Outbound TCP to `127.0.0.1:{proxy_port}`** — the single egress hop.
///   Every well-behaved HTTP client routed via [`crate::proxy::proxy_env_vars`]
///   reaches the proxy through this rule.
/// - **Outbound to Unix sockets** — git-over-ssh, ssh-agent, Docker socket
///   (when bind-mounted), language servers. These never touch the network
///   stack and the proxy can't intercept them anyway.
/// - **Inbound on `localhost:*`** — debugger attach, Node `--inspect`, etc.
///   Localhost-only, no security implication.
/// - **Bind on `localhost:*`** — always allowed for the same reason.
/// - **Bind on `*:*`** — only when `allow_local_binding` is true (user dev
///   servers expecting external clients).
///
/// When `weaker_macos_isolation` is true (Phase 3e of #934) we additionally
/// permit mach-lookups to Apple's `trustd` and `trustd.agent` daemons.
/// Without this, Go's standard library TLS — which validates server certs
/// by IPC to `trustd` rather than in-process — fails on every handshake
/// inside the proxied sandbox. Same toggle Claude Code exposes as
/// `enableWeakerNetworkIsolation`. The trade-off is intentional: trustd
/// learns the hostnames you're validating against (a metadata leak), but
/// it has no power to widen the network egress surface, which the kernel
/// SBPL still enforces independently.
///
/// Pattern mirrors Gemini CLI's `sandbox-macos-strict-proxied.sb` plus the
/// Unix-socket carve-out from Codex's seatbelt rules.
pub fn network_proxied_rules(
    proxy_port: u16,
    allow_local_binding: bool,
    weaker_macos_isolation: bool,
) -> String {
    let mut rules = String::new();
    // SBPL gotcha: `(remote tcp "<host>:<port>")` only accepts `*` or
    // `localhost` as the host — a literal `127.0.0.1` causes
    // sandbox-exec to reject the entire profile with
    // "host must be * or localhost in network address". `localhost`
    // covers both 127.0.0.1 and ::1 via the kernel resolver, so a
    // single rule is correct AND complete. Discovered the hard way
    // when wiring 3c kernel-enforcement to the koda-core test that
    // actually invokes sandbox-exec (the unit tests in 3b only
    // grepped the profile string and never asked the kernel to parse
    // it). See koda-core/src/sandbox.rs::tests::build_attaches_*.
    rules.push_str(&format!(
        "(allow network-outbound (remote tcp \"localhost:{proxy_port}\"))\n"
    ));
    rules.push_str("(allow network-outbound (remote unix-socket))\n");
    rules.push_str("(allow network-inbound (local ip \"localhost:*\"))\n");
    rules.push_str("(allow network-bind (local ip \"localhost:*\"))\n");
    if allow_local_binding {
        rules.push_str("(allow network-bind (local ip \"*:*\"))\n");
    }
    if weaker_macos_isolation {
        rules.push_str(&trustd_mach_lookup_rules());
    }
    rules
}

/// SBPL rules permitting mach-lookups to Apple's `trustd` family.
///
/// These two services are the entire reason the `weaker_macos_isolation`
/// flag exists. Kept as a separate fn so the snapshot tests can
/// substring-match against the exact wire form without re-deriving it.
///
/// Why both names: Apple split `trustd` into `trustd` (system) and
/// `trustd.agent` (per-user) somewhere around macOS 11; depending on
/// what the calling binary links against and which user it runs as,
/// either may be the actual lookup target. Permitting both is the only
/// way to handle every binary in one rule set.
fn trustd_mach_lookup_rules() -> String {
    let mut rules = String::from(
        "; \u{2500}\u{2500} weaker_macos_isolation: Apple trustd for Go-style out-of-process TLS \u{2500}\u{2500}\n",
    );
    rules.push_str("(allow mach-lookup (global-name \"com.apple.trustd\"))\n");
    rules.push_str("(allow mach-lookup (global-name \"com.apple.trustd.agent\"))\n");
    rules
}

/// Build the seatbelt profile for the proxied egress mode (Phase 3a).
///
/// Same baseline as the open-network profile but with [`network_proxied_rules`]
/// in place of the open-network allow.
pub fn build_proxied_profile_string(
    root: &str,
    home: &str,
    proxy_port: u16,
    allow_local_binding: bool,
    weaker_macos_isolation: bool,
) -> String {
    let net = network_proxied_rules(proxy_port, allow_local_binding, weaker_macos_isolation);
    build_profile_string_with_network(root, home, &net)
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

/// Deny writes to `.git/config` and `.git/hooks/` when
/// `policy.fs.allow_git_config` is `false` (the default).
///
/// These two paths are the primary vectors for git-based sandbox escape:
///
/// - `.git/config` — `git config core.fsmonitor <cmd>` or
///   `core.hookspath` register arbitrary executables that run on the
///   next porcelain command.  Preventing writes here blocks the
///   registration step.
/// - `.git/hooks/` — direct creation / replacement of hook scripts
///   (`post-checkout`, `pre-commit`, etc.) achieves the same outcome
///   without touching `config`.
///
/// Seatbelt's last-match-wins semantics let this deny override the
/// broad `(allow file-write* (subpath "{root}"))` emitted earlier.
/// Called only when git config writes are restricted — the flag is
/// part of the seatbelt-cache key, so profiles diverge correctly.
///
/// Injection safety: `root` was already validated by
/// [`validate_seatbelt_path`] at the call site; appending well-known
/// ASCII subpaths (`.git/hooks`, `.git/config`) introduces no new
/// S-expression metacharacters.
pub(crate) fn git_config_deny_rules(root: &str) -> String {
    let mut rules = String::from(
        "; ── deny git hook+config writes (allow_git_config = false, #1072 Gap 1) ──\n",
    );
    // Deny ALL writes inside .git/hooks — prevents direct hook placement.
    rules.push_str(&format!(
        "(deny file-write* (subpath \"{root}/.git/hooks\"))\n"
    ));
    // Deny writes to .git/config — blocks `git config core.fsmonitor …`.
    rules.push_str(&format!(
        "(deny file-write* (literal \"{root}/.git/config\"))\n"
    ));
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

    // ── Gap 1 of #1072: allow_git_config deny rules ──────────────────

    #[test]
    fn git_config_deny_rules_cover_hooks_and_config() {
        let rules = git_config_deny_rules("/work/myproject");
        assert!(
            rules.contains("(deny file-write* (subpath \"/work/myproject/.git/hooks\"))"),
            "hooks subpath must be denied: {rules}"
        );
        assert!(
            rules.contains("(deny file-write* (literal \"/work/myproject/.git/config\"))"),
            "git/config literal must be denied: {rules}"
        );
    }

    #[test]
    fn git_config_deny_rules_deny_only_writes_not_reads() {
        // Git commands need to READ hooks and config (e.g. `git status`
        // reads core.hooksPath). We must deny writes, not reads.
        let rules = git_config_deny_rules("/work");
        assert!(
            !rules.contains("deny file-read*"),
            "git config deny must not block reads (breaks git porcelain): {rules}"
        );
    }

    #[test]
    fn build_command_includes_git_deny_rules_when_not_allowed() {
        // End-to-end: policy with allow_git_config=false (the default)
        // must include the deny rules in the compiled profile.
        let policy = SandboxPolicy::strict_default(); // allow_git_config is false
        let cmd = build_command("true", Path::new("/tmp"), &policy).unwrap();
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let profile = &args[1]; // args: ["-p", "<profile>", "sh", "-c", "true"]
        assert!(
            profile.contains(".git/hooks"),
            "profile must contain git hooks deny when allow_git_config=false"
        );
        assert!(
            profile.contains(".git/config"),
            "profile must contain git config deny when allow_git_config=false"
        );
    }

    #[test]
    fn build_command_omits_git_deny_rules_when_explicitly_allowed() {
        // Callers that set allow_git_config=true (e.g. a dedicated
        // git-workflow agent) must not get the deny rules — they'd break
        // hook-based workflows that the caller explicitly approved.
        let mut policy = SandboxPolicy::strict_default();
        policy.fs.allow_git_config = true;
        let cmd = build_command("true", Path::new("/tmp"), &policy).unwrap();
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let profile = &args[1];
        // The git deny block header must be absent.
        assert!(
            !profile.contains("allow_git_config = false, #1072"),
            "profile must not include git deny rules when allow_git_config=true"
        );
    }

    // ── SEC-001 (v0.2.21 release audit): resolution-aware tests ──────
    //
    // SBPL is last-match-wins. The pre-#1085 tests above only assert that
    // deny strings APPEAR in the profile, not that they WIN resolution.
    // SEC-001 was a real escape: deny rules were emitted but immediately
    // overridden by the policy overlay's `allow file-write* (subpath ROOT)`.
    // These tests model rule resolution and would have caught the bug.

    /// Walk the SBPL profile and return the index (line offset) of the
    /// LAST rule that grants/denies `file-write*` for `path`.
    /// Returns `("allow" | "deny", line_idx)` of the winning rule, or
    /// `None` if no rule mentions the path.
    ///
    /// A rule "matches" `path` if the path equals its `literal` arg or
    /// is equal-to-or-a-descendant-of its `subpath` arg. This mirrors
    /// macOS sandbox's actual resolution semantics for the rule shapes
    /// we emit.
    fn last_write_rule_for(profile: &str, path: &str) -> Option<(&'static str, usize)> {
        let mut winner: Option<(&'static str, usize)> = None;
        for (idx, line) in profile.lines().enumerate() {
            let trimmed = line.trim_start();
            let kind = if trimmed.starts_with("(allow file-write*") {
                "allow"
            } else if trimmed.starts_with("(deny file-write*") {
                "deny"
            } else {
                continue;
            };
            // Extract the quoted argument: ...(literal "X")) or ...(subpath "X"))
            let Some(q1) = line.find('"') else { continue };
            let Some(q2) = line[q1 + 1..].find('"') else {
                continue;
            };
            let arg = &line[q1 + 1..q1 + 1 + q2];
            let matches = if line[..q1].contains("(literal") {
                arg == path
            } else if line[..q1].contains("(subpath") {
                path == arg || path.starts_with(&format!("{arg}/"))
            } else {
                false
            };
            if matches {
                winner = Some((kind, idx));
            }
        }
        winner
    }

    #[test]
    fn sec_001_git_config_write_is_denied_in_resolved_profile() {
        // Mirror what policy_for_agent (in koda-core) seeds: the project
        // root in allow_write AND the git deny pair in
        // deny_write_within_allow. Without the deny_write_within_allow
        // seed, this test fails (allow at line N wins after deny at line
        // M, where M < N) — which is exactly SEC-001.
        let mut policy = SandboxPolicy::strict_default();
        policy.fs.allow_write = vec!["/work".into()];
        policy.fs.deny_write_within_allow =
            vec!["/work/.git/hooks".into(), "/work/.git/config".into()];
        let cmd = build_command("true", Path::new("/work"), &policy).unwrap();
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let profile = &args[1];

        let (kind, _idx) = last_write_rule_for(profile, "/work/.git/config")
            .expect("some rule must mention /work/.git/config");
        assert_eq!(
            kind, "deny",
            "SEC-001 regression: last matching write rule for /work/.git/config must be `deny`. \
             Profile:\n{profile}"
        );

        let (kind, _idx) = last_write_rule_for(profile, "/work/.git/hooks/post-checkout")
            .expect("some rule must mention /work/.git/hooks/post-checkout");
        assert_eq!(
            kind, "deny",
            "SEC-001 regression: last matching write rule for a hook script must be `deny`. \
             Profile:\n{profile}"
        );
    }

    #[test]
    fn sec_001_unrelated_project_path_remains_writable() {
        // Sanity check on the resolution simulator and the fix: paths that
        // are NOT under the protected git subpaths must still be writable.
        // Otherwise the fix would have over-corrected and broken normal
        // workflows.
        let mut policy = SandboxPolicy::strict_default();
        policy.fs.allow_write = vec!["/work".into()];
        policy.fs.deny_write_within_allow =
            vec!["/work/.git/hooks".into(), "/work/.git/config".into()];
        let cmd = build_command("true", Path::new("/work"), &policy).unwrap();
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let profile = &args[1];

        let (kind, _idx) = last_write_rule_for(profile, "/work/src/main.rs")
            .expect("some rule must mention /work/src/main.rs (via subpath /work)");
        assert_eq!(
            kind, "allow",
            "SEC-001 fix must not over-correct: a normal source file under \
             the project root must still be writable. Profile:\n{profile}"
        );
    }

    // ── Phase 3a: proxied network rules ─────────────────────────────

    #[test]
    fn proxied_profile_omits_open_network_allow() {
        let p = build_proxied_profile_string("/work", "/Users/x", 8877, false, false);
        assert!(
            !p.contains("(allow network*)\n"),
            "proxied profile must not include the open-network allow\nprofile:\n{p}"
        );
    }

    #[test]
    fn proxied_profile_allows_only_proxy_port_outbound_tcp() {
        let p = build_proxied_profile_string("/work", "/Users/x", 8877, false, false);
        // Phase 3c fix: SBPL only accepts `*` or `localhost` as the
        // host in `(remote tcp ...)`. A literal `127.0.0.1` causes
        // sandbox-exec to reject the entire profile. `localhost`
        // resolves to both 127.0.0.1 and ::1 via the kernel, so a
        // single rule is correct AND complete.
        assert!(
            p.contains("(allow network-outbound (remote tcp \"localhost:8877\"))"),
            "profile must include the localhost:proxy_port allow; got:\n{p}"
        );
        assert!(
            !p.contains("127.0.0.1:8877"),
            "profile must NOT include literal 127.0.0.1 — SBPL rejects it; got:\n{p}"
        );
        // Exactly one tcp outbound rule should be present.
        let count = p.matches("(allow network-outbound (remote tcp ").count();
        assert_eq!(
            count, 1,
            "expected exactly one (localhost) outbound rule; profile:\n{p}"
        );
    }

    #[test]
    fn proxied_profile_always_allows_unix_socket_outbound() {
        // git-over-ssh, ssh-agent, language servers, Docker.sock when
        // bind-mounted — all need this. Codex's seatbelt has the same
        // carve-out.
        let p = build_proxied_profile_string("/work", "/Users/x", 8877, false, false);
        assert!(p.contains("(allow network-outbound (remote unix-socket))"));
    }

    #[test]
    fn proxied_profile_allows_localhost_inbound_for_debuggers() {
        let p = build_proxied_profile_string("/work", "/Users/x", 8877, false, false);
        assert!(p.contains("(allow network-inbound (local ip \"localhost:*\"))"));
    }

    #[test]
    fn proxied_profile_omits_wildcard_bind_when_local_binding_disabled() {
        let p = build_proxied_profile_string("/work", "/Users/x", 8877, false, false);
        // Localhost bind is always allowed; wildcard *:* must not be.
        assert!(p.contains("(allow network-bind (local ip \"localhost:*\"))"));
        assert!(
            !p.contains("(allow network-bind (local ip \"*:*\"))"),
            "wildcard bind must be gated on allow_local_binding"
        );
    }

    #[test]
    fn proxied_profile_includes_wildcard_bind_when_local_binding_enabled() {
        let p = build_proxied_profile_string("/work", "/Users/x", 8877, true, false);
        assert!(p.contains("(allow network-bind (local ip \"*:*\"))"));
    }

    #[test]
    fn proxied_profile_keeps_baseline_file_writes_unchanged() {
        // Regression guard: the proxied variant must only differ from the
        // open variant in the network section. Same temp/cargo/npm/cache
        // writes should still be present.
        let p = build_proxied_profile_string("/work", "/Users/x", 8877, false, false);
        assert!(p.contains("(subpath \"/work\")"));
        assert!(p.contains("(subpath \"/private/tmp\")"));
        assert!(p.contains("(subpath \"/Users/x/.cargo\")"));
        assert!(p.contains("(subpath \"/Users/x/.npm\")"));
        assert!(p.contains("(subpath \"/Users/x/.cache\")"));
    }

    #[test]
    fn proxied_and_open_differ_only_in_network_section() {
        // DRY guard: confirm that build_profile_string and
        // build_proxied_profile_string share the same baseline.
        let open = build_profile_string("/work", "/Users/x");
        let proxied = build_proxied_profile_string("/work", "/Users/x", 8877, false, false);

        // Both must end the same way (process-exec onward is identical).
        let tail = "(allow process-exec*)\n(allow process-fork)\n(allow sysctl-read)\n(allow ipc-posix*)\n(allow mach*)\n";
        assert!(open.ends_with(tail), "open profile tail mismatch:\n{open}");
        assert!(
            proxied.ends_with(tail),
            "proxied profile tail mismatch:\n{proxied}"
        );
    }

    #[test]
    fn build_command_with_proxy_uses_proxied_profile() {
        // End-to-end check: the public entry point must produce a profile
        // that contains the proxy-port outbound rule and omits the
        // open-network allow.
        let cmd = build_command_with_proxy(
            "true",
            Path::new("/tmp"),
            &SandboxPolicy::default(),
            8877,
            false,
            false,
        )
        .unwrap();
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        // sandbox-exec layout: ["-p", <profile>, "sh", "-c", <cmd>]
        let profile = &args[1];
        // SBPL host syntax: must be `localhost` not `127.0.0.1` (kernel
        // resolves localhost to both v4 + v6 loopback). See
        // [`network_proxied_rules`].
        assert!(profile.contains("localhost:8877"));
        assert!(!profile.contains("(allow network*)\n"));
    }

    #[test]
    fn build_command_without_proxy_keeps_open_network() {
        // Regression guard: the no-proxy entry point must still produce
        // the open-network profile (callers without a proxy should keep
        // current behavior).
        let cmd = build_command("true", Path::new("/tmp"), &SandboxPolicy::default()).unwrap();
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let profile = &args[1];
        assert!(profile.contains("(allow network*)\n"));
        assert!(!profile.contains("127.0.0.1:"));
    }

    #[test]
    fn build_command_with_proxy_local_binding_propagates() {
        // allow_local_binding=true should produce the wildcard bind rule.
        let cmd = build_command_with_proxy(
            "true",
            Path::new("/tmp"),
            &SandboxPolicy::default(),
            8877,
            true,
            false,
        )
        .unwrap();
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let profile = &args[1];
        assert!(profile.contains("(allow network-bind (local ip \"*:*\"))"));
    }

    // ── Phase 3e: weaker_macos_isolation toggle ────────────────────────────

    #[test]
    fn proxied_profile_omits_trustd_by_default() {
        // Strict default: no mach-lookup carve-outs. The trustd daemons
        // are deliberately UNREACHABLE without the explicit opt-in,
        // mirroring the deny-by-default network posture.
        let p = build_proxied_profile_string("/work", "/Users/x", 8877, false, false);
        assert!(
            !p.contains("com.apple.trustd"),
            "strict default must not permit trustd lookups; got:\n{p}"
        );
        assert!(
            !p.contains("mach-lookup"),
            "strict default must not contain any mach-lookup allow; got:\n{p}"
        );
    }

    #[test]
    fn proxied_profile_permits_trustd_when_weaker_isolation_set() {
        // Opt-in: both trustd names must appear so per-user agent and
        // system daemon lookups both succeed regardless of binary.
        let p = build_proxied_profile_string("/work", "/Users/x", 8877, false, true);
        assert!(
            p.contains("(allow mach-lookup (global-name \"com.apple.trustd\"))"),
            "weaker isolation must permit com.apple.trustd; got:\n{p}"
        );
        assert!(
            p.contains("(allow mach-lookup (global-name \"com.apple.trustd.agent\"))"),
            "weaker isolation must permit com.apple.trustd.agent; got:\n{p}"
        );
    }

    #[test]
    fn weaker_isolation_does_not_widen_network_egress() {
        // The whole point of the toggle: it relaxes mach-lookup ONLY.
        // The kernel-enforced TCP egress remains pinned to the loopback
        // proxy port. Regression guard against a future maintainer
        // sneaking a `(allow network*)` in alongside the trustd rules.
        let p = build_proxied_profile_string("/work", "/Users/x", 8877, false, true);
        assert!(
            !p.contains("(allow network*)\n"),
            "weaker isolation must not include the open-network blanket allow; got:\n{p}"
        );
        assert!(
            p.contains("(allow network-outbound (remote tcp \"localhost:8877\"))"),
            "single-port loopback rule must remain intact; got:\n{p}"
        );
    }

    #[test]
    fn weaker_isolation_propagates_through_build_command_with_proxy() {
        // End-to-end: the public entry point must thread the bool all
        // the way to the emitted profile.
        let cmd = build_command_with_proxy(
            "true",
            Path::new("/tmp"),
            &SandboxPolicy::default(),
            8877,
            false,
            true,
        )
        .unwrap();
        let profile = cmd
            .as_std()
            .get_args()
            .nth(1)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            profile.contains("com.apple.trustd"),
            "weaker isolation flag must reach the profile; got:\n{profile}"
        );
    }

    #[test]
    fn trustd_rules_are_idempotent_on_section_header() {
        // The section header is a comment used as a visual landmark in
        // log output. If a future refactor changes the header text, the
        // log/grep tooling that hunts for sandbox events will silently
        // stop matching. Pin the exact prefix.
        let rules = trustd_mach_lookup_rules();
        assert!(
            rules.starts_with("; "),
            "trustd block must lead with an SBPL comment; got:\n{rules}"
        );
        assert!(
            rules.contains("weaker_macos_isolation"),
            "comment must mention the policy field name for grep-ability; got:\n{rules}"
        );
    }

    // ── Phase 4a: cache wiring integration tests ────────────────────────────────
    //
    // The cache module has dedicated unit tests for its own state
    // machine. These tests prove that build_command actually goes
    // through the cache (not bypasses it) and that cached profiles
    // remain semantically correct — catching the regression where a
    // refactor accidentally bypasses the cache or returns a stale
    // profile that drops a deny rule.

    #[test]
    fn build_command_returns_byte_identical_profile_on_repeated_calls() {
        // Same args twice → same emitted profile, byte-for-byte. This
        // is the contract every caller relies on. With the cache, the
        // second call hits the warm path; without it, both calls
        // recompute. Either way the strings must match exactly — if
        // they don't, something non-deterministic crept into the
        // build pipeline (timestamps, hash randomization, etc.) and
        // the cache would be a correctness hazard, not a perf win.
        let policy = SandboxPolicy::default();
        let cmd1 = build_command("true", Path::new("/tmp"), &policy).unwrap();
        let cmd2 = build_command("true", Path::new("/tmp"), &policy).unwrap();
        let p1 = cmd1
            .as_std()
            .get_args()
            .nth(1)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let p2 = cmd2
            .as_std()
            .get_args()
            .nth(1)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(p1, p2, "repeated build_command must emit identical SBPL");
        assert!(
            p1.contains("(version 1)"),
            "profile must look real, not empty"
        );
    }

    #[test]
    fn cached_profile_still_includes_credential_denies() {
        // Regression guard: if a future refactor accidentally moves
        // credential_deny_rules OUT of the cached build closure, the
        // first call would produce the right profile but cached hits
        // would drop the credential denies — a silent privilege
        // escalation. Force a likely cache hit by calling twice and
        // asserting the second result still contains the SSH-deny.
        let policy = SandboxPolicy::default();
        let _warm = build_command("true", Path::new("/tmp"), &policy).unwrap();
        let cmd = build_command("true", Path::new("/tmp"), &policy).unwrap();
        let profile = cmd
            .as_std()
            .get_args()
            .nth(1)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        // credential_deny_rules emits subpath denies under $HOME for
        // .ssh, .aws, .gnupg, etc. Just check one that's universal.
        assert!(
            profile.contains(".ssh"),
            "cached profile must still carry the credential deny rules; got:\n{profile}"
        );
    }

    #[test]
    fn proxy_and_open_profiles_do_not_share_cache_entry() {
        // The pathological case the cache MUST get right: the same
        // root + policy with proxy=Some vs proxy=None must produce
        // different profiles. Mixing them is a 3c kernel-enforcement
        // bypass — the open profile would let TCP egress that the
        // proxied profile denies.
        let policy = SandboxPolicy::default();
        let cmd_open = build_command("true", Path::new("/tmp"), &policy).unwrap();
        let cmd_proxied =
            build_command_with_proxy("true", Path::new("/tmp"), &policy, 8877, false, false)
                .unwrap();
        let p_open = cmd_open
            .as_std()
            .get_args()
            .nth(1)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let p_proxied = cmd_proxied
            .as_std()
            .get_args()
            .nth(1)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            p_open.contains("(allow network*)\n"),
            "open profile must contain blanket network allow"
        );
        assert!(
            !p_proxied.contains("(allow network*)\n"),
            "proxied profile must NOT contain blanket allow (would bypass kernel egress enforcement)"
        );
        assert!(
            p_proxied.contains("localhost:8877"),
            "proxied profile must pin to the loopback proxy port"
        );
    }
}
