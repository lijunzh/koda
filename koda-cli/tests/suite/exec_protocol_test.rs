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
    env!("CARGO_BIN_EXE_koda").to_string()
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

/// A turn that fails on an error the host sends no notice for (here a
/// rejected key) says why: on stderr in text mode, as Codex's `exec` prints
/// `ERROR:`, and in `--json`'s summary. Before, the text run exited 1 with
/// nothing said.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_turn_says_why() {
    let rejected = || {
        ResponseTemplate::new(401)
            .set_body_json(json!({"error": {"message": "invalid api key", "type": "auth"}}))
    };
    let server = stub(vec![rejected(), rejected(), rejected()], no_other_calls).await;
    let tmp = tempfile::tempdir().unwrap();

    // The text run: `command` adds --json, so this one is built here.
    let mut text_run = tokio::process::Command::new(koda_bin());
    text_run
        .args(["exec", "Fix the parser"])
        .args(["--provider", "lmstudio", "--model", "stub"])
        .args(["--base-url", &server.url()])
        .args(["-c", "model_providers.lmstudio.request_max_retries=0"])
        .arg("--project-root")
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(120), text_run.output())
        .await
        .expect("koda hung")
        .expect("failed to run koda");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("401"), "the reason is missing: {stderr}");

    let mut json_run = command(&server, tmp.path());
    json_run
        .as_std_mut()
        .args(["-c", "model_providers.lmstudio.request_max_retries=0"]);
    let run = finish(json_run).await;
    assert_eq!(run.summary()["turnStatus"], "failed");
    let error = run.summary()["error"].as_str().unwrap_or_default();
    assert!(error.contains("401"), "{error}");
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
    std::fs::create_dir_all(tmp.path().join(".koda/agents")).unwrap();
    let agent = json!({"name": "default", "system_prompt": "Test.", "max_iterations": 1});
    std::fs::write(
        tmp.path().join(".koda/agents/default.json"),
        agent.to_string(),
    )
    .unwrap();

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

