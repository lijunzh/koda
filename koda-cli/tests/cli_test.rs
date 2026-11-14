//! CLI binary integration tests.
//!
//! Tests that the compiled binary handles command-line arguments correctly.
//! These run the actual `koda` binary as a subprocess.

use std::process::Command;

/// Get the path to the built binary.
fn koda_bin() -> String {
    // cargo test builds the binary in target/debug/
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // remove test binary name
    path.pop(); // remove deps/
    path.push("koda");
    path.to_string_lossy().to_string()
}

#[test]
fn test_cli_version() {
    let output = Command::new(koda_bin())
        .arg("--version")
        .output()
        .expect("Failed to run koda --version");

    assert!(output.status.success(), "koda --version should succeed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("koda"),
        "Version output should contain 'koda': {stdout}"
    );
    let version = env!("CARGO_PKG_VERSION");
    assert!(
        stdout.contains(version),
        "Version output should contain '{version}': {stdout}"
    );
    // #1259-followup: --version is the user-facing replacement for
    // the (removed) `koda doctor` subcommand. It MUST surface kernel
    // sandbox state on a one-liner so bug filers can paste it without
    // a second command. Pin both halves so a future refactor that
    // strips it becomes loud.
    assert!(
        stdout.contains("sandbox:"),
        "--version must include sandbox state for bug-report aid; got: {stdout}"
    );
    // The sandbox status is one of {available, UNAVAILABLE} and the
    // backend identifier is in brackets (e.g. `[seatbelt]`, `[bwrap]`,
    // `[none]`). Don't pin a specific backend — CI runs on
    // multiple platforms.
    assert!(
        stdout.contains("available") || stdout.contains("UNAVAILABLE"),
        "--version sandbox state must say available or UNAVAILABLE; got: {stdout}"
    );
    assert!(
        stdout.contains('[') && stdout.contains(']'),
        "--version must include backend identifier in brackets; got: {stdout}"
    );
}

#[test]
fn test_cli_help() {
    let output = Command::new(koda_bin())
        .arg("--help")
        .output()
        .expect("Failed to run koda --help");

    assert!(output.status.success(), "koda --help should succeed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--agent"), "Help should mention --agent");
    assert!(stdout.contains("--resume"), "Help should mention --resume");
    assert!(
        stdout.contains("--provider"),
        "Help should mention --provider"
    );
    assert!(stdout.contains("--model"), "Help should mention --model");
}

#[test]
fn test_cli_invalid_flag() {
    let output = Command::new(koda_bin())
        .arg("--nonexistent-flag")
        .output()
        .expect("Failed to run koda with invalid flag");

    assert!(
        !output.status.success(),
        "Invalid flag should exit with error"
    );
}

#[test]
fn test_cli_help_mentions_exec() {
    let output = Command::new(koda_bin())
        .arg("--help")
        .output()
        .expect("Failed to run koda --help");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("koda exec"),
        "Help should point one-shot runs at `koda exec`: {stdout}"
    );
    assert!(
        !stdout.contains("--output-format") && !stdout.contains("--prompt"),
        "Help should not list the removed -p flags: {stdout}"
    );
}

#[test]
fn test_cli_headless_piped_stdin_empty() {
    // Piping empty stdin should not hang — should detect empty and still work
    let output = Command::new(koda_bin())
        .arg("--help") // Just verify the binary handles stdin detection
        .stdin(std::process::Stdio::null())
        .output()
        .expect("Failed to run koda with null stdin");
    assert!(output.status.success());
}

/// Snapshot test — `koda --help` must match the committed snapshot.
///
/// Fails when any CLI flag is added, removed, renamed, or its description
/// edited. To update: run `cargo run -p koda-cli -- --help > \
/// koda-cli/tests/snapshots/help.snap`, then edit `docs/src/cli-reference.md`.
#[test]
fn test_cli_help_snapshot() {
    let output = Command::new(koda_bin())
        .arg("--help")
        .output()
        .expect("Failed to run koda --help");

    assert!(output.status.success());
    let actual = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    let snapshot = include_str!("snapshots/help.snap").replace("\r\n", "\n");
    assert_eq!(
        actual.trim(),
        snapshot.trim(),
        concat!(
            "\n\nkoda --help output changed — update snapshot + docs:\n",
            "  cargo run -p koda-cli -- --help ",
            "> koda-cli/tests/snapshots/help.snap\n",
            "  # then edit docs/src/cli-reference.md\n"
        )
    );
}

