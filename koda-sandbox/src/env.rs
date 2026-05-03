//! Environment-variable scrubbing for sandboxed shell tool calls (#1228).
//!
//! ## Why
//!
//! Before this module, every sandboxed shell tool inherited the **entire
//! parent koda process env**, including secrets like `OPENAI_API_KEY`,
//! `AWS_SECRET_ACCESS_KEY`, `GITHUB_TOKEN`, etc. A prompt-injected
//! sub-agent that convinced the model to run `env`, `printenv`, or
//! `bash -c 'echo $OPENAI_API_KEY'` would exfiltrate them straight into
//! the LLM transcript — and from there into the next provider request.
//!
//! ## How
//!
//! Every sandbox runtime ([`crate::seatbelt`], `crate::bwrap` (Linux),
//! [`crate::UnsandboxedRuntime`]) calls [`scrub`] on its constructed
//! [`Command`] before returning it. `scrub` does:
//!
//! 1. [`Command::env_clear`] — drop the entire inherited env.
//! 2. Re-add a fixed [`SAFE_BASE_VARS`] allowlist (locale, identity,
//!    `PATH`, tmpdir, proxy — never tokens / keys / secrets).
//! 3. Re-add a per-tool extras allowlist keyed on the *first token* of
//!    the user-supplied command (so `cargo build` gets `CARGO_HOME` /
//!    `RUSTUP_HOME` but `ls -la` doesn't).
//!
//! Values are read from the *current* process env at scrub time. A
//! missing var is silently skipped (no panic, no warning — locale vars
//! in particular are often absent).
//!
//! ## Why allowlist instead of denylist
//!
//! Denylists ("strip `*_KEY`, `*_SECRET`, `*_TOKEN`, …") are a footgun:
//! every new framework invents new credential var names (`SUPABASE_…`,
//! `VERCEL_…`, `SENTRY_…`), and the denylist will always trail. An
//! allowlist fails *closed* — unknown vars are dropped by default and
//! must be explicitly added, which is the secure default.
//!
//! ## Per-tool extras: argv\[0\] heuristic
//!
//! [`tool_extras_for`] is keyed on the basename of the first whitespace-
//! separated token of the raw command string. This handles:
//!
//! - `cargo build --release` → argv\[0\] = `cargo` ✓
//! - `/usr/bin/git status`   → argv\[0\] = `git`   ✓ (path stripped)
//! - `cargo test 2>&1 | tee` → argv\[0\] = `cargo` ✓ (pipeline tail dropped)
//! - `bash -c '…'`           → argv\[0\] = `bash`  ✓ (no extras = secure)
//!
//! It deliberately does NOT handle:
//!
//! - `MY_VAR=foo cargo build` → argv\[0\] = `MY_VAR=foo` (no extras → cargo
//!   may fail to find its cache; but `MY_VAR` reaches the inner `cargo`
//!   because the *shell* sets it inline, so the user's intent survives).
//! - `(cd sub && cargo build)` → argv\[0\] = `(cd` (no extras → subshell
//!   parens are rare enough to accept the failure).
//!
//! Both edge cases fail *closed* (no extras forwarded), which is the
//! correct security posture. Users can work around by setting vars
//! inline (`CARGO_HOME=/path cargo build`) or by adding their own
//! allowlist entries in a future config knob (#1229, follow-up).

use std::process::Command as StdCommand;
use tokio::process::Command;

/// Allowlisted env var **names**. Values come from the parent process
/// env at scrub time; missing values are silently skipped.
///
/// **Categories** (do not reorder casually — the comments are the
/// security audit trail):
///
/// - **Identity** (`HOME`, `USER`, `LOGNAME`, `SHELL`): required by
///   git, ssh, package managers. Not credentials.
/// - **Locale** (`LANG`, `LC_*`, `TERM`): UTF-8 + colour. Without
///   these, many tools mojibake or refuse to run.
/// - **Paths** (`PATH`, `TMPDIR`, `TMP`, `TEMP`, `PWD`): required
///   to find the tool itself + scratch space.
/// - **Proxy** (`HTTP_PROXY` etc.): URL-with-no-creds form; if
///   credentials are baked into the URL the user has bigger problems.
///   Both upper- and lower-case variants because tools disagree on
///   convention (curl: lower, Java: upper, Rust: both).
pub const SAFE_BASE_VARS: &[&str] = &[
    // ── Identity ──
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    // ── Locale ──
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LC_MESSAGES",
    "LC_COLLATE",
    "LC_NUMERIC",
    "LC_TIME",
    "TERM",
    // ── Paths ──
    "PATH",
    "TMPDIR",
    "TMP",
    "TEMP",
    "PWD",
    // ── Network proxy (no creds in standard form) ──
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
];

