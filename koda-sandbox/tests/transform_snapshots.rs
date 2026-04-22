//! Phase 0 acceptance: snapshot tests on `transform()` output.
//!
//! These assert that `(cmd, policy) → SandboxExecRequest` produces a
//! deterministic, hash-stable spawn invocation. Insta snapshots make
//! drift visible: if a future PR accidentally changes the seatbelt
//! profile or bwrap arg vector, the diff shows up here loudly.

use koda_sandbox::{SandboxPolicy, SandboxRuntime, SandboxTransformRequest, UnsandboxedRuntime};
use std::path::Path;
use std::process::Command as StdCommand;

/// Extract `(program, args, cwd)` from a `tokio::Command` for snapshotting.
fn deconstruct(cmd: &StdCommand) -> (String, Vec<String>, Option<String>) {
    let program = cmd.get_program().to_string_lossy().to_string();
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().to_string())
        .collect();
    let cwd = cmd
        .get_current_dir()
        .map(|p| p.to_string_lossy().to_string());
    (program, args, cwd)
}

#[test]
fn unsandboxed_runtime_snapshot() {
    let policy = SandboxPolicy::default();
    let req = SandboxTransformRequest {
        command: "echo hi",
        project_root: Path::new("/tmp/snapshot-project"),
        policy: &policy,
        proxy_port: None,
    };
    let result = UnsandboxedRuntime.transform(req).unwrap();
    let (program, args, cwd) = deconstruct(result.command.as_std());

    insta::assert_yaml_snapshot!("unsandboxed_echo", (program, args, cwd));
}

// ── macOS Seatbelt snapshots ────────────────────────────────────────────────

#[cfg(target_os = "macos")]
#[test]
fn seatbelt_runtime_command_snapshot() {
    use koda_sandbox::SeatbeltRuntime;

    // Use a temp dir so canonicalize() can succeed deterministically.
    let dir = tempfile::tempdir().unwrap();
    let policy = SandboxPolicy::default();
    let req = SandboxTransformRequest {
        command: "true",
        project_root: dir.path(),
        policy: &policy,
        proxy_port: None,
    };
    let result = SeatbeltRuntime.transform(req).unwrap();
    let (program, args, _cwd) = deconstruct(result.command.as_std());

    // Profile string (args[1]) varies by $HOME and tempdir path; snapshot
    // only the structurally stable parts: program + arg count + the
    // command-shape tail (last 3 args = "sh" "-c" "true").
    let arg_count = args.len();
    let tail: Vec<String> = args.iter().rev().take(3).rev().cloned().collect();

    insta::assert_yaml_snapshot!("seatbelt_runtime_command_shape", (program, arg_count, tail));
}

#[cfg(target_os = "macos")]
#[test]
fn seatbelt_profile_contains_required_rules() {
    // A "behavioral snapshot": rather than locking the *exact* profile
    // string (sensitive to $HOME), assert the profile contains the rules
    // we know must be present. Catches accidental rule deletion in
    // future refactors without flaking on path differences.
    use koda_sandbox::SeatbeltRuntime;

    let dir = tempfile::tempdir().unwrap();
    let policy = SandboxPolicy::default();
    let req = SandboxTransformRequest {
        command: "true",
        project_root: dir.path(),
        policy: &policy,
        proxy_port: None,
    };
    let result = SeatbeltRuntime.transform(req).unwrap();
    let args: Vec<String> = result
        .command
        .as_std()
        .get_args()
        .map(|a| a.to_string_lossy().to_string())
        .collect();
    // sandbox-exec layout: ["-p", <profile>, "sh", "-c", <cmd>]
    let profile = &args[1];

    // Required allows.
    assert!(profile.contains("(version 1)"));
    assert!(profile.contains("(deny default)"));
    assert!(profile.contains("(allow file-read*)"));
    assert!(profile.contains("(allow network*)"));
    assert!(profile.contains("(allow process-exec*)"));
    assert!(profile.contains("(literal \"/dev/null\")"));
    assert!(profile.contains("(literal \"/dev/urandom\")"));

    // Required denies (#847 koda-db full deny).
    assert!(profile.contains("(deny file-read* file-write*"));
    assert!(profile.contains("koda/db"));

    // Protected project subdirs (#844 agent-write protection).
    assert!(profile.contains(".koda/agents"));
    assert!(profile.contains(".koda/skills"));
}

// ── Linux bwrap snapshots ───────────────────────────────────────────────────

#[cfg(target_os = "linux")]
#[test]
fn bwrap_runtime_command_snapshot() {
    use koda_sandbox::BwrapRuntime;

    // bwrap may not be installed — skip if unavailable. Snapshot tests
    // shouldn't fail just because the test host lacks the backend.
    if !koda_sandbox::bwrap::is_available() {
        eprintln!("bwrap not available; skipping snapshot test");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let policy = SandboxPolicy::default();
    let req = SandboxTransformRequest {
        command: "true",
        project_root: dir.path(),
        policy: &policy,
        proxy_port: None,
    };
    let result = BwrapRuntime.transform(req).unwrap();
    let (program, args, _cwd) = deconstruct(result.command.as_std());

    // Last 4 args are the terminator: "--" "sh" "-c" "true". Stable shape.
    // arg_count itself is *not* in the snapshot — it varies with the test
    // host (presence of /var/tmp, ~/.cargo, ~/.npm, ~/.cache shifts the
    // count by 3 each), and a brittle count just causes flaky CI without
    // catching real regressions. The first/last few args + the existing
    // `bwrap_command_starts_with_ro_bind_root` test are what actually
    // protects against accidental reordering.
    let tail: Vec<String> = args.iter().rev().take(4).rev().cloned().collect();

    insta::assert_yaml_snapshot!("bwrap_runtime_command_shape", (program, tail));
}

#[cfg(target_os = "linux")]
#[test]
fn bwrap_command_starts_with_ro_bind_root() {
    // Behavioral snapshot — the bwrap arg vector must start with the
    // root-fs read-only bind. Catches accidental reordering that would
    // break the "deny by exclusion" model (everything is read-only by
    // default; only project + caches get re-bound writable).
    use koda_sandbox::BwrapRuntime;

    if !koda_sandbox::bwrap::is_available() {
        eprintln!("bwrap not available; skipping behavioral test");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let policy = SandboxPolicy::default();
    let req = SandboxTransformRequest {
        command: "true",
        project_root: dir.path(),
        policy: &policy,
        proxy_port: None,
    };
    let result = BwrapRuntime.transform(req).unwrap();
    let args: Vec<String> = result
        .command
        .as_std()
        .get_args()
        .map(|a| a.to_string_lossy().to_string())
        .collect();

    assert_eq!(args[0], "--ro-bind", "first arg must be --ro-bind");
    assert_eq!(args[1], "/", "first ro-bind source must be /");
    assert_eq!(args[2], "/", "first ro-bind dest must be /");
}
