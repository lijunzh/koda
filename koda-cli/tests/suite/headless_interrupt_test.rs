//! A run that stops early, by SIGINT or a failure, in a real headless run.
//!
//! The eval harness stops a run that hits its time limit this way, so what
//! koda reports here is all that survives of that run: the turn end, the run
//! summary and the log. No API key, no model.
#![cfg(unix)]

use koda_test_utils::log::read_log;
use koda_test_utils::network::{FakeLlmServer, ResponseTemplate};
use koda_test_utils::openai::sse;
use serde_json::{Value, json};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// How long a step Koda should take may take. CI's macOS runners have
/// stalled every running test for 19 s and for 35 s (2026-11-18, 2026-11-26),
/// which a 30 s wait didn't outlast; nextest stops a test at 60 s.
const WAIT: Duration = Duration::from_secs(50);

fn koda_bin() -> String {
    env!("CARGO_BIN_EXE_koda").to_string()
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
        .args(["exec", "Fix the parser", "--json"])
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
    let deadline = tokio::time::Instant::now() + WAIT;
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

    let output = tokio::time::timeout(WAIT, child.wait_with_output())
        .await
        .expect("koda ignored SIGINT")
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let events: Vec<Value> = stdout
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let of = |method: &str| events.iter().find(|e| e["method"] == method);
    let turn = of("turn/completed").unwrap_or_else(|| panic!("no turn/completed: {stdout}"));
    assert_eq!(turn["params"]["turn"]["status"], "interrupted", "{stdout}");
    let summary = of("exec/summary").unwrap_or_else(|| panic!("no summary: {stdout}"));
    assert_eq!(summary["params"]["turnStatus"], "interrupted", "{stdout}");

    let log = read_log(tmp.path());
    assert!(log.contains(r#"outcome="cancelled""#), "{log}");
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
                let task = json!({"agent_name": "isolated", "prompt": "do the work"});
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

/// A project in a git repo, with `HOME` beside it, where the `isolated`
/// sub-agent asks for a worktree that a stopped turn has to give back.
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
        let agents = project.root().join(".koda/agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join("isolated.json"),
            json!({
                "name": "isolated",
                "system_prompt": "You are a worker.",
                "isolation": "worktree",
            })
            .to_string(),
        )
        .unwrap();
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
            .args(["exec", "Fix the parser", "--json"])
            .args(["--provider", "lmstudio", "--model", "stub"])
            .args(["--base-url", &server.url()])
            .args(["--mode", "auto"])
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
        let deadline = tokio::time::Instant::now() + WAIT;
        while !self.holds(name) {
            assert!(tokio::time::Instant::now() < deadline, "no {name}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// How a run ended after SIGINT.
struct Interrupted {
    output: std::process::Output,
    stdout: String,
    events: Vec<Value>,
}

impl Interrupted {
    fn one(&self, method: &str) -> &Value {
        let found: Vec<_> = self
            .events
            .iter()
            .filter(|e| e["method"] == method)
            .collect();
        assert_eq!(found.len(), 1, "{method}: {}", self.stdout);
        &found[0]["params"]
    }

    fn summary(&self) -> &Value {
        let last = self.events.last().unwrap();
        assert_eq!(last["method"], "exec/summary", "{}", self.stdout);
        &last["params"]
    }
}

async fn interrupt(child: tokio::process::Child) -> Interrupted {
    let pid = child.id().unwrap().to_string();
    let sent = std::process::Command::new("kill")
        .args(["-s", "INT", &pid])
        .status()
        .unwrap();
    assert!(sent.success());

    let output = tokio::time::timeout(WAIT, child.wait_with_output())
        .await
        .expect("koda ignored SIGINT")
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let events = stdout
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    Interrupted {
        output,
        stdout,
        events,
    }
}

/// The turn stopped on its own, as a clean cancel: the log says it was
/// cancelled, not dropped once the grace after a cancel ran out. How long
/// the run took isn't the evidence: on a busy machine a clean cancel, with
/// the worktree's commit and the exit, can take longer than the grace.
fn assert_clean_cancel(run: &Interrupted, project: &Project) {
    let log = read_log(&project.home());
    assert!(log.contains(r#"outcome="cancelled""#), "{log}");
    assert!(!log.contains(r#"outcome="abandoned""#), "{log}");
    assert_eq!(run.one("turn/completed")["turn"]["status"], "interrupted");
    assert_eq!(run.summary()["success"], true, "{}", run.stdout);
    assert_eq!(run.output.status.code(), Some(0), "{}", run.stdout);
}

/// The work reached the worktree's branch.
fn assert_work_is_on_a_branch(project: &Project, name: &str) {
    let log = std::process::Command::new("git")
        .args(["log", "--all", "--format=%s", "--", name])
        .current_dir(project.root())
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&log.stdout);
    assert!(log.contains("koda: sub-agent 'isolated' changes"), "{log}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sub_agent_waiting_on_its_model_releases_its_workspace() {
    let project = Project::new();
    let write = json!({"file_path": "work.txt", "content": "work"});
    let server = stub_with_a_sub_agent(child_call("Write", write), never_answers()).await;
    let child = project.koda(&server);

    // Parent, the child's Write, then the child call that hangs.
    project.wait_for("work.txt").await;
    let deadline = tokio::time::Instant::now() + WAIT;
    while chat_requests(&server).await < 3 {
        assert!(tokio::time::Instant::now() < deadline, "child never hung");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let run = interrupt(child).await;
    assert!(!project.holds("work.txt"), "the workspace is still there");
    assert_work_is_on_a_branch(&project, "work.txt");
    assert_clean_cancel(&run, &project);
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
    assert_clean_cancel(&run, &project);
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
    assert!(stdout.contains(r#""method":"exec/summary""#), "{stdout}");
    assert!(!project.holds("work.txt"), "the workspace is still there");
    assert_work_is_on_a_branch(&project, "work.txt");
}

/// The cancel drops the running Bash call, and with it the command's process
/// group, so the turn ends at once as a clean cancel instead of waiting out
/// the limit and reporting a failure. (`until_abandoned`'s own test covers
/// the limit, for a call that can't be dropped.)
#[tokio::test(flavor = "multi_thread")]
async fn sigint_stops_a_running_command_at_once() {
    let project = Project::new();
    let server = FakeLlmServer::spawn().await;
    server
        .mount_fn("POST", r".*/chat/completions$", |_| {
            // 67 s: no other test sleeps that long, so pgrep finds only this.
            stream_call("Bash", json!({"command": "touch started; sleep 67"}))
        })
        .await;
    let child = project.koda(&server);
    project.wait_for("started").await;

    let run = interrupt(child).await;
    assert_clean_cancel(&run, &project);

    // What finished before the signal still counts.
    let usage = &run.one("turn/completed")["turn"]["usage"];
    assert_eq!(usage["modelCalls"], 1, "{usage}");
    assert_eq!(usage["inputTokens"], 100, "{usage}");

    // The command died with the call; it didn't run on unwatched. A killed
    // process is gone once it next runs, which a busy machine delays: wait
    // for it instead of looking once.
    let deadline = Instant::now() + WAIT;
    loop {
        let left = std::process::Command::new("pgrep")
            .args(["-f", "sleep 67"])
            .output()
            .unwrap();
        if left.stdout.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "still running: {}",
            String::from_utf8_lossy(&left.stdout)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

async fn chat_requests(server: &FakeLlmServer) -> usize {
    server
        .received_requests()
        .await
        .iter()
        .filter(|request| request.url.path().ends_with("/chat/completions"))
        .count()
}
