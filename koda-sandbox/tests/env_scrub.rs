//! A command run by any of Koda's runtimes doesn't see the keys in Koda's
//! own environment (#1228): the sandbox's builders clear the environment and
//! add back only an allowlist. `env::scrub` has unit tests; this checks the
//! commands the runtimes really build, so dropping the call from a builder
//! fails here.
//!
//! The keys must be in the environment of the process that builds the
//! command, and a test may not change its own, so each case runs this test
//! binary again with them set: [`case`] builds the command and prints what
//! `env` showed it.
#![cfg(unix)]

use std::path::PathBuf;
use std::process::Command;

use koda_sandbox::{Loopback, SandboxPolicy, SandboxRuntime, SandboxTransformRequest};

const SECRETS: &[(&str, &str)] = &[
    ("OPENAI_API_KEY", "sk-env-scrub-test-openai"),
    ("ANTHROPIC_API_KEY", "sk-ant-env-scrub-test"),
    ("GITHUB_TOKEN", "ghp_env_scrub_test"),
    ("AWS_SECRET_ACCESS_KEY", "env-scrub-test-aws"),
];

fn available() -> bool {
    #[cfg(target_os = "linux")]
    return koda_sandbox::bwrap::is_available();
    #[cfg(not(target_os = "linux"))]
    true
}

/// What `env` printed under `runtime` ("sandbox" or "none"), with the keys
/// in the building process's environment.
fn env_under(runtime: &str) -> String {
    let project = tempfile::Builder::new()
        .prefix("env-scrub-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "case", "--ignored", "--nocapture"])
        .env("CASE_RUNTIME", runtime)
        .env("CASE_PROJECT", project.path());
    for (name, value) in SECRETS {
        command.env(name, value);
    }
    let output = command.output().unwrap();
    let text = String::from_utf8_lossy(&output.stdout).into_owned()
        + &String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("case ran"), "{text}");
    text
}

#[track_caller]
fn assert_scrubbed(runtime: &str) {
    let seen = env_under(runtime);
    for (name, value) in SECRETS {
        assert!(
            !seen.contains(value),
            "{runtime}: the command saw {name}:\n{seen}"
        );
    }
    // The allowlist still comes through: the command can find programs.
    assert!(seen.contains("PATH="), "{runtime}: no PATH:\n{seen}");
}

#[test]
fn a_sandboxed_command_sees_no_api_keys() {
    if !available() {
        koda_sandbox::skip_test("no working sandbox here");
        return;
    }
    assert_scrubbed("sandbox");
}

#[test]
fn a_command_on_a_machine_with_no_sandbox_sees_no_api_keys() {
    assert_scrubbed("none");
}

/// Run by [`env_under`]: build `env` with `CASE_RUNTIME` and run it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "run by the other tests"]
async fn case() {
    let project = PathBuf::from(std::env::var("CASE_PROJECT").unwrap());
    let policy = SandboxPolicy::strict_default();
    let request = SandboxTransformRequest {
        command: "env",
        project_root: &project,
        policy: &policy,
        proxy_port: None,
        loopback: &Loopback::default(),
    };
    let runtime: Box<dyn SandboxRuntime> = match std::env::var("CASE_RUNTIME").unwrap().as_str() {
        "none" => Box::new(koda_sandbox::UnsandboxedRuntime),
        _ => koda_sandbox::current_runtime(),
    };
    let mut command = runtime.transform(request).unwrap().command;
    let output = command.output().await.unwrap();
    println!(
        "{}{}case ran",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