/// `koda exec resume` picks a saved session as `codex exec resume` does:
/// `--last` the most recent in this project, or an id's first characters.
#[tokio::test(flavor = "multi_thread")]
async fn exec_resume_takes_the_last_session_or_an_id() {
    let server = stub(vec![], no_other_calls).await;
    let tmp = tempfile::tempdir().unwrap();
    let session_of = |run: &Run| {
        run.lines[0]["params"]["session"]["id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let first = finish(command(&server, tmp.path())).await;
    assert!(first.output.status.success(), "{}", first.stdout);
    let second = finish(command(&server, tmp.path())).await;
    assert!(second.output.status.success(), "{}", second.stdout);
    let (first, second) = (session_of(&first), session_of(&second));
    assert_ne!(first, second);

    let resume = |args: &[&str]| {
        let mut command = tokio::process::Command::new(koda_bin());
        command
            .args(["exec", "resume"])
            .args(args)
            .args(["--json", "--provider", "lmstudio", "--model", "stub"])
            .args(["--base-url", &server.url()])
            .arg("--project-root")
            .arg(tmp.path())
            .env("XDG_CONFIG_HOME", tmp.path())
            .kill_on_drop(true);
        command
    };
    let last = finish(resume(&["--last", "Carry on"])).await;
    assert!(last.output.status.success(), "{}", last.stdout);
    assert_eq!(session_of(&last), second, "--last");

    let by_id = finish(resume(&[&first[..8], "Carry on"])).await;
    assert!(by_id.output.status.success(), "{}", by_id.stdout);
    assert_eq!(session_of(&by_id), first, "by id");

    let neither = finish(resume(&["Carry on"])).await;
    assert!(
        !neither.output.status.success(),
        "a prompt alone names no session"
    );
}

/// `koda exec fork` runs the prompt in a copy of the session, as `codex
/// exec fork` does: by an id's first characters, with the history, the
/// original left as it was. Without a prompt it only forks and prints the
/// copy's id.
#[tokio::test(flavor = "multi_thread")]
async fn exec_fork_runs_the_prompt_in_a_copy_or_only_forks() {
    use koda_state::session_store::SessionStore;
    let server = stub(vec![], no_other_calls).await;
    let tmp = tempfile::tempdir().unwrap();
    let session_of = |run: &Run| {
        run.lines[0]["params"]["session"]["id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let first = finish(command(&server, tmp.path())).await;
    assert!(first.output.status.success(), "{}", first.stdout);
    let source = session_of(&first);

    let fork = |args: &[&str]| {
        let mut command = tokio::process::Command::new(koda_bin());
        command
            .args(["exec", "fork"])
            .args(args)
            .args(["--provider", "lmstudio", "--model", "stub"])
            .args(["--base-url", &server.url()])
            .arg("--project-root")
            .arg(tmp.path())
            .env("XDG_CONFIG_HOME", tmp.path())
            .kill_on_drop(true);
        command
    };
    let run = finish(fork(&[&source[..8], "Carry on", "--json"])).await;
    assert!(run.output.status.success(), "{}", run.stdout);
    let copy = session_of(&run);
    assert_ne!(copy, source);
    let requests = server.received_requests().await;
    let last = String::from_utf8_lossy(&requests.last().unwrap().body).to_string();
    assert!(last.contains("Fix the parser"), "history lost: {last}");
    assert!(last.contains("Carry on"), "{last}");
    let db = koda_state::db::Database::init(&tmp.path().join("koda"))
        .await
        .unwrap();
    let said = |messages: Vec<koda_state::session_store::Message>| {
        messages
            .iter()
            .filter_map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(!said(db.load_all_messages(&source).await.unwrap()).contains("Carry on"));

    // No prompt: the fork only, its id on stdout, and no model call.
    let chats = || async {
        server
            .received_requests()
            .await
            .iter()
            .filter(|r| r.url.path().ends_with("/chat/completions"))
            .count()
    };
    let calls = chats().await;
    // Text output: the id alone, not JSON.
    let only = fork(&[&source]).output().await.unwrap();
    let stdout = String::from_utf8_lossy(&only.stdout);
    assert!(
        only.status.success(),
        "{}",
        String::from_utf8_lossy(&only.stderr)
    );
    let only_id = stdout.trim().to_string();
    assert_eq!(only_id.len(), 36, "{stdout:?}");
    assert_ne!(only_id, source);
    assert_eq!(chats().await, calls, "the fork alone called the model");
    assert!(said(db.load_all_messages(&only_id).await.unwrap()).contains("Fix the parser"));

    // Images and -o go with a prompt.
    let refused = fork(&[&source, "-o", "last.txt"]).output().await.unwrap();
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("only with a prompt"), "{stderr}");
}

/// `koda exec fork <id> --worktree` runs the copy in a new worktree of the
/// repository the session worked in, wherever the command runs, as `codex
/// exec fork --worktree` takes the thread's saved cwd.
#[tokio::test(flavor = "multi_thread")]
async fn exec_fork_worktree_runs_the_copy_in_a_checkout_of_the_sessions_repository() {
    let server = stub(vec![], no_other_calls).await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("proj");
    std::fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        let ok = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(&repo)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hi\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "first"]);
    let config = tmp.path().join("config");
    let mut first = command(&server, &repo);
    first.env("XDG_CONFIG_HOME", &config);
    let first = finish(first).await;
    assert!(first.output.status.success(), "{}", first.stdout);
    let source = first.lines[0]["params"]["session"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // From another folder, without --project-root; two side by side, as
    // the docs suggest for two attempts.
    let elsewhere = tmp.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    let fork = |prompt: &str| {
        let mut fork = tokio::process::Command::new(koda_bin());
        fork.args(["exec", "fork", &source, prompt, "--worktree", "--json"])
            .args(["--provider", "lmstudio", "--model", "stub"])
            .args(["--base-url", &server.url()])
            .current_dir(&elsewhere)
            .env("XDG_CONFIG_HOME", &config)
            .kill_on_drop(true);
        fork
    };
    let (run, other) = tokio::join!(finish(fork("Go on")), finish(fork("Or this")));
    assert!(
        other.output.status.success(),
        "{}",
        String::from_utf8_lossy(&other.output.stderr)
    );
    let buckets = std::fs::read_dir(config.join("koda/worktrees"))
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .count();
    assert_eq!(buckets, 2, "one checkout each");
    let stderr = String::from_utf8_lossy(&run.output.stderr);
    assert!(run.output.status.success(), "{stderr}");
    assert!(stderr.contains("The worktree stays at"), "{stderr}");
    let started = run.of("session/started")[0];
    let copy = started["params"]["session"]["id"].as_str().unwrap();
    assert_ne!(copy, source);
    // This run's checkout is the one its session started in.
    let checkout = std::path::PathBuf::from(started["params"]["session"]["cwd"].as_str().unwrap());
    assert!(
        checkout.starts_with(config.join("koda/worktrees").canonicalize().unwrap()),
        "{}",
        checkout.display()
    );
    assert_eq!(checkout.file_name().unwrap(), "proj");
    assert_eq!(
        std::fs::read_to_string(checkout.join("README.md")).unwrap(),
        "hi\n"
    );
    assert_eq!(
        koda_git::worktree::owner(&checkout).unwrap().as_deref(),
        Some(copy)
    );
}

// ── turn_token_budget ────────────────────────────────────────────────────

/// A streamed reply that calls one tool, reporting `prompt` input tokens.
fn costly_tool_call(id: &str, name: &str, args: Value, prompt: u32) -> ResponseTemplate {
    sse(&[
        json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": id,
            "type": "function",
            "function": {"name": name, "arguments": args.to_string()},
        }]}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
        usage(prompt, 10),
    ])
}

