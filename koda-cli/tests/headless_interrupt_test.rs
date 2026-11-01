//! SIGINT during a model call, in a real headless run.
//!
//! The eval harness stops a run that hits its time limit this way, so what
//! koda reports here is all that survives of that run: the turn end, the run
//! summary and the log. No API key, no model.
#![cfg(unix)]

use koda_test_utils::network::{FakeLlmServer, ResponseTemplate};
use serde_json::Value;
use std::process::Stdio;
use std::time::Duration;

fn koda_bin() -> String {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // test binary name
    path.pop(); // deps/
    path.push("koda");
    path.to_string_lossy().to_string()
}

/// Everything koda logged for a run whose config dir was `config`.
fn koda_log(config: &std::path::Path) -> String {
    std::fs::read_dir(config.join("koda/logs"))
        .unwrap()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("koda-"))
        .map(|entry| std::fs::read_to_string(entry.path()).unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn sigint_during_a_model_call_still_reports_the_turn() {
    let server = FakeLlmServer::spawn().await;
    server
        .mount_fn("POST", r".*/chat/completions$", |_| {
            ResponseTemplate::new(200).set_delay(Duration::from_secs(120))
        })
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let child = tokio::process::Command::new(koda_bin())
        .args(["-p", "Fix the parser", "--output-format", "stream-json"])
        .args(["--provider", "lmstudio", "--model", "stub"])
        .args(["--base-url", &server.url()])
        .arg("--project-root")
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    // Signal once the request is in flight.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !has_chat_request(&server).await {
        assert!(tokio::time::Instant::now() < deadline, "no model call");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let pid = child.id().unwrap().to_string();
    let sent = std::process::Command::new("kill")
        .args(["-s", "INT", &pid])
        .status()
        .unwrap();
    assert!(sent.success());

    let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .expect("koda ignored SIGINT")
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let events: Vec<Value> = stdout
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let of_type = |kind: &str| events.iter().find(|e| e["type"] == kind);
    let turn_end = of_type("turn_end").unwrap_or_else(|| panic!("no turn_end: {stdout}"));
    assert_eq!(turn_end["reason"]["kind"], "cancelled", "{stdout}");
    let summary = of_type("run_summary").unwrap_or_else(|| panic!("no summary: {stdout}"));
    assert_eq!(summary["turn_end"]["kind"], "cancelled", "{stdout}");

    let log = koda_log(tmp.path());
    assert!(log.contains(r#"outcome="cancelled""#), "{log}");
}

async fn has_chat_request(server: &FakeLlmServer) -> bool {
    server
        .received_requests()
        .await
        .iter()
        .any(|request| !request.body.is_empty())
}
