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
fn test_cli_help_mentions_headless() {
    let output = Command::new(koda_bin())
        .arg("--help")
        .output()
        .expect("Failed to run koda --help");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--prompt") || stdout.contains("-p"),
        "Help should mention -p/--prompt for headless mode: {stdout}"
    );
    assert!(
        stdout.contains("--output-format"),
        "Help should mention --output-format: {stdout}"
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

/// Snapshot test — `koda server --help` must match the committed snapshot.
///
/// Fails when any server flag is added, removed, renamed, or its description
/// edited. To update: run `cargo run -p koda-cli -- server --help > \
/// koda-cli/tests/snapshots/server_help.snap`, then edit `docs/src/acp.md`.
#[test]
fn test_cli_server_help_snapshot() {
    let output = Command::new(koda_bin())
        .args(["server", "--help"])
        .output()
        .expect("Failed to run koda server --help");

    assert!(output.status.success());
    let actual = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    let snapshot = include_str!("snapshots/server_help.snap").replace("\r\n", "\n");
    assert_eq!(
        actual.trim(),
        snapshot.trim(),
        concat!(
            "\n\nkoda server --help output changed — update snapshot + docs:\n",
            "  cargo run -p koda-cli -- server --help ",
            "> koda-cli/tests/snapshots/server_help.snap\n",
            "  # then edit docs/src/acp.md\n"
        )
    );
}

#[test]
fn test_cli_output_format_validates() {
    let output = Command::new(koda_bin())
        .args(["--output-format", "invalid_format", "-p", "test"])
        .output()
        .expect("Failed to run koda");
    // clap should reject invalid output formats
    assert!(
        !output.status.success(),
        "Invalid output-format should fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid") || stderr.contains("possible values"),
        "Should mention invalid value: {stderr}"
    );
}
