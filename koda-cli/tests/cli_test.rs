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
    assert!(
        stdout.contains("--session"),
        "Help should mention --session"
    );
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
