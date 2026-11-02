//! A run that stops early, by SIGINT or a failure, in a real headless run.
//!
//! The eval harness stops a run that hits its time limit this way, so what
//! koda reports here is all that survives of that run: the turn end, the run
//! summary and the log. No API key, no model.
#![cfg(unix)]

use koda_core::inference::CANCEL_GRACE;
use koda_test_utils::log::read_log;
use koda_test_utils::network::{FakeLlmServer, ResponseTemplate};
use serde_json::{Value, json};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn koda_bin() -> String {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // test binary name
    path.pop(); // deps/
    path.push("koda");
    path.to_string_lossy().to_string()
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

    // Signal once the chat request is in flight. Earlier requests are startup
    // probes, sent before koda listens for SIGINT, so the signal would kill it.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while chat_requests(&server).await == 0 {
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

    let log = read_log(tmp.path());
    assert!(log.contains(r#"outcome="cancelled""#), "{log}");
}

fn sse(chunks: &[Value]) -> ResponseTemplate {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

/// A streamed reply from the top-level agent that calls one tool.
fn stream_call(name: &str, args: Value) -> ResponseTemplate {
    let call = json!({
        "index": 0,
        "id": "call",
        "type": "function",
        "function": {"name": name, "arguments": args.to_string()},
    });
    sse(&[
        json!({"choices": [{"delta": {"tool_calls": [call]}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 100, "completion_tokens": 10}}),
    ])
}

/// A sub-agent's reply, which doesn't stream, that calls one tool.
fn child_call(name: &str, args: Value) -> ResponseTemplate {
    let call = json!({
        "id": "child",
        "type": "function",
        "function": {"name": name, "arguments": args.to_string()},
    });
    ResponseTemplate::new(200).set_body_json(json!({
        "choices": [{
            "message": {"role": "assistant", "tool_calls": [call]},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 30, "completion_tokens": 3},
    }))
}

fn never_answers() -> ResponseTemplate {
    ResponseTemplate::new(200).set_delay(Duration::from_secs(120))
}

/// A streamed reply of plain text, which ends the turn.
fn stream_text(reply: &str) -> ResponseTemplate {
    sse(&[
        json!({"choices": [{"delta": {"content": reply}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 200, "completion_tokens": 20}}),
    ])
}

/// The top-level agent delegates to the `task` agent, which can write, and
/// finishes once that returns. The sub-agent gets `first`, then `then`.
async fn stub_with_a_sub_agent(first: ResponseTemplate, then: ResponseTemplate) -> FakeLlmServer {
    let server = FakeLlmServer::spawn().await;
    let delegated = AtomicBool::new(false);
    server
        .mount_fn("POST", r".*/chat/completions$", move |request| {
            let request: Value = serde_json::from_slice(&request.body).unwrap();
            if request["stream"] == json!(true) {
                if delegated.swap(true, Ordering::SeqCst) {
                    return stream_text("Done.");
                }
                let task = json!({"agent_name": "task", "prompt": "do the work"});
                return stream_call("InvokeAgent", task);
            }
            let replied = request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["role"] == "tool");
            if replied { then.clone() } else { first.clone() }
        })
        .await;
    server
}

/// A project in a git repo, with `HOME` beside it, so a sub-agent's workspace
/// lands somewhere the test can see on every platform.
struct Project {
    tmp: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let project = Self { tmp };
        std::fs::create_dir_all(project.root()).unwrap();
        std::fs::create_dir_all(project.home()).unwrap();
        std::fs::write(project.root().join("a.txt"), "a").unwrap();
        for args in [
            &["init", "-q"][..],
            &["add", "."],
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "start",
            ],
        ] {
            let done = std::process::Command::new("git")
                .args(args)
                .current_dir(project.root())
                .output()
                .unwrap();
            assert!(done.status.success(), "{done:?}");
        }
        project
    }

    fn root(&self) -> std::path::PathBuf {
        self.tmp.path().join("project")
    }

    fn home(&self) -> std::path::PathBuf {
        self.tmp.path().join("home")
    }

    fn koda(&self, server: &FakeLlmServer) -> tokio::process::Child {
        tokio::process::Command::new(koda_bin())
            .args(["-p", "Fix the parser", "--output-format", "stream-json"])
            .args(["--provider", "lmstudio", "--model", "stub"])
            .args(["--base-url", &server.url()])
            .arg("--project-root")
            .arg(self.root())
            .env("XDG_CONFIG_HOME", self.home())
            .env("HOME", self.home())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    }

    /// Whether a file with this name exists anywhere under the temp dir.
    fn holds(&self, name: &str) -> bool {
        fn search(dir: &std::path::Path, name: &str) -> bool {
            std::fs::read_dir(dir).is_ok_and(|entries| {
                entries.flatten().any(|entry| {
                    let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
                    entry.file_name() != ".git"
                        && (entry.file_name() == name || is_dir && search(&entry.path(), name))
                })
            })
        }
        search(self.tmp.path(), name)
    }

    async fn wait_for(&self, name: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while !self.holds(name) {
            assert!(tokio::time::Instant::now() < deadline, "no {name}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// How a run ended after SIGINT, and how long it took to.
struct Interrupted {
    output: std::process::Output,
    took: Duration,
    stdout: String,
    events: Vec<Value>,
}

impl Interrupted {
    fn one(&self, kind: &str) -> &Value {
        let found: Vec<_> = self.events.iter().filter(|e| e["type"] == kind).collect();
        assert_eq!(found.len(), 1, "{kind}: {}", self.stdout);
        found[0]
    }

    fn summary(&self) -> &Value {
        let last = self.events.last().unwrap();
        assert_eq!(last["type"], "run_summary", "{}", self.stdout);
        last
    }
}

async fn interrupt(child: tokio::process::Child) -> Interrupted {
    let pid = child.id().unwrap().to_string();
    let signalled = Instant::now();
    let sent = std::process::Command::new("kill")
        .args(["-s", "INT", &pid])
        .status()
        .unwrap();
    assert!(sent.success());

    let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .expect("koda ignored SIGINT")
        .unwrap();
    let took = signalled.elapsed();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let events = stdout
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    Interrupted {
        output,
        took,
        stdout,
        events,
    }
}

/// The turn stopped on its own, in time, as a clean cancel.
fn assert_clean_cancel(run: &Interrupted) {
    assert!(run.took < CANCEL_GRACE, "took {:?}", run.took);
    assert_eq!(run.one("turn_end")["reason"]["kind"], "cancelled");
    assert_eq!(run.summary()["success"], true, "{}", run.stdout);
    assert_eq!(run.output.status.code(), Some(0), "{}", run.stdout);
}

/// The work reached git where the provider keeps it: a worktree commits it to
/// a branch, and a macOS clone is thrown away, which is that provider's design.
#[cfg(not(target_os = "macos"))]
fn assert_work_is_on_a_branch(project: &Project, name: &str) {
    let log = std::process::Command::new("git")
        .args(["log", "--all", "--format=%s", "--", name])
        .current_dir(project.root())
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&log.stdout);
    assert!(log.contains("koda: sub-agent 'task' changes"), "{log}");
}

#[cfg(target_os = "macos")]
fn assert_work_is_on_a_branch(_project: &Project, _name: &str) {}

#[tokio::test(flavor = "multi_thread")]
async fn a_sub_agent_waiting_on_its_model_releases_its_workspace() {
    let project = Project::new();
    let write = json!({"file_path": "work.txt", "content": "work"});
    let server = stub_with_a_sub_agent(child_call("Write", write), never_answers()).await;
    let child = project.koda(&server);

    // Parent, the child's Write, then the child call that hangs.
    project.wait_for("work.txt").await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while chat_requests(&server).await < 3 {
        assert!(tokio::time::Instant::now() < deadline, "child never hung");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let run = interrupt(child).await;
    assert!(!project.holds("work.txt"), "the workspace is still there");
    assert_work_is_on_a_branch(&project, "work.txt");
    assert_clean_cancel(&run);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sub_agent_running_a_command_releases_its_workspace() {
    let project = Project::new();
    let bash = json!({"command": "echo work > work.txt; sleep 60"});
    let server = stub_with_a_sub_agent(child_call("Bash", bash), never_answers()).await;
    let child = project.koda(&server);
    project.wait_for("work.txt").await;

    let run = interrupt(child).await;
    assert!(!project.holds("work.txt"), "the workspace is still there");
    assert_work_is_on_a_branch(&project, "work.txt");
    assert_clean_cancel(&run);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sub_agent_whose_model_fails_releases_its_workspace() {
    let project = Project::new();
    let write = json!({"file_path": "work.txt", "content": "work"});
    let failure = ResponseTemplate::new(500).set_body_string("boom");
    let server = stub_with_a_sub_agent(child_call("Write", write), failure).await;

    let child = project.koda(&server);
    let output = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .expect("koda hung")
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    // The top-level agent carried on after its sub-agent failed.
    assert!(stdout.contains(r#""response":"Done.""#), "{stdout}");
    assert!(!project.holds("work.txt"), "the workspace is still there");
    assert_work_is_on_a_branch(&project, "work.txt");
}

/// Nothing delivers the cancel to the top-level agent's Bash call, so this is
/// the limit at work.
#[tokio::test(flavor = "multi_thread")]
async fn a_turn_that_ignores_sigint_still_closes() {
    let project = Project::new();
    let server = FakeLlmServer::spawn().await;
    server
        .mount_fn("POST", r".*/chat/completions$", |_| {
            stream_call("Bash", json!({"command": "touch started; sleep 60"}))
        })
        .await;
    let child = project.koda(&server);
    project.wait_for("started").await;

    let run = interrupt(child).await;
    assert!(run.took >= CANCEL_GRACE, "stopped early: {:?}", run.took);
    assert_eq!(run.one("turn_end")["reason"]["kind"], "cancelled");

    // What finished before the signal still counts.
    let usage = &run.one("turn_summary")["usage"];
    assert_eq!(usage["api_calls"], 1, "{usage}");
    assert_eq!(usage["prompt_tokens"], 100, "{usage}");

    let summary = run.summary();
    assert_eq!(summary["success"], false, "{summary}");
    assert_eq!(summary["turn_end"]["kind"], "cancelled", "{summary}");
    assert!(summary["error"].is_string(), "{summary}");
    assert_eq!(run.output.status.code(), Some(1), "{}", run.stdout);

    let log = read_log(&project.home());
    assert!(log.contains(r#"outcome="abandoned""#), "{log}");
}

async fn chat_requests(server: &FakeLlmServer) -> usize {
    server
        .received_requests()
        .await
        .iter()
        .filter(|request| request.url.path().ends_with("/chat/completions"))
        .count()
}