/// `koda exec` with `-c turn_token_budget=20000`.
fn budgeted(server: &FakeLlmServer, root: &Path) -> tokio::process::Command {
    let mut command = command(server, root);
    command.args(["-c", "turn_token_budget=20000"]);
    command
}

/// The chat calls the server got, streamed or not.
async fn chat_bodies(server: &FakeLlmServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .iter()
        .filter(|r| r.url.path().ends_with("/chat/completions"))
        .map(|r| String::from_utf8_lossy(&r.body).to_string())
        .collect()
}

/// At 90% of `turn_token_budget` the model is told to wrap up, as Codex
/// tells a goal at its budget; once the budget is spent the turn makes no
/// further call, as Claude Code's `--max-budget-usd` stops a run. The call
/// that spent it completes, and its tool runs. The turn fails with a code
/// a client can read, and `koda exec` exits 1.
#[tokio::test(flavor = "multi_thread")]
async fn a_turn_wraps_up_near_its_token_budget_and_stops_at_it() {
    let script = vec![
        costly_tool_call("read-1", "Read", json!({"file_path": "a.txt"}), 18_000),
        costly_tool_call("read-2", "Read", json!({"file_path": "a.txt"}), 3_000),
    ];
    let server = stub(script, no_other_calls).await;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "a").unwrap();
    let run = finish(budgeted(&server, tmp.path())).await;

    let calls = chat_bodies(&server).await;
    assert_eq!(calls.len(), 2, "no call after the budget: {}", run.stdout);
    assert!(!calls[0].contains("Wrap up now"), "{}", calls[0]);
    assert!(
        calls[1].contains("Wrap up now"),
        "told at 90%: {}",
        calls[1]
    );
    // The call that spent the budget still had its tool run.
    assert_eq!(run.call("read-2")["item"]["status"], "completed");

    let turn = run.turn();
    assert_eq!(turn["status"], "failed", "{turn}");
    assert_eq!(turn["error"]["code"], "tokenBudgetSpent", "{turn}");
    let message = turn["error"]["message"].as_str().unwrap();
    assert!(message.contains("of 20000 tokens"), "{message}");
    // Said once: the failed turn's line, no warning before it.
    assert!(
        !run.of("warning")
            .iter()
            .any(|w| w["params"]["message"].to_string().contains("budget")),
        "{}",
        run.stdout
    );
    assert_eq!(run.summary()["success"], false);
    assert_eq!(run.output.status.code(), Some(1));
}

