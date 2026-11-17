//! Typing into a background command: `Bash` with `tty: true` gives it a
//! terminal in the sandbox, and `WriteStdin` types into it and reads back.

use serde_json::json;

fn registry(root: &std::path::Path) -> koda_core::tools::ToolRegistry {
    koda_core::tools::ToolRegistry::new(root.to_path_buf(), 100_000)
}

fn pid_of(start: &str) -> u64 {
    start
        .lines()
        .find_map(|line| line.trim().strip_prefix("PID:"))
        .and_then(|pid| pid.trim().parse().ok())
        .unwrap_or_else(|| panic!("no PID in {start}"))
}

/// A prompt that waits for a typed answer, the case the background start
/// with closed stdin couldn't handle: it sees a terminal, shows its prompt
/// in the start's result, and answers what is typed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_prompt_is_answered_by_typing() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = registry(tmp.path());
    let start = registry
        .execute(
            "Bash",
            &json!({"command": "[ -t 0 ] && printf 'Proceed? [y/N] '; read a; \
                     [ \"$a\" = y ] && echo PROCEEDED > done.txt && echo PROCEEDED",
                    "tty": true})
            .to_string(),
            None,
            None,
        )
        .await;
    assert!(start.success, "{}", start.output);
    assert!(start.output.contains("Proceed? [y/N]"), "{}", start.output);
    assert!(start.output.contains("[still running]"), "{}", start.output);
    let pid = pid_of(&start.output);

    let reply = registry
        .execute(
            "WriteStdin",
            &json!({"pid": pid, "chars": "y\n", "yield_time_ms": 10_000}).to_string(),
            None,
            None,
        )
        .await;
    assert!(reply.success, "{}", reply.output);
    assert!(reply.output.contains("PROCEEDED"), "{}", reply.output);
    assert!(
        reply.output.contains("[exited with code 0]"),
        "{}",
        reply.output
    );
    // It ran in the project, inside the sandbox's writable area.
    assert!(tmp.path().join("done.txt").exists());

    let late = registry
        .execute(
            "WriteStdin",
            &json!({"pid": pid, "chars": "more\n"}).to_string(),
            None,
            None,
        )
        .await;
    assert!(
        late.output.contains("has exited, so nothing was typed"),
        "{}",
        late.output
    );
}

/// A typed Ctrl+C stops the command, as it would in a terminal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctrl_c_stops_the_command() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = registry(tmp.path());
    let start = registry
        .execute(
            "Bash",
            &json!({"command": "echo READY; sleep 60", "tty": true}).to_string(),
            None,
            None,
        )
        .await;
    assert!(start.output.contains("READY"), "{}", start.output);
    let reply = registry
        .execute(
            "WriteStdin",
            &json!({"pid": pid_of(&start.output), "chars": "\u{3}", "yield_time_ms": 10_000})
                .to_string(),
            None,
            None,
        )
        .await;
    assert!(reply.output.contains("[exited"), "{}", reply.output);
}

/// A background command started without a terminal can't be typed into,
/// and the error says how to start one that can.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_without_a_terminal_says_how_to_get_one() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = registry(tmp.path());
    let start = registry
        .execute(
            "Bash",
            &json!({"command": "sleep 60", "background": true}).to_string(),
            None,
            None,
        )
        .await;
    let reply = registry
        .execute(
            "WriteStdin",
            &json!({"pid": pid_of(&start.output), "chars": "x"}).to_string(),
            None,
            None,
        )
        .await;
    assert!(!reply.success);
    assert!(reply.output.contains("Bash{tty: true}"), "{}", reply.output);
    let unknown = registry
        .execute("WriteStdin", &json!({"pid": 1}).to_string(), None, None)
        .await;
    assert!(
        unknown.output.contains("no background process has PID 1"),
        "{}",
        unknown.output
    );
}

/// A terminal outside the sandbox is refused: typing would do anything there
/// on an approval given for one command.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_terminal_outside_the_sandbox() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = registry(tmp.path());
    let start = registry
        .execute(
            "Bash",
            &json!({"command": "sh", "tty": true, "outside_sandbox": true, "reason": "x"})
                .to_string(),
            None,
            None,
        )
        .await;
    assert!(!start.success);
    assert!(
        start.output.contains("can't be used together"),
        "{}",
        start.output
    );
}

/// The sandbox opens the command's own terminal fully: its settings
/// (`stty`, the ioctls a REPL makes) and `/dev/tty`, which password-style
/// prompts open. Without that, macOS's sandbox refused both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sandbox_opens_the_command_s_own_terminal() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = registry(tmp.path());
    let start = registry
        .execute(
            "Bash",
            &json!({"command": "stty size; printf 'secret? ' > /dev/tty; read a < /dev/tty; \
                                echo \"got $a\"",
                    "tty": true})
            .to_string(),
            None,
            None,
        )
        .await;
    assert!(start.output.contains("24 80"), "{}", start.output);
    assert!(start.output.contains("secret? "), "{}", start.output);
    let reply = registry
        .execute(
            "WriteStdin",
            &json!({"pid": pid_of(&start.output), "chars": "s3\n", "yield_time_ms": 10_000})
                .to_string(),
            None,
            None,
        )
        .await;
    assert!(reply.output.contains("got s3"), "{}", reply.output);
}