/// Per-tool env var allowlist, keyed on the resolved binary basename.
///
/// Returns the empty slice for unknown tools — the secure default.
///
/// ## Audit notes per tool
///
/// - **Rust toolchain**: `CARGO_HOME` / `RUSTUP_HOME` are filesystem
///   paths to caches, not credentials. `RUST_LOG` / `RUST_BACKTRACE`
///   are diagnostic knobs. `RUSTC_WRAPPER` is a build-system hook.
/// - **git**: `GIT_AUTHOR_*` / `GIT_COMMITTER_*` are name+email; even
///   if attacker-readable, they're public commit metadata anyway.
///   Notably **excluded**: `GIT_ASKPASS`, `GIT_SSH_COMMAND` (could
///   coerce credential prompts), `GIT_HTTP_*` (HTTP creds).
/// - **Node ecosystem**: `NODE_PATH` is a module search path.
///   `NPM_CONFIG_USERCONFIG` points to `.npmrc` (which itself may
///   contain `_authToken` lines — but the file lives on disk, not in
///   env, so reading it requires filesystem access we don't grant by
///   default to non-cwd paths). **Excluded**: `NPM_TOKEN`,
///   `NODE_AUTH_TOKEN`.
/// - **Python**: `PYTHONPATH` / `VIRTUAL_ENV` / `PYENV_*` are paths.
///   **Excluded**: `PYPI_TOKEN`, anything matching `*_API_KEY`.
/// - **Cloud CLIs**: paths to config files only, not creds. The CLIs
///   then read creds from those files (which the sandbox FS policy
///   gates separately). **Excluded**: every `*_TOKEN`, `*_SECRET`,
///   `*_ACCESS_KEY` variant.
/// - **make**: `MAKEFLAGS` / `MAKELEVEL` are recursion bookkeeping.
///   Required for parallel builds to behave.
pub fn tool_extras_for(argv0: &str) -> &'static [&'static str] {
    match argv0 {
        // Rust
        "cargo" | "rustc" | "rustup" | "rustfmt" | "clippy-driver" => &[
            "CARGO_HOME",
            "RUSTUP_HOME",
            "RUST_LOG",
            "RUST_BACKTRACE",
            "RUSTC_WRAPPER",
            "CARGO_TARGET_DIR",
        ],
        // git (NOT _ASKPASS, _SSH_COMMAND, _HTTP_* — those are creds vectors)
        "git" => &[
            "GIT_AUTHOR_NAME",
            "GIT_AUTHOR_EMAIL",
            "GIT_COMMITTER_NAME",
            "GIT_COMMITTER_EMAIL",
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_PAGER",
        ],
        // Node / JS (NOT NPM_TOKEN, NODE_AUTH_TOKEN)
        "npm" | "node" | "yarn" | "pnpm" | "npx" => {
            &["NODE_PATH", "NPM_CONFIG_USERCONFIG", "NODE_ENV"]
        }
        // Python (NOT PYPI_TOKEN)
        "python" | "python3" | "pip" | "pip3" | "uv" | "pipx" | "poetry" => &[
            "PYTHONPATH",
            "VIRTUAL_ENV",
            "PYENV_ROOT",
            "PYENV_VERSION",
            "PIPX_HOME",
            "PIPX_BIN_DIR",
        ],
        // Kubernetes (KUBECONFIG is a file path; creds inside the file)
        "kubectl" | "helm" | "k9s" => &["KUBECONFIG"],
        // Container runtimes
        "docker" | "podman" => &["DOCKER_HOST", "DOCKER_CONFIG"],
        // GCP (config dir only, not auth tokens)
        "gcloud" | "bq" | "gsutil" => &["CLOUDSDK_CONFIG", "CLOUDSDK_ACTIVE_CONFIG_NAME"],
        // AWS (config + profile name; creds in ~/.aws/credentials file)
        "aws" => &[
            "AWS_CONFIG_FILE",
            "AWS_PROFILE",
            "AWS_REGION",
            "AWS_DEFAULT_REGION",
            "AWS_SHARED_CREDENTIALS_FILE",
        ],
        // make
        "make" | "gmake" => &["MAKEFLAGS", "MAKELEVEL"],
        // Unknown tool → no extras (secure default)
        _ => &[],
    }
}

/// Extract argv\[0\] from a raw shell command string.
///
/// See module docs for the heuristic and its known limitations.
fn parse_argv0(raw_command: &str) -> &str {
    raw_command
        .split_whitespace()
        .next()
        .unwrap_or("")
        .rsplit('/')
        .next()
        .unwrap_or("")
}

/// Scrub `cmd`'s env down to the allowlist. Call this on every
/// sandbox-bound `Command` before spawning.
///
/// `raw_command` is the *user-supplied* shell command (the inner
/// command, not the `sh -c` wrapper) — used to look up per-tool
/// extras via [`tool_extras_for`].
pub fn scrub(cmd: &mut Command, raw_command: &str) {
    cmd.env_clear();
    apply_allowlist(raw_command, |name, value| {
        cmd.env(name, value);
    });
}