/// Once the budget is spent no sub-agent starts, as Claude Code refuses
/// one at `--max-budget-usd`.
#[tokio::test(flavor = "multi_thread")]
async fn no_sub_agent_starts_once_the_budget_is_spent() {
    let script = vec![costly_tool_call(
        "agent-1",
        "InvokeAgent",
        json!({"agent_name": "explore", "prompt": "find alpha"}),
        25_000,
    )];
    let server = stub(script, no_other_calls).await;
    let tmp = tempfile::tempdir().unwrap();
    let run = finish(budgeted(&server, tmp.path())).await;

    let output = run.call("agent-1")["item"].to_string();
    assert!(output.contains("Budget limit reached"), "{output}");
    assert_eq!(
        chat_bodies(&server).await.len(),
        1,
        "the child never called"
    );
    assert_eq!(run.turn()["error"]["code"], "tokenBudgetSpent");
}

/// A running sub-agent stops at its next call once the turn's budget is
/// spent, and returns what it has.
#[tokio::test(flavor = "multi_thread")]
async fn a_running_sub_agent_stops_when_the_budget_is_spent() {
    let script = vec![tool_call(
        "agent-1",
        "InvokeAgent",
        json!({"agent_name": "explore", "prompt": "find alpha"}),
    )];
    // The child's first call spends the budget on its own.
    let server = stub(script, |_| {
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"role": "assistant", "content": "looking", "tool_calls": [{
                "id": "read-alpha",
                "type": "function",
                "function": {"name": "Read", "arguments": json!({"file_path": "alpha.txt"}).to_string()},
            }]}, "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": 25_000, "completion_tokens": 3},
        }))
    })
    .await;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("alpha.txt"), "a").unwrap();
    let run = finish(budgeted(&server, tmp.path())).await;

    // One parent call and one child call: neither called again.
    assert_eq!(chat_bodies(&server).await.len(), 2, "{}", run.stdout);
    let output = run.call("agent-1")["item"].to_string();
    assert!(
        output.contains("token budget (turn_token_budget) is spent"),
        "{output}"
    );
    assert_eq!(run.turn()["error"]["code"], "tokenBudgetSpent");
}

/// A budget under the Claude API's smallest task budget stops the run
/// before it starts.
#[tokio::test(flavor = "multi_thread")]
async fn a_turn_token_budget_under_twenty_thousand_is_refused() {
    let server = stub(vec![], no_other_calls).await;
    let tmp = tempfile::tempdir().unwrap();
    let mut command = command(&server, tmp.path());
    command.args(["-c", "turn_token_budget=1000"]);
    let output = command.output().await.unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("at least 20000"), "{stderr}");
    assert!(chat_bodies(&server).await.is_empty());
}

