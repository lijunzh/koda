//! The v1 wire protocol, as a real `koda -p --output-format stream-json` run
//! writes it. Each test runs the binary against a local OpenAI-compatible stub
//! and reads stdout the way a client would. No API key, no model.

use koda_test_utils::network::{FakeLlmServer, Request, ResponseTemplate};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::Path;
use std::process::Output;
use std::sync::Mutex;
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

fn usage(prompt: u32, completion: u32) -> Value {
    json!({"choices": [], "usage": {"prompt_tokens": prompt, "completion_tokens": completion}})
}

/// A streamed reply that calls one tool.
fn tool_call(id: &str, name: &str, args: Value) -> ResponseTemplate {
    tool_calls(&[(id, name, args)])
}

/// A streamed reply that calls several tools at once.
fn tool_calls(calls: &[(&str, &str, Value)]) -> ResponseTemplate {
    let calls: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(index, (id, name, args))| {
            json!({
                "index": index,
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": args.to_string()},
            })
        })
        .collect();
    sse(&[
        json!({"choices": [{"delta": {"tool_calls": calls}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
        usage(100, 10),
    ])
}

/// A streamed reply of plain text.
fn text(reply: &str) -> ResponseTemplate {
    sse(&[
        json!({"choices": [{"delta": {"content": reply}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
        usage(200, 20),
    ])
}

/// A non-streaming reply, which is how sub-agents and summaries call.
fn plain(reply: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "choices": [{"message": reply, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 30, "completion_tokens": 3},
    }))
}

fn body(request: &Request) -> Value {
    serde_json::from_slice(&request.body).unwrap()
}

/// Answer streaming calls from `script`, in order, then with `Done.`.
/// Non-streaming calls get `other`.
async fn stub(
    script: Vec<ResponseTemplate>,
    other: impl Fn(&Value) -> ResponseTemplate + Send + Sync + 'static,
) -> FakeLlmServer {
    let server = FakeLlmServer::spawn().await;
    let script = Mutex::new(VecDeque::from(script));
    server
        .mount_fn("POST", r".*/chat/completions$", move |request| {
            let request = body(request);
            if request["stream"] != json!(true) {
                return other(&request);
            }
            let next = script.lock().unwrap().pop_front();
            next.unwrap_or_else(|| text("Done."))
        })
        .await;
    server
}

/// One run of `koda -p`, read as a client reads it.
struct Run {
    lines: Vec<Value>,
    stdout: String,
    output: Output,
}

impl Run {
    fn of_type(&self, kind: &str) -> Vec<&Value> {
        self.lines.iter().filter(|l| l["type"] == kind).collect()
    }

    fn one(&self, kind: &str) -> &Value {
        let found = self.of_type(kind);
        assert_eq!(found.len(), 1, "{kind}: {}", self.stdout);
        found[0]
    }

    fn types(&self) -> Vec<&str> {
        self.lines
            .iter()
            .filter_map(|l| l["type"].as_str())
            .collect()
    }
}

fn read_run(output: Output) -> Run {
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let lines = stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("not JSON: {line}: {e}")))
        .collect();
    Run {
        lines,
        stdout,
        output,
    }
}

fn command(server: &FakeLlmServer, root: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(koda_bin());
    command
        .args(["-p", "Fix the parser", "--output-format", "stream-json"])
        .args(["--provider", "lmstudio", "--model", "stub"])
        .args(["--base-url", &server.url()])
        .arg("--project-root")
        .arg(root)
        .env("XDG_CONFIG_HOME", root)
        .kill_on_drop(true);
    command
}

async fn finish(mut command: tokio::process::Command) -> Run {
    let output = tokio::time::timeout(Duration::from_secs(120), command.output())
        .await
        .expect("koda hung")
        .expect("failed to run koda");
    read_run(output)
}

async fn run_koda(server: &FakeLlmServer) -> Run {
    let tmp = tempfile::tempdir().unwrap();
    finish(command(server, tmp.path())).await
}

fn no_other_calls(_: &Value) -> ResponseTemplate {
    ResponseTemplate::new(500)
}

// ── Framing ──────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_run_opens_with_hello_and_session_ready_and_closes_with_the_summary() {
    let server = stub(vec![], no_other_calls).await;
    let run = run_koda(&server).await;
    assert!(run.output.status.success(), "{}", run.stdout);

    let types = run.types();
    assert_eq!(types[..3], ["hello", "session_ready", "turn_start"]);
    assert_eq!(
        types[types.len() - 3..],
        ["turn_summary", "turn_end", "run_summary"]
    );

    let hello = &run.lines[0];
    assert_eq!(hello["protocol_version"], 1);
    assert_eq!(hello["koda_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(hello["capabilities"], json!(["one_shot", "self_answer"]));

    let ready = &run.lines[1];
    assert_eq!(ready["provider"], "lm-studio");
    assert_eq!(ready["model"], "stub");
    assert!(ready["context_window"].as_u64().unwrap() > 0, "{ready}");
    assert_eq!(ready["trust"], "auto");
    let tools = ready["tools"].as_array().unwrap();
    assert!(tools.contains(&json!("Read")), "{ready}");
    assert!(tools.contains(&json!("Bash")), "{ready}");
}

#[tokio::test(flavor = "multi_thread")]
async fn every_line_says_which_session_and_turn_it_belongs_to() {
    let server = stub(vec![], no_other_calls).await;
    let run = run_koda(&server).await;

    assert!(run.lines[0].get("session_id").is_none(), "hello has none");
    let session = run.lines[1]["session_id"].as_str().unwrap();
    assert!(!session.is_empty());
    let turn = run.one("turn_start")["turn_id"].as_str().unwrap();

    let last = run.lines.len() - 1;
    for line in &run.lines[1..] {
        assert_eq!(line["session_id"], session, "{line}");
    }
    for line in &run.lines[2..last] {
        assert_eq!(line["turn_id"], turn, "{line}");
    }
    assert!(run.lines[1].get("turn_id").is_none(), "before the turn");
    assert!(run.lines[last].get("turn_id").is_none(), "after the turn");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_wire_carries_no_presentation() {
    let server = stub(vec![], no_other_calls).await;
    let run = run_koda(&server).await;
    for kind in [
        "spinner_start",
        "spinner_stop",
        "status_update",
        "response_start",
        "footer",
        "model_usage",
    ] {
        assert!(run.of_type(kind).is_empty(), "{kind}: {}", run.stdout);
    }
}

// ── Tools ────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_result_carries_its_outcome() {
    let server = stub(
        vec![
            tool_call("ok", "Bash", json!({"command": "true"})),
            tool_call("exit", "Bash", json!({"command": "false"})),
            tool_call("bad", "Read", json!({"file_path": "missing.txt"})),
        ],
        no_other_calls,
    )
    .await;
    let run = run_koda(&server).await;

    let outcome = |id: &str| {
        let results = run.of_type("tool_call_result");
        let result = results.iter().find(|r| r["id"] == id).unwrap();
        result["outcome"].as_str().unwrap().to_string()
    };
    assert_eq!(outcome("ok"), "success", "{}", run.stdout);
    assert_eq!(outcome("exit"), "error", "a non-zero exit: {}", run.stdout);
    assert_eq!(outcome("bad"), "error", "a failed read: {}", run.stdout);
}

#[tokio::test(flavor = "multi_thread")]
async fn sub_agent_lines_name_the_call_that_started_them() {
    // Two sub-agents at once, so nothing but the sink can tell them apart.
    let script = vec![tool_calls(&[
        (
            "agent-1",
            "InvokeAgent",
            json!({"agent_name": "explore", "prompt": "find alpha"}),
        ),
        (
            "agent-2",
            "InvokeAgent",
            json!({"agent_name": "explore", "prompt": "find beta"}),
        ),
    ])];
    let server = stub(script, |request| {
        // First call of each child reads a file named for its task; the
        // second, with the result in its history, finishes.
        let prompt = request["messages"].to_string();
        let name = if prompt.contains("find alpha") {
            "alpha"
        } else {
            "beta"
        };
        if request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "tool")
        {
            return plain(json!({"role": "assistant", "content": "found"}));
        }
        plain(json!({"role": "assistant", "tool_calls": [{
            "id": format!("read-{name}"),
            "type": "function",
            "function": {
                "name": "Read",
                "arguments": json!({"file_path": format!("{name}.txt")}).to_string(),
            },
        }]}))
    })
    .await;

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("alpha.txt"), "a").unwrap();
    std::fs::write(tmp.path().join("beta.txt"), "b").unwrap();
    let run = finish(command(&server, tmp.path())).await;

    let starts = run.of_type("sub_agent_start");
    assert_eq!(starts.len(), 2, "one per agent: {}", run.stdout);
    let mut parents: Vec<&str> = starts
        .iter()
        .map(|s| s["parent_id"].as_str().unwrap())
        .collect();
    parents.sort();
    assert_eq!(parents, ["agent-1", "agent-2"], "{}", run.stdout);

    let starts = run.of_type("tool_call_start");
    let parent_of = |id: &str| {
        let line = starts.iter().find(|s| s["id"] == id).unwrap();
        line.get("parent_id").and_then(Value::as_str)
    };
    assert_eq!(parent_of("read-alpha"), Some("agent-1"), "{}", run.stdout);
    assert_eq!(parent_of("read-beta"), Some("agent-2"), "{}", run.stdout);
    // The delegation itself is the top-level agent's own call.
    assert_eq!(parent_of("agent-1"), None, "{}", run.stdout);
    assert!(!run.stdout.contains("is_sub_agent"), "{}", run.stdout);

    // Their tokens count toward the turn: 2 stream calls and 4 from children.
    let usage = &run.one("turn_summary")["usage"];
    assert_eq!(usage["api_calls"], 6, "{usage}");
    assert_eq!(usage["prompt_tokens"], 100 + 200 + 4 * 30, "{usage}");

    // The run summary agrees, and splits them out by agent.
    let summary = run.one("run_summary");
    assert_eq!(summary["usage"]["api_calls"], 6, "{summary}");
    assert_eq!(summary["usage"]["prompt_tokens"], 420, "{summary}");
    assert_eq!(
        summary["usage_by_agent"]["main"]["api_calls"], 2,
        "{summary}"
    );
    assert_eq!(
        summary["usage_by_agent"]["explore"]["api_calls"], 4,
        "{summary}"
    );
}

// ── Typed state ──────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_retry_is_typed() {
    let server = stub(
        vec![ResponseTemplate::new(429).set_body_string("slow down")],
        no_other_calls,
    )
    .await;
    let run = run_koda(&server).await;

    let retry = run.one("retry");
    assert_eq!(retry["reason"], "rate_limit", "{retry}");
    assert_eq!(retry["attempt"], 1, "{retry}");
    assert_eq!(retry["max_attempts"], 5, "{retry}");
    assert_eq!(retry["delay_ms"], 1000, "{retry}");
    assert!(run.of_type("warn").is_empty(), "{}", run.stdout);
    assert_eq!(run.one("turn_end")["reason"]["kind"], "complete");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_reply_is_a_typed_retry_and_its_tokens_count() {
    let empty = sse(&[
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
        usage(50, 0),
    ]);
    let server = stub(
        vec![tool_call("a", "Bash", json!({"command": "true"})), empty],
        no_other_calls,
    )
    .await;
    let run = run_koda(&server).await;

    let retry = run.one("retry");
    assert_eq!(retry["reason"], "empty_response", "{retry}");
    let usage = &run.one("turn_summary")["usage"];
    assert_eq!(usage["api_calls"], 3, "{usage}");
    assert_eq!(usage["prompt_tokens"], 100 + 50 + 200, "{usage}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_loop_guard_is_typed() {
    let same = || tool_call("same", "Bash", json!({"command": "true"}));
    let server = stub((0..10).map(|_| same()).collect(), no_other_calls).await;
    let run = run_koda(&server).await;

    let guards = run.of_type("loop_guard");
    assert_eq!(guards.len(), 2, "{}", run.stdout);
    assert_eq!(guards[0]["action"], "feedback");
    assert_eq!(guards[1]["action"], "stop");
    assert!(guards[0]["detail"].as_str().unwrap().contains("Bash"));
    assert!(!run.stdout.contains("Loop detected"), "{}", run.stdout);
    assert!(!run.stdout.contains("Loop guard:"), "{}", run.stdout);
}

/// Stub an agent that reads big files until a summary call arrives.
async fn filling_stub() -> FakeLlmServer {
    let server = FakeLlmServer::spawn().await;
    let reads = Mutex::new(0usize);
    let summarized = Mutex::new(false);
    server
        .mount_fn("POST", r".*/chat/completions$", move |request| {
            let request = body(request);
            if request["stream"] != json!(true) {
                *summarized.lock().unwrap() = true;
                return ResponseTemplate::new(200).set_body_json(json!({
                    "choices": [{
                        "message": {"role": "assistant", "content": "<summary>Read notes.</summary>"},
                        "finish_reason": "stop",
                    }],
                    "usage": {"prompt_tokens": 70, "completion_tokens": 7},
                }));
            }
            if *summarized.lock().unwrap() {
                return text("Done.");
            }
            let mut reads = reads.lock().unwrap();
            *reads += 1;
            tool_call(
                &format!("read-{reads}"),
                "Read",
                json!({"file_path": format!("notes_{}.txt", *reads % 40)}),
            )
        })
        .await;
    server
}

#[tokio::test(flavor = "multi_thread")]
async fn a_compaction_is_typed_and_its_tokens_count() {
    let server = filling_stub().await;
    let tmp = tempfile::tempdir().unwrap();
    for i in 0..40 {
        let line = format!("note {i}: nothing to see in this file\n");
        std::fs::write(tmp.path().join(format!("notes_{i}.txt")), line.repeat(400)).unwrap();
    }
    let mut command = command(&server, tmp.path());
    command.args(["--max-context-tokens", "12000"]);
    let run = finish(command).await;

    let steps = run.of_type("compaction");
    let threshold: Vec<_> = steps
        .iter()
        .filter(|s| s["trigger"] == "threshold")
        .collect();
    assert_eq!(threshold[0]["status"], "started", "{}", run.stdout);
    assert!(
        threshold[0]["context_percent"].as_u64().unwrap() >= 80,
        "{}",
        run.stdout
    );
    // Each start is closed by the next line, and one of them got a summary.
    for pair in threshold.chunks(2) {
        assert_eq!(pair[0]["status"], "started", "{}", run.stdout);
        assert_ne!(pair[1]["status"], "started", "{}", run.stdout);
    }
    let done = threshold
        .iter()
        .find(|s| s["status"] == "completed")
        .unwrap_or_else(|| panic!("no compaction finished: {}", run.stdout));
    assert!(done["messages_removed"].as_u64().unwrap() > 0, "{done}");
    assert!(done["summary_tokens"].as_u64().unwrap() > 0, "{done}");
    assert!(!run.stdout.contains("compacting before"), "{}", run.stdout);

    let usage = &run.one("turn_summary")["usage"];
    let reads = run.of_type("tool_call_start").len() as i64;
    assert_eq!(
        usage["api_calls"],
        reads + 2,
        "reads, summary, reply: {usage}"
    );
    assert_eq!(usage["prompt_tokens"], reads * 100 + 70 + 200, "{usage}");
}

// ── Turn summary ─────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn turn_summary_adds_up_the_turn() {
    let server = stub(
        vec![tool_call("a", "Bash", json!({"command": "true"}))],
        no_other_calls,
    )
    .await;
    let run = run_koda(&server).await;

    let summary = run.one("turn_summary");
    assert_eq!(summary["usage"]["prompt_tokens"], 300, "{summary}");
    assert_eq!(summary["usage"]["completion_tokens"], 30, "{summary}");
    assert_eq!(summary["usage"]["api_calls"], 2, "{summary}");
    for field in [
        "cache_read_tokens",
        "cache_creation_tokens",
        "thinking_tokens",
    ] {
        assert_eq!(summary["usage"][field], 0, "{summary}");
    }
    assert!(summary["elapsed_ms"].is_u64(), "{summary}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_turn_still_reports_what_it_used() {
    let server = stub(
        vec![
            tool_call("a", "Bash", json!({"command": "true"})),
            ResponseTemplate::new(500).set_body_string("boom"),
        ],
        no_other_calls,
    )
    .await;
    let run = run_koda(&server).await;
    assert_eq!(run.output.status.code(), Some(1), "{}", run.stdout);

    assert_eq!(run.one("turn_end")["reason"]["kind"], "error");
    let usage = &run.one("turn_summary")["usage"];
    assert_eq!(usage["prompt_tokens"], 100, "{usage}");
    assert_eq!(usage["api_calls"], 1, "{usage}");
    assert_eq!(run.lines.last().unwrap()["type"], "run_summary");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_turn_still_reports_what_it_used() {
    use std::process::Stdio;

    let server = stub(
        vec![
            tool_call("a", "Bash", json!({"command": "true"})),
            ResponseTemplate::new(200).set_delay(Duration::from_secs(120)),
        ],
        no_other_calls,
    )
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut command = command(&server, tmp.path());
    let child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Signal once the second model call is in flight.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let requests = server.received_requests().await;
        let calls = requests
            .iter()
            .filter(|r| r.url.path().ends_with("/chat/completions"));
        if calls.count() >= 2 {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "no second call");
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
    let run = read_run(output);

    assert_eq!(run.one("turn_end")["reason"]["kind"], "cancelled");
    let usage = &run.one("turn_summary")["usage"];
    assert_eq!(usage["prompt_tokens"], 100, "{usage}");
    assert_eq!(usage["api_calls"], 1, "{usage}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resumed_turn_reports_only_itself() {
    let server = stub(vec![], no_other_calls).await;
    let tmp = tempfile::tempdir().unwrap();
    let first = finish(command(&server, tmp.path())).await;
    assert!(first.output.status.success(), "{}", first.stdout);
    let session = first.lines[1]["session_id"].as_str().unwrap();

    let mut again = command(&server, tmp.path());
    again.args(["-s", session]);
    let second = finish(again).await;
    assert!(second.output.status.success(), "{}", second.stdout);

    assert_eq!(second.lines[1]["session_id"], session, "{}", second.stdout);
    let usage = &second.one("turn_summary")["usage"];
    assert_eq!(usage["prompt_tokens"], 200, "this turn: {usage}");
    assert_eq!(usage["api_calls"], 1, "this turn: {usage}");

    // The run summary covers the stored session, by design.
    let totals = &second.one("run_summary")["usage"];
    assert_eq!(totals["prompt_tokens"], 400, "the session: {totals}");
    assert_eq!(totals["api_calls"], 2, "the session: {totals}");
}

// ── One-shot mode ────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn the_json_format_is_still_one_summary_object() {
    let server = stub(vec![], no_other_calls).await;
    let tmp = tempfile::tempdir().unwrap();
    let mut command = tokio::process::Command::new(koda_bin());
    command
        .args(["-p", "Fix the parser", "--output-format", "json"])
        .args(["--provider", "lmstudio", "--model", "stub"])
        .args(["--base-url", &server.url()])
        .arg("--project-root")
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(120), command.output())
        .await
        .expect("koda hung")
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let start = stdout.find('{').expect("no JSON on stdout");
    let summary: Value = serde_json::from_str(&stdout[start..]).unwrap();
    assert_eq!(summary["type"], "run_summary", "{stdout}");
    assert_eq!(summary["success"], true, "{stdout}");
    assert_eq!(summary["response"], "Done.", "{stdout}");
    assert!(summary["session_id"].is_string(), "{stdout}");
    assert_eq!(summary["usage"]["prompt_tokens"], 200, "{stdout}");
}