/// `koda server` used to start the ACP server. It has to fail with a reason:
/// a plain `server` would otherwise be read as a prompt.
#[test]
fn the_removed_server_command_says_so() {
    for args in [
        &["server"][..],
        &["server", "--stdio"],
        &["server", "--port", "9999"],
    ] {
        let output = Command::new(koda_bin())
            .args(args)
            .output()
            .expect("Failed to run koda");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{args:?}");
        assert!(stderr.contains("has been removed"), "{args:?}: {stderr}");
        assert!(stderr.contains("acp.html"), "{args:?}: {stderr}");
    }
}

/// A bad context-window pin is an error at parse time, from the flag or the
/// env var, never a silent fall-back to the server's answer or the table.
#[test]
fn test_cli_max_context_tokens_rejects_zero_and_non_numbers() {
    for bad in ["0", "abc", "64k"] {
        let flag = Command::new(koda_bin())
            .args(["--max-context-tokens", bad, "exec", "test"])
            .output()
            .expect("Failed to run koda");
        assert!(!flag.status.success(), "--max-context-tokens {bad}");
        let stderr = String::from_utf8_lossy(&flag.stderr);
        assert!(
            stderr.contains("--max-context-tokens") && stderr.contains("context window"),
            "flag {bad}: {stderr}"
        );

        let env = Command::new(koda_bin())
            .env("KODA_MAX_CONTEXT_TOKENS", bad)
            .args(["exec", "test"])
            .output()
            .expect("Failed to run koda");
        assert!(!env.status.success(), "KODA_MAX_CONTEXT_TOKENS={bad}");
        let stderr = String::from_utf8_lossy(&env.stderr);
        assert!(
            stderr.contains("--max-context-tokens") && stderr.contains("context window"),
            "env {bad}: {stderr}"
        );
    }
}

/// An effort level or thinking budget typed for a Claude model that doesn't
/// take it is refused before any request, with the levels it does take.
#[test]
fn test_cli_refuses_effort_a_claude_model_does_not_take() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let run = |extra: &[&str]| {
        Command::new(koda_bin())
            .args([
                "exec",
                "test",
                "--provider",
                "anthropic",
                "--model",
                "claude-haiku-5-5",
            ])
            .args(["--base-url", "http://127.0.0.1:9"])
            .args(extra)
            .arg("--project-root")
            .arg(&project)
            .env("HOME", tmp.path())
            .env("XDG_CONFIG_HOME", tmp.path())
            .env_remove("ANTHROPIC_API_KEY")
            .output()
            .expect("Failed to run koda")
    };

    let effort = run(&["--effort", "ultra"]);
    assert!(!effort.status.success());
    let stderr = String::from_utf8_lossy(&effort.stderr);
    assert!(
        stderr.contains("claude-haiku-5-5 doesn't take effort \"ultra\"")
            && stderr.contains("low, medium, high, xhigh, max"),
        "{stderr}"
    );

    let from_env = Command::new(koda_bin())
        .args([
            "exec",
            "test",
            "--provider",
            "anthropic",
            "--model",
            "claude-haiku-5-5",
        ])
        .args(["--base-url", "http://127.0.0.1:9"])
        .arg("--project-root")
        .arg(&project)
        .env("HOME", tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .env("KODA_REASONING_EFFORT", "ultra")
        .env_remove("ANTHROPIC_API_KEY")
        .output()
        .expect("Failed to run koda");
    assert!(!from_env.status.success());
    let stderr = String::from_utf8_lossy(&from_env.stderr);
    assert!(stderr.contains("doesn't take effort \"ultra\""), "{stderr}");

    let budget = run(&["--thinking-budget", "8000"]);
    assert!(!budget.status.success());
    let stderr = String::from_utf8_lossy(&budget.stderr);
    assert!(stderr.contains("takes no --thinking-budget"), "{stderr}");
}

/// `--resume` with an id no session has stops at startup. It used to run a
/// session that didn't exist: the prompt couldn't be saved, and the model
/// was sent no messages (Anthropic answered 400, which read like a key
/// problem). Nothing listens on the base URL, so a model call would fail
/// differently.
#[test]
fn resume_with_an_unknown_id_stops_before_running() {
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(koda_bin())
        .args(["--resume", "28545", "exec", "hi"])
        .args([
            "--provider",
            "lmstudio",
            "--base-url",
            "http://127.0.0.1:9/v1",
        ])
        .args(["--model", "stub-model"])
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .output()
        .expect("failed to run koda");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no session matches '28545'"), "{stderr}");
}
