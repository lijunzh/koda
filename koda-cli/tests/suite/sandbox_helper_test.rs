//! `koda` runs the Linux sandbox's stage 2 itself, as Codex's binary runs
//! its sandbox helpers, so a sandboxed command gets its own network however
//! Koda was installed. The release archives and `cargo install koda-cli`
//! carry only `koda`; before this, an installed Koda found no
//! `koda-sandbox-stage2` next to it and fell back to proxy variables only,
//! so a command that ignores them reached any host loopback server.
//!
//! Linux only: stage 2 is part of the bwrap sandbox. No API key, no model:
//! the model is a local stub.
#![cfg(target_os = "linux")]

use koda_test_utils::network::{FakeLlmServer, ResponseTemplate};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

fn koda_bin() -> &'static str {
    env!("CARGO_BIN_EXE_koda")
}

fn have(tool: &str, arg: &str) -> bool {
    std::process::Command::new(tool)
        .arg(arg)
        .output()
        .is_ok_and(|o| o.status.success())
}

/// bwrap that can make a sandbox here, not just installed.
fn bwrap_works() -> bool {
    std::process::Command::new("bwrap")
        .args(["--ro-bind", "/", "/", "--unshare-net", "--", "true"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn sse(chunks: &[Value]) -> ResponseTemplate {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

/// `koda <marker> <command>` is stage 2, not an unknown flag: without the
/// bridge it is given inside a sandbox, it fails setup with stage 2's own
/// error and exit code.
#[test]
fn koda_runs_stage_2_when_given_its_marker() {
    let output = std::process::Command::new(koda_bin())
        .args([koda_sandbox::stage2::STAGE2_ARG1, "true"])
        .env_remove(koda_sandbox::bwrap_proxy::STAGE2_UDS_ENV_KEY)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(koda_sandbox::stage2::STAGE2_SETUP_FAILED_EXIT),
        "{stderr}"
    );
    assert!(stderr.starts_with("koda-sandbox-stage2:"), "{stderr}");
}

/// A `koda` alone in its folder, as installed, still gives a sandboxed
/// command its own network: a `curl` that ignores the proxy can't reach a
/// host server Koda didn't start.
#[tokio::test(flavor = "multi_thread")]
async fn an_installed_koda_keeps_a_command_off_the_host_s_loopback() {
    if !bwrap_works() || !have("curl", "--version") {
        eprintln!("skip: needs a working bwrap and curl");
        return;
    }
    // A host server Koda didn't start, answering 200 to anyone who reaches it.
    let theirs = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let their_port = theirs.local_addr().unwrap().port();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        for mut conn in theirs.incoming().flatten() {
            let _ = conn.read(&mut [0u8; 1024]);
            let _ = conn.write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n");
        }
    });
    let reach = format!(
        "curl -s --max-time 5 --noproxy '*' -o /dev/null -w 'code=%{{http_code}}' \
         http://127.0.0.1:{their_port}/; echo; readlink /proc/self/ns/net"
    );

    let server = FakeLlmServer::spawn().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let next = Arc::clone(&calls);
    server
        .mount_fn("POST", r".*/chat/completions$", move |_| {
            match next.fetch_add(1, Ordering::SeqCst) {
                0 => sse(&[
                    json!({"choices": [{"delta": {"tool_calls": [{
                        "index": 0, "id": "c1", "type": "function",
                        "function": {"name": "Bash", "arguments": json!({"command": reach}).to_string()},
                    }]}}]}),
                    json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
                ]),
                _ => sse(&[
                    json!({"choices": [{"delta": {"content": "Done."}}]}),
                    json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
                ]),
            }
        })
        .await;

    // `koda` alone, with no helper binary beside it.
    let install = tempfile::tempdir().unwrap();
    let koda = install.path().join("koda");
    std::fs::copy(koda_bin(), &koda).unwrap();
    let project = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let child = tokio::process::Command::new(&koda)
        .args(["exec", "reach it", "--json"])
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
        .arg(project.path())
        .env("XDG_CONFIG_HOME", config.path())
        .env_remove(koda_sandbox::bwrap::STAGE2_BIN_ENV_KEY)
        .env_remove("CARGO_BIN_EXE_koda-sandbox-stage2")
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(Duration::from_secs(120), child)
        .await
        .expect("koda hung")
        .expect("failed to run koda");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let result = stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|e| e["method"] == "item/completed" && e["params"]["item"]["id"] == "c1")
        .and_then(|e| e["params"]["item"]["output"].as_str().map(str::to_string))
        .unwrap_or_else(|| {
            panic!(
                "no result for the command: {stdout}\nstderr: {}",
                String::from_utf8_lossy(&output.stderr)
            )
        });

    assert!(
        result.contains("code=000"),
        "reached the host server: {result}"
    );
    let host_net = std::fs::read_link("/proc/self/ns/net").unwrap();
    assert!(
        !result.contains(&*host_net.to_string_lossy()),
        "the command shared the host's network: {result}"
    );
}
