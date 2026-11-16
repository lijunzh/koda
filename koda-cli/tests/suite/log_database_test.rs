//! A run's log goes to the log database, filed under its session, and
//! `koda-logs` reads it back; no log file is written unless `log_dir` asks
//! for one. Runs the `koda` binary against a local stub. No API key, no
//! model.

use koda_test_utils::network::{FakeLlmServer, ResponseTemplate};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A binary Cargo built for these tests. Cargo says where: since 1.100 a
/// test binary no longer sits beside them.
fn bin(name: &str) -> PathBuf {
    PathBuf::from(match name {
        "koda" => env!("CARGO_BIN_EXE_koda"),
        "koda-logs" => env!("CARGO_BIN_EXE_koda-logs"),
        other => panic!("no binary {other}"),
    })
}

fn reply() -> ResponseTemplate {
    let chunk = json!({"choices": [{"delta": {"content": "Done."}, "finish_reason": "stop"}]});
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(format!("data: {chunk}\n\ndata: [DONE]\n\n"))
}

/// Run `koda exec` in `home`; return its session id.
async fn run_koda(server: &FakeLlmServer, home: &Path, extra: &[&str]) -> String {
    let output = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new(bin("koda"))
            .args(["exec", "Say done", "--json"])
            .args(["--provider", "lmstudio", "--model", "stub"])
            .args(["--base-url", &server.url()])
            .args(extra)
            .arg("--project-root")
            .arg(home)
            .env("XDG_CONFIG_HOME", home)
            .env_remove("RUST_LOG")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("koda hung")
    .expect("failed to run koda");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let summary: Value = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|line| line["method"] == "exec/summary")
        .expect("an exec/summary line");
    summary["params"]["sessionId"].as_str().unwrap().to_string()
}

/// `koda-logs` with `args` against the database under `home`.
async fn koda_logs(home: &Path, args: &[&str]) -> String {
    let output = tokio::process::Command::new(bin("koda-logs"))
        .args(args)
        .env("XDG_CONFIG_HOME", home)
        .output()
        .await
        .expect("failed to run koda-logs");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_is_logged_under_its_session_and_read_back() {
    let server = FakeLlmServer::spawn().await;
    server
        .mount_fn("POST", r".*/chat/completions$", |_| reply())
        .await;
    let home = tempfile::tempdir().unwrap();
    let first = run_koda(&server, home.path(), &[]).await;
    let second = run_koda(&server, home.path(), &[]).await;

    // No file without `log_dir`; the database exists.
    assert!(!home.path().join("koda/logs").exists());
    assert!(home.path().join("koda/db/logs_1.db").exists());

    let mine = koda_logs(home.path(), &["--session", &first]).await;
    assert!(mine.contains("model call"), "{mine}");
    assert!(mine.contains(&format!("session_id={first}")), "{mine}");
    assert!(!mine.contains(&second), "another session's lines: {mine}");
    // Startup is logged outside any session.
    assert!(!mine.contains("Koda starting"), "{mine}");
    let all = koda_logs(home.path(), &[]).await;
    assert!(all.contains("Koda starting"), "{all}");
    assert!(all.contains(&second), "{all}");

    let warnings = koda_logs(home.path(), &["--session", &first, "--level", "error"]).await;
    assert!(!warnings.contains("model call"), "{warnings}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn log_dir_also_writes_a_file() {
    let server = FakeLlmServer::spawn().await;
    server
        .mount_fn("POST", r".*/chat/completions$", |_| reply())
        .await;
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("file-log");
    let log_dir = format!("log_dir={}", dir.display());
    run_koda(&server, home.path(), &["-c", &log_dir]).await;
    run_koda(&server, home.path(), &["-c", &log_dir]).await;

    let file = std::fs::read_to_string(dir.join("koda.log")).unwrap();
    // Appended to, run after run; every Koda crate at info.
    assert_eq!(file.matches("Koda starting").count(), 2, "{file}");
    assert!(
        file.contains("koda_server"),
        "the host's lines are kept: {file}"
    );
}
