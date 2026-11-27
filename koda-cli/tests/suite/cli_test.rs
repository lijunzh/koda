//! CLI binary integration tests.
//!
//! Tests that the compiled binary handles command-line arguments correctly.
//! These run the actual `koda` binary as a subprocess.

use std::process::Command;

/// Get the path to the built binary.
fn koda_bin() -> String {
    env!("CARGO_BIN_EXE_koda").to_string()
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
    // Only the version, as `codex --version` prints: the sandbox state is
    // the host's, shown in the status bar, `/status` and `/feedback`.
    assert!(!stdout.contains("sandbox"), "{stdout}");
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

/// Snapshot test — `koda --help` must match the committed snapshot.
///
/// Fails when any CLI flag is added, removed, renamed, or its description
/// edited. To update: run `COLUMNS=100 cargo run -p koda-cli --bin koda --
/// --help > koda-cli/tests/snapshots/help.snap`, then edit
/// `docs/src/cli-reference.md`. Help wraps to the terminal, at most 100
/// columns, so the snapshot is taken at 100.
#[test]
fn test_cli_help_snapshot() {
    let output = Command::new(koda_bin())
        .arg("--help")
        .env("COLUMNS", "100")
        .output()
        .expect("Failed to run koda --help");

    assert!(output.status.success());
    let actual = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    let snapshot = include_str!("../snapshots/help.snap").replace("\r\n", "\n");
    assert_eq!(
        actual.trim(),
        snapshot.trim(),
        concat!(
            "\n\nkoda --help output changed — update snapshot + docs:\n",
            "  COLUMNS=100 cargo run -p koda-cli --bin koda -- --help ",
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
        assert!(stderr.contains("acp-is-gone"), "{args:?}: {stderr}");
    }
}

/// A bad context-window pin is an error at parse time, never a silent
/// fall-back to the server's answer or the table.
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

    let budget = run(&["-c", "model_thinking_budget=8000"]);
    assert!(!budget.status.success());
    let stderr = String::from_utf8_lossy(&budget.stderr);
    assert!(
        stderr.contains("takes no model_thinking_budget"),
        "{stderr}"
    );
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

/// `--provider` takes a provider's id or its name as `/provider` lists it;
/// one Koda doesn't offer stops the run with the ids, rather than running
/// OpenAI as an unknown name once did.
#[test]
fn an_unknown_provider_stops_the_run_with_the_list() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let output = Command::new(koda_bin())
        .args(["exec", "test", "--provider", "nope"])
        .arg("--project-root")
        .arg(&project)
        .env("HOME", tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .output()
        .expect("Failed to run koda");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown provider \"nope\""), "{stderr}");
    assert!(
        stderr.contains("anthropic") && stderr.contains("lm-studio"),
        "{stderr}"
    );
}

/// Every help page reads as a reference on its own: no line wider than 100
/// columns on a wide terminal, and no comparisons with other tools.
#[test]
fn help_pages_fit_100_columns_and_stand_alone() {
    for args in [
        vec![],
        vec!["exec"],
        vec!["exec", "resume"],
        vec!["exec", "fork"],
        vec!["resume"],
        vec!["fork"],
        vec!["serve"],
        vec!["mcp"],
        vec!["mcp", "add"],
    ] {
        for flag in ["-h", "--help"] {
            let output = Command::new(koda_bin())
                .args(&args)
                .arg(flag)
                .env("COLUMNS", "200")
                .output()
                .expect("Failed to run koda");
            assert!(output.status.success(), "koda {args:?} {flag}");
            let text = String::from_utf8_lossy(&output.stdout);
            for line in text.lines() {
                assert!(
                    line.chars().count() <= 100,
                    "koda {args:?} {flag}: a line over 100 columns: {line}"
                );
            }
            let lower = text.to_lowercase();
            for other in ["codex", "claude code", "claude's"] {
                assert!(
                    !lower.contains(other),
                    "koda {args:?} {flag} mentions {other:?}:\n{text}"
                );
            }
        }
    }
}

/// `-m` is `--model` and `-C` is `--cd`, which `--project-root` still names.
#[test]
fn short_flags_for_the_model_and_the_directory() {
    let help = Command::new(koda_bin()).arg("-h").output().unwrap();
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("-m, --model"), "{help}");
    assert!(help.contains("-C, --cd <DIR>"), "{help}");
    assert!(
        help.contains("--project-root"),
        "the old name is listed: {help}"
    );
}
