//! Stopping a server the model started with `&`, through a real headless run
//! in Auto mode against a local stub. Headless refuses anything that asks, so
//! any approval request shows up in the stream and fails the test. No API
//! key, no model.
//!
//! The server is a `sleep` started by `sleep N & echo PID=$!` in an ordinary
//! Bash call. The call's shell exits at once, so Koda takes over the group it
//! leaves running (as Codex owns an exec session's group). Then:
//! 1. a `kill` chained with other commands gets Koda's hint, not a prompt:
//!    the sandbox can't signal the process from inside a longer command;
//! 2. a plain `kill PID` of the server, which isn't the group's leader, is
//!    Koda's own and runs without asking.
//!
//! macOS only: on Linux the sandbox gives each command its own PID namespace
//! (`bwrap --unshare-pid`), so `echo $!` prints a PID from inside it (CI saw
//! 4), not the host PID Koda tracks, and a sandboxed command can't reach a
//! Koda-started server's port either.
#![cfg(target_os = "macos")]

use koda_test_utils::network::{FakeLlmServer, ResponseTemplate};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

fn koda_bin() -> String {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // test binary name
    path.pop(); // deps/
    path.push("koda");
    path.to_string_lossy().to_string()
}

fn sse(chunks: &[Value]) -> ResponseTemplate {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

fn bash(id: &str, command: &str) -> ResponseTemplate {
    sse(&[
        json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0, "id": id, "type": "function",
            "function": {"name": "Bash", "arguments": json!({"command": command}).to_string()},
        }]}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ])
}

fn text(reply: &str) -> ResponseTemplate {
    sse(&[
        json!({"choices": [{"delta": {"content": reply}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
    ])
}

/// The PID the first call printed as `PID=<n>`.
fn pid_in(body: &str) -> u32 {
    // The last one: the system prompt may mention PIDs too.
    body.rsplit("PID=")
        .next()
        .filter(|_| body.contains("PID="))
        .and_then(|rest| {
            rest.split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|n| n.parse().ok())
        })
        .unwrap_or_else(|| panic!("no PID= in the request: {body}"))
}

fn alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|s| s.success())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_started_with_amp_is_stopped_koda_s_way_without_asking() {
    let server = FakeLlmServer::spawn().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let next = Arc::clone(&calls);
    let pid = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&pid);
    server
        .mount_fn("POST", r".*/chat/completions$", move |request| {
            let body = String::from_utf8_lossy(&request.body).to_string();
            match next.fetch_add(1, Ordering::SeqCst) {
                // 34 s: no other test sleeps that long.
                0 => bash("c1", "sleep 34 > /dev/null 2>&1 & echo PID=$!"),
                1 => {
                    let p = pid_in(&body);
                    seen.store(p as usize, Ordering::SeqCst);
                    bash(
                        "c2",
                        &format!("kill {p}; sleep 0.2; kill -0 {p} && echo up || echo down"),
                    )
                }
                2 => bash("c3", &format!("kill {}", seen.load(Ordering::SeqCst))),
                _ => text("Stopped."),
            }
        })
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let child = tokio::process::Command::new(koda_bin())
        .args([
            "-p",
            "start it, then stop it",
            "--output-format",
            "stream-json",
        ])
        .args([
            "--mode",
            "auto",
            "--provider",
            "lmstudio",
            "--model",
            "stub",
        ])
        .args(["--base-url", &server.url()])
        .arg("--project-root")
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(Duration::from_secs(120), child)
        .await
        .expect("koda hung")
        .expect("failed to run koda");
    let stdout = String::from_utf8_lossy(&output.stdout);

    let events: Vec<Value> = stdout
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let asked: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"] == "approval_request")
        .collect();
    assert!(asked.is_empty(), "Koda asked: {asked:?}");
    let result = |id: &str| -> String {
        events
            .iter()
            .find(|e| e["type"] == "tool_call_result" && e["id"] == id)
            .and_then(|e| e["output"].as_str())
            .unwrap_or_else(|| panic!("no result for {id}: {stdout}"))
            .to_string()
    };
    assert!(
        result("c1").contains("left processes running"),
        "{}",
        result("c1")
    );
    let hint = result("c2");
    assert!(hint.starts_with("Not run:"), "{hint}");
    let p = pid.load(Ordering::SeqCst);
    assert!(hint.contains(&format!("kill {p}")), "{hint}");
    assert!(
        result("c3").contains("Stopped background process"),
        "{}",
        result("c3")
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(!alive(p as u32), "the server is still running");
}
