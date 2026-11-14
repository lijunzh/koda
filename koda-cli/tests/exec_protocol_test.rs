//! The client protocol as a real `koda exec --json` run writes it. Each test
//! runs the binary against a local OpenAI-compatible stub and reads stdout
//! the way a client would. No API key, no model.

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

/// One run of `koda exec --json`, read as a client reads it.
struct Run {
    lines: Vec<Value>,
    stdout: String,
    output: Output,
}

impl Run {
    /// Messages with this method.
    fn of(&self, method: &str) -> Vec<&Value> {
        self.lines
            .iter()
            .filter(|l| l["method"] == method)
            .collect()
    }

    fn one(&self, method: &str) -> &Value {
        let found = self.of(method);
        assert_eq!(found.len(), 1, "{method}: {}", self.stdout);
        found[0]
    }

    fn methods(&self) -> Vec<&str> {
        self.lines
            .iter()
            .filter_map(|l| l["method"].as_str())
            .collect()
    }

    /// `item/completed` params for items of this type.
    fn completed(&self, kind: &str) -> Vec<&Value> {
        self.of("item/completed")
            .into_iter()
            .map(|l| &l["params"])
            .filter(|p| p["item"]["type"] == kind)
            .collect()
    }

    /// The completed tool call with this id.
    fn call(&self, id: &str) -> &Value {
        let found = self.completed("toolCall");
        let call = found.iter().find(|p| p["item"]["id"] == id);
        call.unwrap_or_else(|| panic!("no completed call {id}: {}", self.stdout))
    }

    fn turn(&self) -> &Value {
        &self.one("turn/completed")["params"]["turn"]
    }

    fn summary(&self) -> &Value {
        &self.one("exec/summary")["params"]
    }

    /// Every item that starts completes, once, after it starts.
    fn assert_items_balanced(&self) {
        let mut open = std::collections::HashSet::new();
        for line in &self.lines {
            let id = line["params"]["item"]["id"].as_str().map(str::to_string);
            match (line["method"].as_str(), id) {
                (Some("item/started"), Some(id)) => {
                    assert!(
                        open.insert(id.clone()),
                        "{id} started twice: {}",
                        self.stdout
                    )
                }
                (Some("item/completed"), Some(id)) => {
                    assert!(open.remove(&id), "{id} not open: {}", self.stdout)
                }
                _ => {}
            }
        }
        assert!(open.is_empty(), "never completed {open:?}: {}", self.stdout);
    }
}

fn read_run(output: Output) -> Run {
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let lines: Vec<Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("not JSON: {line}: {e}")))
        .collect();
    for line in &lines {
        assert_eq!(line["jsonrpc"], "2.0", "{line}");
    }
    let run = Run {
        lines,
        stdout,
        output,
    };
    run.assert_items_balanced();
    run
}