/// `koda exec -i` sends the image with the prompt, as `codex exec -i` does.
#[tokio::test(flavor = "multi_thread")]
async fn exec_image_goes_with_the_prompt() {
    let server = stub(vec![], no_other_calls).await;
    let tmp = tempfile::tempdir().unwrap();
    // A 1x1 PNG.
    let png = [
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8,
        0xcf, 0xc0, 0xf0, 0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    let image = tmp.path().join("red.png");
    std::fs::write(&image, png).unwrap();
    let mut command = command(&server, tmp.path());
    command
        .arg("-i")
        .arg(&image)
        .args(["-c", r#"model_input_modalities=["text","image"]"#]);
    let run = finish(command).await;
    assert!(run.output.status.success(), "{}", run.stdout);
    let requests = server.received_requests().await;
    let sent = requests
        .iter()
        .map(|r| String::from_utf8_lossy(&r.body).to_string());
    assert!(
        sent.clone().any(|b| b.contains("data:image/png;base64,")),
        "no image in the requests"
    );

    // A path that isn't an image stops the run before it starts.
    let mut command = self::command(&server, tmp.path());
    command.args(["-i", "notes.txt"]);
    let refused = finish(command).await;
    assert!(!refused.output.status.success());
    let stderr = String::from_utf8_lossy(&refused.output.stderr);
    assert!(stderr.contains("--image notes.txt"), "{stderr}");
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

/// `koda exec` without flags runs on config.toml's choice, as `codex exec`
/// uses `config.toml`; `--model` alone keeps the saved provider, and the
/// provider's endpoint comes from `[model_providers.lm-studio]`.
#[tokio::test(flavor = "multi_thread")]
async fn a_run_without_flags_uses_the_saved_choice() {
    let server = stub(vec![], no_other_calls).await;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("koda")).unwrap();
    std::fs::write(
        tmp.path().join("koda/config.toml"),
        format!(
            "model_provider = \"lm-studio\"\nmodel = \"saved-model\"\n\n\
             [model_providers.lm-studio]\nbase_url = \"{}\"\n",
            server.url()
        ),
    )
    .unwrap();
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

    let plain = finish(exec(&[])).await;
    assert!(plain.output.status.success(), "{}", plain.stdout);
    assert_eq!(plain.lines[0]["params"]["session"]["model"], "saved-model");

    let flagged = finish(exec(&["--model", "flag-model"])).await;
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
    assert_eq!(models, ["saved-model", "flag-model"]);
}

/// `-c key=value` sits over config.toml for one run, as Codex's does: it
/// beats the file, `--model` beats it, and an unknown key stops the run
/// before anything starts.
#[tokio::test(flavor = "multi_thread")]
async fn a_config_override_beats_the_file_and_a_flag_beats_it() {
    let server = stub(vec![], no_other_calls).await;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("koda")).unwrap();
    std::fs::write(
        tmp.path().join("koda/config.toml"),
        format!(
            "model_provider = \"lm-studio\"\nmodel = \"file-model\"\n\n\
             [model_providers.lm-studio]\nbase_url = \"{}\"\n",
            server.url()
        ),
    )
    .unwrap();
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

    let overridden = finish(exec(&["-c", "model=c-model"])).await;
    assert!(overridden.output.status.success(), "{}", overridden.stdout);
    assert_eq!(overridden.lines[0]["params"]["session"]["model"], "c-model");

    let flagged = finish(exec(&["-c", "model=c-model", "--model", "flag-model"])).await;
    assert!(flagged.output.status.success(), "{}", flagged.stdout);
    assert_eq!(flagged.lines[0]["params"]["session"]["model"], "flag-model");

    let unknown = finish(exec(&["-c", "modle=x"])).await;
    assert!(!unknown.output.status.success());
    let stderr = String::from_utf8_lossy(&unknown.output.stderr);
    assert!(
        stderr.contains("unknown configuration field `modle`"),
        "{stderr}"
    );

    // Nothing from -c reached the file.
    let file = std::fs::read_to_string(tmp.path().join("koda/config.toml")).unwrap();
    assert!(!file.contains("c-model"), "{file}");
}

/// A new session starts in the config file's `mode`, and `-c mode=` beats
/// it, as `--mode` does.
#[tokio::test(flavor = "multi_thread")]
async fn a_run_starts_in_the_files_mode() {
    let server = stub(vec![], no_other_calls).await;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("koda")).unwrap();
    std::fs::write(tmp.path().join("koda/config.toml"), "mode = \"safe\"\n").unwrap();

    let plain = command(&server, tmp.path());
    let run = finish(plain).await;
    assert!(run.output.status.success(), "{}", run.stdout);
    assert_eq!(run.lines[0]["params"]["session"]["mode"], "safe");

    let mut flagged = command(&server, tmp.path());
    flagged.args(["-c", "mode=plan"]);
    let run = finish(flagged).await;
    assert!(run.output.status.success(), "{}", run.stdout);
    assert_eq!(run.lines[0]["params"]["session"]["mode"], "plan");
}

/// A setting variable Koda no longer reads changes nothing, and the run
/// says so on stderr, naming what replaced it (CF-3b).
#[tokio::test(flavor = "multi_thread")]
async fn a_retired_variable_is_named_and_ignored() {
    let server = stub(vec![], no_other_calls).await;
    let tmp = tempfile::tempdir().unwrap();
    let mut run = command(&server, tmp.path());
    run.env("KODA_MODEL", "from-env").env("KODA_MODE", "safe");
    let run = finish(run).await;
    assert!(run.output.status.success(), "{}", run.stdout);
    let session = &run.lines[0]["params"]["session"];
    assert_eq!(session["model"], "stub");
    assert_eq!(session["mode"], "auto");
    let stderr = String::from_utf8_lossy(&run.output.stderr);
    assert!(
        stderr.contains("KODA_MODEL is no longer read; use --model"),
        "{stderr}"
    );
    assert!(stderr.contains("KODA_MODE is no longer read"), "{stderr}");
}

// ── --worktree ───────────────────────────────────────────────────────────

/// `koda exec --worktree` runs in a new managed worktree, as Codex's flag
/// does: a detached checkout of the project's HEAD under Koda's config
/// folder, outside the repository, recorded with the session that ran in
/// it, and kept afterwards.
#[tokio::test]
async fn worktree_runs_the_session_in_a_new_checkout_outside_the_repository() {
    let server = stub(vec![text("Done.")], no_other_calls).await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("proj");
    std::fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        let ok = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(&repo)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hi\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "first"]);
    let config = tmp.path().join("config");

    let mut command = command(&server, &repo);
    command.env("XDG_CONFIG_HOME", &config).arg("--worktree");
    let run = finish(command).await;
    let stderr = String::from_utf8_lossy(&run.output.stderr);
    assert!(run.output.status.success(), "{stderr}");
    assert!(stderr.contains("Working in a new worktree"), "{stderr}");
    assert!(stderr.contains("The worktree stays at"), "{stderr}");

    // One checkout, under <config>/koda/worktrees/<bucket>/proj.
    let root = config.join("koda/worktrees");
    let buckets: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .collect();
    assert_eq!(buckets.len(), 1, "{buckets:?}");
    let checkout = buckets[0].path().join("proj");
    assert_eq!(
        std::fs::read_to_string(checkout.join("README.md")).unwrap(),
        "hi\n"
    );
    // The session that ran there is recorded with it.
    let started = run
        .lines
        .iter()
        .find(|line| line["method"] == "session/started")
        .expect("no session/started");
    let id = started["params"]["session"]["id"].as_str().unwrap();
    assert_eq!(
        koda_git::worktree::owner(&checkout).unwrap().as_deref(),
        Some(id)
    );
    assert_eq!(
        started["params"]["session"]["cwd"]
            .as_str()
            .map(std::path::Path::new),
        Some(checkout.canonicalize().unwrap().as_path())
    );
}