/// `std::process::Command` variant of [`scrub`]. The `is_available`
/// probes use the std variant; production runtime calls go through the
/// tokio variant.
pub fn scrub_std(cmd: &mut StdCommand, raw_command: &str) {
    cmd.env_clear();
    apply_allowlist(raw_command, |name, value| {
        cmd.env(name, value);
    });
}

/// Inner: walk the allowlist and invoke `set(name, value)` for every
/// var present in the parent env. Shared by both [`scrub`] variants
/// to keep the allowlist application logic DRY.
fn apply_allowlist(raw_command: &str, mut set: impl FnMut(&str, String)) {
    for name in SAFE_BASE_VARS {
        if let Ok(value) = std::env::var(name) {
            set(name, value);
        }
    }
    let argv0 = parse_argv0(raw_command);
    for name in tool_extras_for(argv0) {
        if let Ok(value) = std::env::var(name) {
            set(name, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_argv0 ──────────────────────────────────────────────────

    #[test]
    fn parse_argv0_simple_command() {
        assert_eq!(parse_argv0("cargo build"), "cargo");
    }

    #[test]
    fn parse_argv0_strips_path_prefix() {
        assert_eq!(parse_argv0("/usr/bin/git status"), "git");
    }

    #[test]
    fn parse_argv0_with_pipeline() {
        assert_eq!(parse_argv0("cargo test 2>&1 | tee out.log"), "cargo");
    }

    #[test]
    fn parse_argv0_empty() {
        assert_eq!(parse_argv0(""), "");
    }

    #[test]
    fn parse_argv0_whitespace_only() {
        assert_eq!(parse_argv0("   "), "");
    }

    #[test]
    fn parse_argv0_bash_dash_c() {
        // bash -c gets no extras — that's the secure default.
        assert_eq!(parse_argv0("bash -c 'cargo build'"), "bash");
    }

    // ── tool_extras_for ──────────────────────────────────────────────

    #[test]
    fn tool_extras_for_cargo_includes_cargo_home() {
        assert!(tool_extras_for("cargo").contains(&"CARGO_HOME"));
    }

    #[test]
    fn tool_extras_for_git_excludes_credential_vectors() {
        let extras = tool_extras_for("git");
        // Author info OK
        assert!(extras.contains(&"GIT_AUTHOR_NAME"));
        // Credential vectors NOT OK
        assert!(!extras.contains(&"GIT_ASKPASS"));
        assert!(!extras.contains(&"GIT_SSH_COMMAND"));
        assert!(!extras.contains(&"GIT_HTTP_USER_AGENT"));
    }

    #[test]
    fn tool_extras_for_npm_excludes_token() {
        let extras = tool_extras_for("npm");
        assert!(!extras.contains(&"NPM_TOKEN"));
        assert!(!extras.contains(&"NODE_AUTH_TOKEN"));
    }

    #[test]
    fn tool_extras_for_aws_excludes_secret_access_key() {
        let extras = tool_extras_for("aws");
        // Config + profile + region OK
        assert!(extras.contains(&"AWS_PROFILE"));
        // Creds NOT OK
        assert!(!extras.contains(&"AWS_SECRET_ACCESS_KEY"));
        assert!(!extras.contains(&"AWS_ACCESS_KEY_ID"));
        assert!(!extras.contains(&"AWS_SESSION_TOKEN"));
    }

    #[test]
    fn tool_extras_for_unknown_tool_returns_empty() {
        assert!(tool_extras_for("ls").is_empty());
        assert!(tool_extras_for("rm").is_empty());
        assert!(tool_extras_for("totally-bespoke-tool").is_empty());
    }

    #[test]
    fn safe_base_vars_excludes_all_credential_patterns() {
        // Defense-in-depth: the SAFE_BASE_VARS list itself must not
        // accidentally include a credential-shaped var.
        for var in SAFE_BASE_VARS {
            let upper = var.to_uppercase();
            assert!(!upper.contains("KEY"), "SAFE_BASE_VARS contains {var}");
            assert!(!upper.contains("SECRET"), "SAFE_BASE_VARS contains {var}");
            assert!(!upper.contains("TOKEN"), "SAFE_BASE_VARS contains {var}");
            assert!(!upper.contains("PASSWORD"), "SAFE_BASE_VARS contains {var}");
            assert!(!upper.contains("CRED"), "SAFE_BASE_VARS contains {var}");
        }
    }

    #[test]
    fn tool_extras_excludes_all_credential_patterns() {
        // Same defense for every per-tool extras list.
        for argv0 in [
            "cargo", "git", "npm", "node", "python", "pip", "uv", "kubectl", "helm", "docker",
            "gcloud", "aws", "make",
        ] {
            for var in tool_extras_for(argv0) {
                let upper = var.to_uppercase();
                assert!(
                    !upper.contains("TOKEN"),
                    "{argv0} extras contain TOKEN-shaped var: {var}"
                );
                assert!(
                    !upper.contains("SECRET"),
                    "{argv0} extras contain SECRET-shaped var: {var}"
                );
                assert!(
                    !upper.contains("PASSWORD"),
                    "{argv0} extras contain PASSWORD-shaped var: {var}"
                );
                // "KEY" allowed only inside AWS_ACCESS_KEY_ID-style names which
                // we explicitly exclude above; double-check none slipped in.
                if upper.contains("KEY") {
                    panic!("{argv0} extras contain KEY-shaped var: {var}");
                }
            }
        }
    }

    // ── scrub end-to-end (real `env` subprocess) ─────────────────────

    /// Spawn `env` through a scrubbed Command and return stdout.
    /// Sets a known poison value in the current process env so the
    /// caller can assert it doesn't appear in the child's env.
    fn run_env_with_poison(poison_var: &str, poison_val: &str, raw_command: &str) -> String {
        // SAFETY: `set_var` is only safe in single-threaded contexts.
        // Cargo runs each test in its own thread but the *process* may
        // be multi-threaded by other tests racing this one. We mitigate
        // by using poison values unique per test (so one test's poison
        // doesn't bleed into another's assertion) — see callers.
        unsafe {
            std::env::set_var(poison_var, poison_val);
        }
        let mut cmd = StdCommand::new("env");
        scrub_std(&mut cmd, raw_command);
        let output = cmd.output().expect("env spawn");
        unsafe {
            std::env::remove_var(poison_var);
        }
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    #[test]
    fn scrub_strips_openai_api_key() {
        let env_dump = run_env_with_poison("KODA_TEST_OPENAI_KEY_1228", "sk-must-not-leak", "ls");
        assert!(
            !env_dump.contains("sk-must-not-leak"),
            "scrub leaked the poison value into child env:\n{env_dump}"
        );
        assert!(
            !env_dump.contains("KODA_TEST_OPENAI_KEY_1228"),
            "scrub leaked the var name into child env:\n{env_dump}"
        );
    }

    #[test]
    fn scrub_strips_aws_secret_access_key() {
        let env_dump = run_env_with_poison(
            "KODA_TEST_AWS_SECRET_1228",
            "wJalrXUtnFEMI-must-not-leak",
            "aws s3 ls",
        );
        assert!(
            !env_dump.contains("wJalrXUtnFEMI-must-not-leak"),
            "scrub leaked AWS-shaped secret:\n{env_dump}"
        );
    }

    #[test]
    fn scrub_strips_github_token() {
        let env_dump = run_env_with_poison(
            "KODA_TEST_GITHUB_TOKEN_1228",
            "ghp_must-not-leak",
            "git status",
        );
        assert!(
            !env_dump.contains("ghp_must-not-leak"),
            "scrub leaked GITHUB_TOKEN-shaped value:\n{env_dump}"
        );
    }

    #[test]
    fn scrub_keeps_path() {
        // PATH must survive — without it the sandboxed shell can't find
        // any tool. This is the single most important "did we break it"
        // sanity check.
        let mut cmd = StdCommand::new("env");
        scrub_std(&mut cmd, "ls");
        let output = cmd.output().expect("env spawn");
        let env_dump = String::from_utf8_lossy(&output.stdout);
        assert!(
            env_dump.contains("PATH="),
            "scrub dropped PATH — sandbox would be unable to find any tool:\n{env_dump}"
        );
    }

    #[test]
    fn scrub_per_tool_extras_for_cargo_only() {
        // Set CARGO_HOME and verify it survives for `cargo …` but NOT for `ls`.
        unsafe {
            std::env::set_var("CARGO_HOME", "/tmp/koda-test-cargo-home-1228");
        }

        let mut cargo_cmd = StdCommand::new("env");
        scrub_std(&mut cargo_cmd, "cargo build");
        let cargo_env =
            String::from_utf8_lossy(&cargo_cmd.output().expect("env").stdout).into_owned();

        let mut ls_cmd = StdCommand::new("env");
        scrub_std(&mut ls_cmd, "ls -la");
        let ls_env = String::from_utf8_lossy(&ls_cmd.output().expect("env").stdout).into_owned();

        unsafe {
            std::env::remove_var("CARGO_HOME");
        }

        assert!(
            cargo_env.contains("/tmp/koda-test-cargo-home-1228"),
            "CARGO_HOME should pass through for `cargo …`:\n{cargo_env}"
        );
        assert!(
            !ls_env.contains("/tmp/koda-test-cargo-home-1228"),
            "CARGO_HOME should NOT pass through for `ls`:\n{ls_env}"
        );
    }
}