fn command(server: &FakeLlmServer, root: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(koda_bin());
    command
        .args(["exec", "Fix the parser", "--json"])
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
async fn a_run_opens_with_its_session_and_closes_with_the_summary() {
    let server = stub(vec![], no_other_calls).await;
    let run = run_koda(&server).await;
    assert!(run.output.status.success(), "{}", run.stdout);

    // The session, what starting it said (the stub reports no context
    // window, so Koda says what it assumed), then the turn.
    let methods = run.methods();
    assert_eq!(methods[..3], ["session/started", "warning", "turn/started"]);
    assert_eq!(
        methods[methods.len() - 2..],
        ["turn/completed", "exec/summary"]
    );

    let started = &run.lines[0]["params"];
    assert_eq!(started["protocolVersion"], 2);
    assert_eq!(started["kodaVersion"], env!("CARGO_PKG_VERSION"));
    let session = &started["session"];
    assert_eq!(session["provider"], "lm-studio");
    assert_eq!(session["model"], "stub");
    assert!(session["contextWindow"].as_u64().unwrap() > 0, "{session}");
    assert_eq!(session["mode"], "auto");
    let tools = session["tools"].as_array().unwrap();
    assert!(tools.contains(&json!("Read")), "{session}");
    assert!(tools.contains(&json!("Bash")), "{session}");
}

#[tokio::test(flavor = "multi_thread")]
async fn every_message_says_which_session_and_turn_it_belongs_to() {
    let server = stub(vec![], no_other_calls).await;
    let run = run_koda(&server).await;

    let session = run.lines[0]["params"]["session"]["id"].as_str().unwrap();
    assert!(!session.is_empty());
    let turn = run.one("turn/started")["params"]["turn"]["id"]
        .as_str()
        .unwrap();
    for line in &run.lines[1..] {
        assert_eq!(line["params"]["sessionId"], session, "{line}");
    }
    let first = run
        .methods()
        .iter()
        .position(|m| *m == "turn/started")
        .unwrap();
    let last = run.lines.len() - 2;
    for line in &run.lines[first + 1..last] {
        assert_eq!(line["params"]["turnId"], turn, "{line}");
    }
    assert_eq!(run.turn()["id"], turn);
}

#[tokio::test(flavor = "multi_thread")]
async fn only_protocol_messages_reach_stdout() {
    let server = stub(vec![], no_other_calls).await;
    let run = run_koda(&server).await;
    let known = koda_client::protocol::ServerNotification::METHODS;
    for method in run.methods() {
        assert!(known.contains(&method), "{method}: {}", run.stdout);
    }
    // No deltas without --json-deltas.
    assert!(
        run.of("item/agentMessage/delta").is_empty(),
        "{}",
        run.stdout
    );
}

// ── Tools ────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_call_says_how_it_ended() {
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

    let status = |id: &str| run.call(id)["item"]["status"].as_str().unwrap().to_string();
    assert_eq!(status("ok"), "completed", "{}", run.stdout);
    assert_eq!(status("exit"), "failed", "a non-zero exit: {}", run.stdout);
    assert_eq!(status("bad"), "failed", "a failed read: {}", run.stdout);
    assert_eq!(run.call("ok")["item"]["arguments"]["command"], "true");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_destructive_call_is_asked_about_resolved_and_declined() {
    let server = stub(
        vec![tool_call("rm", "Bash", json!({"command": "rm -rf build"}))],
        no_other_calls,
    )
    .await;
    let run = run_koda(&server).await;

    let request = run.one("item/toolCall/requestApproval");
    let params = &request["params"];
    assert_eq!(params["itemId"], "rm", "{request}");
    assert_eq!(params["effect"], "destructive", "{request}");
    assert!(params["detail"].as_str().unwrap().contains("rm -rf build"));
    let resolved = run.one("serverRequest/resolved");
    assert_eq!(resolved["params"]["requestId"], request["id"], "{resolved}");
    let at = |line: &Value| run.lines.iter().position(|l| l == line).unwrap();
    assert_eq!(at(resolved), at(request) + 1, "{}", run.stdout);

    let call = &run.call("rm")["item"];
    assert_eq!(call["status"], "declined", "{call}");
    assert!(
        call["output"]
            .as_str()
            .unwrap()
            .starts_with("[auto-rejected")
    );
    assert_eq!(call["detail"], params["detail"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn sub_agent_items_name_the_call_that_started_them() {
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

    let parent_of = |id: &str| run.call(id).get("parentItemId").and_then(Value::as_str);
    assert_eq!(parent_of("read-alpha"), Some("agent-1"), "{}", run.stdout);
    assert_eq!(parent_of("read-beta"), Some("agent-2"), "{}", run.stdout);
    assert_eq!(run.call("read-alpha")["item"]["status"], "completed");
    // The delegation itself is the top-level agent's own call.
    assert_eq!(parent_of("agent-1"), None, "{}", run.stdout);
    // A sub-agent's turn is not a turn of the session.
    assert_eq!(run.of("turn/started").len(), 1, "{}", run.stdout);

    // Their tokens count toward the turn: 2 stream calls and 4 from children.
    let usage = &run.turn()["usage"];
    assert_eq!(usage["modelCalls"], 6, "{usage}");
    assert_eq!(usage["inputTokens"], 100 + 200 + 4 * 30, "{usage}");

    // The run summary agrees, and splits them out by agent.
    let summary = run.summary();
    assert_eq!(summary["usage"]["modelCalls"], 6, "{summary}");
    assert_eq!(summary["usage"]["inputTokens"], 420, "{summary}");
    assert_eq!(
        summary["usageByAgent"]["main"]["modelCalls"], 2,
        "{summary}"
    );
    assert_eq!(
        summary["usageByAgent"]["explore"]["modelCalls"], 4,
        "{summary}"
    );
    assert_eq!(summary["subAgentToolCalls"]["Read"], 2, "{summary}");
}

// ── Typed state ──────────────────────────────────────────────────────────

/// The `retry` of the one `error` that will retry.
fn retry(run: &Run) -> &Value {
    let errors = run.of("error");
    let retrying: Vec<_> = errors
        .iter()
        .filter(|e| e["params"]["willRetry"] == true)
        .collect();
    assert_eq!(retrying.len(), 1, "{}", run.stdout);
    &retrying[0]["params"]["retry"]
}

#[tokio::test(flavor = "multi_thread")]
async fn a_retry_is_an_error_that_will_retry() {
    let server = stub(
        vec![ResponseTemplate::new(429).set_body_string("slow down")],
        no_other_calls,
    )
    .await;
    let run = run_koda(&server).await;

    let retry = retry(&run);
    assert_eq!(retry["reason"], "rateLimit", "{retry}");
    assert_eq!(retry["attempt"], 1, "{retry}");
    assert_eq!(retry["maxAttempts"], 5, "{retry}");
    assert_eq!(retry["delayMs"], 1000, "{retry}");
    // Only the session's start warned (the stub reports no context window).
    assert_eq!(run.of("warning").len(), 1, "{}", run.stdout);
    assert_eq!(run.methods()[1], "warning");
    assert_eq!(run.turn()["status"], "completed");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_error_waits_as_long_as_the_server_says() {
    let busy = ResponseTemplate::new(500)
        .insert_header("retry-after", "0")
        .set_body_string("boom");
    let server = stub(vec![busy], no_other_calls).await;
    let run = run_koda(&server).await;

    assert_eq!(run.output.status.code(), Some(0), "{}", run.stdout);
    let retry = retry(&run);
    assert_eq!(retry["reason"], "serverError", "{retry}");
    assert_eq!(retry["delayMs"], 0, "{retry}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reply_cut_off_partway_starts_over() {
    let chunk = json!({"choices": [{"delta": {"content": "half a re"}}]});
    let cut_off = ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(format!("data: {chunk}\n\n"));
    let server = stub(vec![cut_off], no_other_calls).await;
    let run = run_koda(&server).await;

    assert_eq!(run.output.status.code(), Some(0), "{}", run.stdout);
    let retry = retry(&run);
    assert_eq!(retry["reason"], "streamFailed", "{retry}");
    assert_eq!(retry["attempt"], 1, "{retry}");
    assert_eq!(retry["maxAttempts"], 5, "{retry}");
    let calls = server.received_requests().await;
    let chats = calls
        .iter()
        .filter(|r| r.url.path().ends_with("/completions"));
    assert_eq!(chats.count(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_reply_is_a_retry_and_its_tokens_count() {
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

    assert_eq!(retry(&run)["reason"], "emptyResponse");
    let usage = &run.turn()["usage"];
    assert_eq!(usage["modelCalls"], 3, "{usage}");
    assert_eq!(usage["inputTokens"], 100 + 50 + 200, "{usage}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_loop_guard_is_typed() {
    let same = || tool_call("same", "Bash", json!({"command": "true"}));
    let server = stub((0..10).map(|_| same()).collect(), no_other_calls).await;
    let run = run_koda(&server).await;

    let guards = run.of("turn/loopGuard");
    assert_eq!(guards.len(), 2, "{}", run.stdout);
    assert_eq!(guards[0]["params"]["action"], "feedback");
    assert_eq!(guards[1]["params"]["action"], "stop");
    assert!(
        guards[0]["params"]["detail"]
            .as_str()
            .unwrap()
            .contains("Bash")
    );
    assert!(!run.stdout.contains("Loop detected"), "{}", run.stdout);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_file_sets_the_loop_cap() {
    let server = stub(vec![tool_call("one", "List", json!({}))], no_other_calls).await;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("agents")).unwrap();
    let agent = json!({"name": "default", "system_prompt": "Test.", "max_iterations": 1});
    std::fs::write(tmp.path().join("agents/default.json"), agent.to_string()).unwrap();

    let run = finish(command(&server, tmp.path())).await;

    let request = run.one("turn/loopCap/requestDecision");
    assert_eq!(request["params"]["cap"], 1, "{}", run.stdout);
    let resolved = run.one("serverRequest/resolved");
    assert_eq!(resolved["params"]["requestId"], request["id"]);
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
async fn a_compaction_is_an_item_and_its_tokens_count() {
    let server = filling_stub().await;
    let tmp = tempfile::tempdir().unwrap();
    for i in 0..40 {
        let line = format!("note {i}: nothing to see in this file\n");
        std::fs::write(tmp.path().join(format!("notes_{i}.txt")), line.repeat(400)).unwrap();
    }
    let mut command = command(&server, tmp.path());
    command.args(["--max-context-tokens", "12000"]);
    let run = finish(command).await;

    let started: Vec<&Value> = run
        .of("item/started")
        .into_iter()
        .map(|l| &l["params"]["item"])
        .filter(|i| i["type"] == "contextCompaction" && i["trigger"] == "threshold")
        .collect();
    assert!(!started.is_empty(), "{}", run.stdout);
    assert_eq!(started[0]["status"], "inProgress");
    assert!(
        started[0]["contextPercent"].as_u64().unwrap() >= 80,
        "{}",
        run.stdout
    );
    let done = run.completed("contextCompaction");
    let summarized = done
        .iter()
        .map(|p| &p["item"])
        .find(|i| i["status"] == "completed" && i["trigger"] == "threshold")
        .unwrap_or_else(|| panic!("no compaction finished: {}", run.stdout));
    assert!(
        summarized["messagesRemoved"].as_u64().unwrap() > 0,
        "{summarized}"
    );
    assert!(
        summarized["summaryTokens"].as_u64().unwrap() > 0,
        "{summarized}"
    );
    assert!(!run.stdout.contains("compacting before"), "{}", run.stdout);

    let usage = &run.turn()["usage"];
    let reads = run.completed("toolCall").len() as i64;
    assert_eq!(
        usage["modelCalls"],
        reads + 2,
        "reads, summary, reply: {usage}"
    );
    assert_eq!(usage["inputTokens"], reads * 100 + 70 + 200, "{usage}");
}

// ── Turn usage ───────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn turn_completed_adds_up_the_turn() {
    let server = stub(
        vec![tool_call("a", "Bash", json!({"command": "true"}))],
        no_other_calls,
    )
    .await;
    let run = run_koda(&server).await;

    let turn = run.turn();
    assert_eq!(turn["status"], "completed", "{turn}");
    assert_eq!(turn["usage"]["inputTokens"], 300, "{turn}");
    assert_eq!(turn["usage"]["outputTokens"], 30, "{turn}");
    assert_eq!(turn["usage"]["modelCalls"], 2, "{turn}");
    for field in [
        "cachedInputTokens",
        "cacheCreationTokens",
        "reasoningTokens",
    ] {
        assert_eq!(turn["usage"][field], 0, "{turn}");
    }
    assert!(turn["durationMs"].is_u64(), "{turn}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_turn_still_reports_what_it_used() {
    let server = stub(
        vec![
            tool_call("a", "Bash", json!({"command": "true"})),
            ResponseTemplate::new(400).set_body_string("boom"),
        ],
        no_other_calls,
    )
    .await;
    let run = run_koda(&server).await;
    assert_eq!(run.output.status.code(), Some(1), "{}", run.stdout);

    let turn = run.turn();
    assert_eq!(turn["status"], "failed", "{turn}");
    assert!(turn["error"]["message"].is_string(), "{turn}");
    assert_eq!(turn["usage"]["inputTokens"], 100, "{turn}");
    assert_eq!(turn["usage"]["modelCalls"], 1, "{turn}");
    assert_eq!(run.lines.last().unwrap()["method"], "exec/summary");
    assert_eq!(run.summary()["success"], false);
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

    let turn = run.turn();
    assert_eq!(turn["status"], "interrupted", "{turn}");
    assert_eq!(turn["usage"]["inputTokens"], 100, "{turn}");
    assert_eq!(turn["usage"]["modelCalls"], 1, "{turn}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resumed_turn_reports_only_itself() {
    let server = stub(vec![], no_other_calls).await;
    let tmp = tempfile::tempdir().unwrap();
    let first = finish(command(&server, tmp.path())).await;
    assert!(first.output.status.success(), "{}", first.stdout);
    let session = first.lines[0]["params"]["session"]["id"].as_str().unwrap();

    let mut again = command(&server, tmp.path());
    again.args(["-s", session]);
    let second = finish(again).await;
    assert!(second.output.status.success(), "{}", second.stdout);

    assert_eq!(second.lines[0]["params"]["session"]["id"], session);
    let usage = &second.turn()["usage"];
    assert_eq!(usage["inputTokens"], 200, "this turn: {usage}");
    assert_eq!(usage["modelCalls"], 1, "this turn: {usage}");

    // The run summary covers the stored session, by design.
    let totals = &second.summary()["usage"];
    assert_eq!(totals["inputTokens"], 400, "the session: {totals}");
    assert_eq!(totals["modelCalls"], 2, "the session: {totals}");
}

/// `koda exec -s` keeps the session's own provider, endpoint and model
/// unless flags name others, as `codex exec resume` does.
#[tokio::test(flavor = "multi_thread")]
async fn a_resumed_run_keeps_its_model_unless_flags_say_otherwise() {
    let server = stub(vec![], no_other_calls).await;
    let tmp = tempfile::tempdir().unwrap();
    // `koda exec`, with these flags.
    let exec = |flags: &[&str]| {
        let mut command = tokio::process::Command::new(koda_bin());
        command
            .args(["exec", "Go on", "--json"])
            .args(flags)
            .arg("--project-root")
            .arg(tmp.path())
            .env("XDG_CONFIG_HOME", tmp.path())
            .kill_on_drop(true);
        command
    };
    let url = server.url();
    let first = finish(exec(&[
        "--provider",
        "lmstudio",
        "--base-url",
        &url,
        "--model",
        "first-model",
    ]))
    .await;
    assert!(first.output.status.success(), "{}", first.stdout);
    let session = first.lines[0]["params"]["session"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // No provider, endpoint or model flags: the session's own come back.
    let resumed = finish(exec(&["-s", &session])).await;
    assert!(resumed.output.status.success(), "{}", resumed.stdout);
    assert_eq!(
        resumed.lines[0]["params"]["session"]["model"],
        "first-model"
    );

    let flagged = finish(exec(&[
        "--provider",
        "lmstudio",
        "--base-url",
        &url,
        "--model",
        "flag-model",
        "-s",
        &session,
    ]))
    .await;
    assert!(flagged.output.status.success(), "{}", flagged.stdout);
    assert_eq!(flagged.lines[0]["params"]["session"]["model"], "flag-model");

    let models: Vec<String> = server
        .received_requests()
        .await
        .iter()
        .filter(|r| r.url.path().ends_with("/chat/completions"))
        .map(|r| body(r)["model"].as_str().unwrap_or_default().to_string())
        .filter(|m| !m.is_empty())
        .collect();
    assert_eq!(models, ["first-model", "first-model", "flag-model"]);
}