#[tokio::test]
async fn worktree_refuses_to_resume() {
    let server = stub(vec![], no_other_calls).await;
    let tmp = tempfile::tempdir().unwrap();
    let output = tokio::process::Command::new(koda_bin())
        .args(["exec", "resume", "--last", "--worktree", "hi"])
        .args(["--base-url", &server.url()])
        .arg("--project-root")
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("can't resume a session"), "{stderr}");
    assert!(stderr.contains("koda fork <id> --worktree"), "{stderr}");
}

/// `koda fork --worktree` needs the session named, as `codex fork
/// --worktree` refuses `--last` and the picker.
#[tokio::test]
async fn fork_worktree_needs_a_session_id() {
    let server = stub(vec![], no_other_calls).await;
    let tmp = tempfile::tempdir().unwrap();
    for args in [
        vec!["fork", "--last", "--worktree"],
        vec!["fork", "--worktree"],
    ] {
        let output = tokio::process::Command::new(koda_bin())
            .args(&args)
            .args(["--base-url", &server.url()])
            .arg("--project-root")
            .arg(tmp.path())
            .env("XDG_CONFIG_HOME", tmp.path())
            .output()
            .await
            .unwrap();
        assert!(!output.status.success(), "{args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("needs a session id"), "{args:?}: {stderr}");
    }
}
