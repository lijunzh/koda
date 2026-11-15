//! `koda serve --stdio` over pipes: the eight acceptance scenarios of the
//! wire protocol review, driven with JSON-RPC lines against a scripted
//! model. The host's own rules (a busy session, stale ids, an interrupt at
//! an approval) are pinned in `turns.rs`; these check that they hold over
//! the wire, and the transport's own: bad lines, EOF, a broken or slow
//! reader, and characters split across reads.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use koda_client::stdio::{ServeArgs, serve_with_config};
use koda_core::config::{KodaConfig, ProviderType};
use koda_test_utils::network::{FakeLlmServer, Request, ResponseTemplate};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::task::JoinHandle;

const WAIT: Duration = Duration::from_secs(30);

// ── The model ───────────────────────────────────────────────────────────

fn sse(chunks: &[Value]) -> ResponseTemplate {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

fn text(reply: &str) -> ResponseTemplate {
    text_in(&[reply])
}

/// A reply streamed in `pieces`, one delta each.
fn text_in(pieces: &[&str]) -> ResponseTemplate {
    let mut chunks: Vec<Value> = pieces
        .iter()
        .map(|p| json!({"choices": [{"delta": {"content": p}}]}))
        .collect();
    chunks.push(json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}));
    chunks.push(json!({"choices": [], "usage": {"prompt_tokens": 200, "completion_tokens": 20}}));
    sse(&chunks)
}

fn tool_call(id: &str, name: &str, args: Value) -> ResponseTemplate {
    sse(&[
        json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": id,
            "type": "function",
            "function": {"name": name, "arguments": args.to_string()},
        }]}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 100, "completion_tokens": 10}}),
    ])
}

fn write_file(id: &str, path: &str) -> ResponseTemplate {
    tool_call(id, "Write", json!({"file_path": path, "content": "hi"}))
}

async fn model(replies: Vec<ResponseTemplate>) -> FakeLlmServer {
    let server = FakeLlmServer::spawn().await;
    let queue = Mutex::new(VecDeque::from(replies));
    server
        .mount_fn("POST", r".*/chat/completions$", move |_| {
            queue
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| text("(no more replies)"))
        })
        .await;
    server
}

async fn chat_requests(server: &FakeLlmServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .iter()
        .filter(|r: &&Request| r.url.path().ends_with("/chat/completions"))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

/// Wait until the model has had `n` calls.
async fn model_calls(server: &FakeLlmServer, n: usize) {
    let deadline = Instant::now() + WAIT;
    while chat_requests(server).await.len() < n {
        assert!(Instant::now() < deadline, "no model call {n}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ── The client ──────────────────────────────────────────────────────────

/// A client on the other end of `koda serve`'s pipes.
struct Wire {
    /// Koda's stdin.
    input: Option<DuplexStream>,
    /// Koda's stdout.
    output: Option<BufReader<DuplexStream>>,
    served: JoinHandle<Result<(), koda_client::RequestError>>,
    next_id: i64,
    session: String,
    project: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

fn config(server: &FakeLlmServer, safe: bool) -> KodaConfig {
    let mut config = KodaConfig::default_for_testing(ProviderType::LMStudio);
    config.base_url = server.url();
    config.model = "stub".to_string();
    config.model_settings.model = "stub".to_string();
    config.max_context_tokens = 100_000;
    config.model_settings.max_context_tokens = 100_000;
    if safe {
        config.trust = koda_core::trust::TrustMode::Safe;
    }
    config
}

impl Wire {
    /// `koda serve` with `output_buffer` bytes of pipe to stdout; nothing
    /// sent yet.
    fn spawn(config: KodaConfig, output_buffer: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let (input, koda_in) = tokio::io::duplex(64 * 1024);
        let (koda_out, output) = tokio::io::duplex(output_buffer);
        let args = ServeArgs {
            config_file: koda_client::ConfigFile::in_dir(&dir.path().join("config")),
            project_root: project.clone(),
            home: dir.path().join("config"),
        };
        let served = tokio::spawn(serve_with_config(
            args,
            config,
            koda_in,
            koda_out,
            std::future::pending(),
        ));
        Self {
            input: Some(input),
            output: Some(BufReader::new(output)),
            served,
            next_id: 1,
            session: String::new(),
            project,
            _dir: dir,
        }
    }

    /// [`Wire::spawn`], initialized, with a session open.
    async fn open(config: KodaConfig) -> Self {
        let mut wire = Self::spawn(config, 64 * 1024);
        // Sent back to back: `initialize` is answered before the next
        // line is read.
        let init = wire
            .send_request(
                "initialize",
                json!({"clientInfo": {"name": "serve-test", "version": "0"}}),
            )
            .await;
        let start = wire.send_request("session/start", json!({})).await;
        wire.response(init).await.unwrap();
        let started = wire.response(start).await.unwrap();
        wire.session = started["session"]["id"].as_str().unwrap().to_string();
        wire
    }

    async fn send_line(&mut self, line: &str) {
        let input = self.input.as_mut().expect("stdin is closed");
        input.write_all(line.as_bytes()).await.unwrap();
        input.write_all(b"\n").await.unwrap();
    }

    async fn send(&mut self, message: Value) {
        self.send_line(&message.to_string()).await;
    }

    /// Send a request; its id.
    async fn send_request(&mut self, method: &str, params: Value) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        id
    }

    /// The next line Koda wrote; `None` at EOF.
    async fn next(&mut self) -> Option<Value> {
        let output = self.output.as_mut().expect("stdout is closed");
        let mut line = String::new();
        let read = tokio::time::timeout(WAIT, output.read_line(&mut line))
            .await
            .expect("Koda wrote nothing in time")
            .unwrap();
        (read > 0).then(|| serde_json::from_str(&line).unwrap())
    }

    /// Lines up to and including the first `pick` matches.
    async fn until(&mut self, pick: impl Fn(&Value) -> bool) -> Vec<Value> {
        let mut seen = Vec::new();
        loop {
            let line = self.next().await.expect("Koda's stdout ended");
            let done = pick(&line);
            seen.push(line);
            if done {
                return seen;
            }
        }
    }

    /// Lines up to and including the first `method` notification or request.
    async fn until_method(&mut self, method: &str) -> Vec<Value> {
        self.until(|l| l["method"] == method).await
    }

    /// The answer to request `id`: its result, or its error.
    async fn response(&mut self, id: i64) -> Result<Value, Value> {
        let lines = self
            .until(|l| l["id"] == id && l.get("method").is_none())
            .await;
        let answer = lines.last().unwrap();
        match answer.get("error") {
            Some(error) => Err(error.clone()),
            None => Ok(answer["result"].clone()),
        }
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        let id = self.send_request(method, params).await;
        self.response(id).await
    }

    /// Start a turn; its id.
    async fn turn(&mut self, input: &str) -> String {
        let session = self.session.clone();
        let started = self
            .call("turn/start", json!({"sessionId": session, "input": input}))
            .await
            .unwrap();
        started["turn"]["id"].as_str().unwrap().to_string()
    }

    /// The approval the turn waits on: its request id.
    async fn approval(&mut self) -> Value {
        let lines = self.until_method("item/toolCall/requestApproval").await;
        lines.last().unwrap()["id"].clone()
    }

    async fn answer(&mut self, id: &Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}))
            .await;
    }

    /// Close stdin and stdout and wait for `koda serve` to end.
    async fn finish(mut self) {
        drop(self.input.take());
        drop(self.output.take());
        tokio::time::timeout(Duration::from_secs(15), &mut self.served)
            .await
            .expect("koda serve didn't end after EOF")
            .unwrap()
            .unwrap();
    }
}

fn turn_completed(lines: &[Value]) -> &Value {
    let last = lines.last().unwrap();
    assert_eq!(last["method"], "turn/completed", "{last}");
    &last["params"]["turn"]
}

fn count(lines: &[Value], method: &str) -> usize {
    lines.iter().filter(|l| l["method"] == method).count()
}

// ── The scenarios ───────────────────────────────────────────────────────

/// A prompt waits for approval; the client replies on stdin, and the tool
/// runs once without waiting for the turn to finish.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_approval_is_answered_on_stdin_and_the_tool_runs_once() {
    let server = model(vec![write_file("call_1", "a.txt"), text("Written.")]).await;
    let mut wire = Wire::open(config(&server, true)).await;
    wire.turn("Write a file").await;

    let id = wire.approval().await;
    assert!(!wire.project.join("a.txt").exists());
    wire.answer(&id, json!({"decision": {"type": "approve"}}))
        .await;

    let lines = wire.until_method("turn/completed").await;
    assert_eq!(turn_completed(&lines)["status"], "completed");
    assert!(wire.project.join("a.txt").exists());
    let tool_items = lines
        .iter()
        .filter(|l| l["method"] == "item/completed" && l["params"]["item"]["type"] == "toolCall")
        .count();
    assert_eq!(tool_items, 1);
    assert_eq!(count(&lines, "serverRequest/resolved"), 1);
    wire.finish().await;
}

/// Cancel at an approval: the turn settles once, interrupted, and the
/// request is closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_interrupt_at_an_approval_settles_the_turn_once() {
    let server = model(vec![write_file("call_1", "a.txt")]).await;
    let mut wire = Wire::open(config(&server, true)).await;
    let turn = wire.turn("Write a file").await;
    let id = wire.approval().await;

    let session = wire.session.clone();
    wire.call(
        "turn/interrupt",
        json!({"sessionId": session, "turnId": turn}),
    )
    .await
    .unwrap();
    let mut lines = wire.until_method("turn/completed").await;
    assert_eq!(turn_completed(&lines)["status"], "interrupted");
    let resolved: Vec<_> = lines
        .iter()
        .filter(|l| l["method"] == "serverRequest/resolved")
        .map(|l| l["params"]["requestId"].clone())
        .collect();
    assert_eq!(resolved, std::slice::from_ref(&id));

    // A late answer changes nothing; no second ending follows before the
    // next request's answer.
    wire.answer(&id, json!({"decision": {"type": "approve"}}))
        .await;
    let state = wire.send_request("state/read", json!({})).await;
    lines.extend(
        wire.until(|l| l["id"] == state && l.get("method").is_none())
            .await,
    );
    assert_eq!(count(&lines, "turn/completed"), 1);
    assert!(!wire.project.join("a.txt").exists());
    wire.finish().await;
}

/// Wrong or stale ids are refused, and unblock nothing: the real approval
/// still waits for its own answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_ids_are_refused_and_unblock_nothing() {
    let server = model(vec![write_file("call_1", "a.txt"), text("Done.")]).await;
    let mut wire = Wire::open(config(&server, true)).await;
    wire.turn("Write a file").await;
    let id = wire.approval().await;

    let session = wire.session.clone();
    let error = wire
        .call(
            "turn/interrupt",
            json!({"sessionId": session, "turnId": "not-this-turn"}),
        )
        .await
        .unwrap_err();
    assert!(error["message"].is_string(), "{error}");
    let error = wire
        .call(
            "turn/start",
            json!({"sessionId": "no-such-session", "input": "hi"}),
        )
        .await
        .unwrap_err();
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("no open session"),
        "{error}"
    );
    // An answer to a request Koda never sent.
    wire.answer(&json!(9999), json!({"decision": {"type": "approve"}}))
        .await;
    wire.answer(&json!("nope"), json!({"decision": {"type": "approve"}}))
        .await;
    // Still waiting: nothing ran.
    wire.call("state/read", json!({})).await.unwrap();
    assert!(!wire.project.join("a.txt").exists());

    wire.answer(&id, json!({"decision": {"type": "approve"}}))
        .await;
    let lines = wire.until_method("turn/completed").await;
    assert_eq!(turn_completed(&lines)["status"], "completed");
    assert!(wire.project.join("a.txt").exists());
    wire.finish().await;
}

/// Another prompt while busy is refused; steering text is taken and
/// reaches the model.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_prompt_while_busy_is_refused_and_a_steer_is_taken() {
    let server = model(vec![write_file("call_1", "a.txt"), text("Done.")]).await;
    let mut wire = Wire::open(config(&server, true)).await;
    let turn = wire.turn("Write a file").await;
    let id = wire.approval().await;

    let session = wire.session.clone();
    let error = wire
        .call(
            "turn/start",
            json!({"sessionId": session, "input": "Something else"}),
        )
        .await
        .unwrap_err();
    assert_eq!(error["message"], "a turn is already running");
    let steered = wire
        .call(
            "turn/steer",
            json!({"sessionId": session, "expectedTurnId": turn,
                   "input": "STEER-MARKER", "clientMessageId": "s1"}),
        )
        .await
        .unwrap();
    assert_eq!(steered["turnId"], turn);

    wire.answer(&id, json!({"decision": {"type": "approve"}}))
        .await;
    let lines = wire.until_method("turn/completed").await;
    assert_eq!(turn_completed(&lines)["status"], "completed");
    let requests = chat_requests(&server).await;
    assert!(
        requests
            .last()
            .unwrap()
            .to_string()
            .contains("STEER-MARKER"),
        "the steer never reached the model"
    );
    assert!(
        !requests
            .iter()
            .any(|r| r.to_string().contains("Something else"))
    );
    wire.finish().await;
}

/// Bad lines, unknown methods and requests before `initialize` fail
/// explicitly, and the connection carries on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_lines_and_unknown_methods_fail_explicitly() {
    let server = model(vec![]).await;
    let mut wire = Wire::spawn(config(&server, false), 64 * 1024);

    let error = wire.call("state/read", json!({})).await.unwrap_err();
    assert_eq!(error["code"], -32600);
    assert_eq!(error["message"], "Not initialized");

    wire.send_line("{this isn't json").await;
    let line = wire.next().await.unwrap();
    assert_eq!(line["error"]["code"], -32700, "{line}");
    assert_eq!(line["id"], Value::Null);
    wire.send_line(r#"{"id": 41, "method": "state/read"}"#)
        .await;
    let line = wire.next().await.unwrap();
    assert_eq!(line["error"]["code"], -32600, "{line}");
    assert_eq!(line["id"], 41);
    // A notification gets no answer; the next request is answered.
    wire.send_line(r#"{"jsonrpc": "2.0", "method": "initialized"}"#)
        .await;

    let init = wire
        .call(
            "initialize",
            json!({"clientInfo": {"name": "serve-test", "version": "0"}}),
        )
        .await
        .unwrap();
    assert_eq!(
        init["protocolVersion"],
        koda_client::protocol::PROTOCOL_VERSION
    );
    let error = wire
        .call(
            "initialize",
            json!({"clientInfo": {"name": "serve-test", "version": "0"}}),
        )
        .await
        .unwrap_err();
    assert_eq!(error["message"], "Already initialized");
    let error = wire.call("no/such/method", json!({})).await.unwrap_err();
    assert_eq!(error["code"], -32601);
    let error = wire
        .call("turn/start", json!({"input": 5}))
        .await
        .unwrap_err();
    assert_eq!(error["code"], -32602);
    wire.call("state/read", json!({})).await.unwrap();
    wire.finish().await;
}

/// EOF with an approval pending: the turn stops, and `koda serve` ends
/// without waiting for an answer that can't come.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eof_at_an_approval_ends_the_connection() {
    let server = model(vec![write_file("call_1", "a.txt")]).await;
    let mut wire = Wire::open(config(&server, true)).await;
    wire.turn("Write a file").await;
    wire.approval().await;

    let closed = Instant::now();
    drop(wire.input.take());
    let mut rest = Vec::new();
    while let Some(line) = wire.next().await {
        rest.push(line);
    }
    tokio::time::timeout(Duration::from_secs(15), &mut wire.served)
        .await
        .expect("koda serve didn't end after EOF")
        .unwrap()
        .unwrap();
    assert!(
        closed.elapsed() < Duration::from_secs(10),
        "{:?}",
        closed.elapsed()
    );
    assert!(!wire.project.join("a.txt").exists());
    // What was queued still goes out: the request closed, the turn
    // ended once, interrupted.
    assert_eq!(count(&rest, "serverRequest/resolved"), 1, "{rest:?}");
    assert_eq!(count(&rest, "turn/completed"), 1, "{rest:?}");
    assert_eq!(turn_completed(&rest)["status"], "interrupted");
}

/// A client that stops reading stdout ends the connection when the pipe
/// breaks, though stdin stays open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_broken_stdout_ends_the_connection() {
    let server = model(vec![text("Hello.")]).await;
    let mut wire = Wire::open(config(&server, false)).await;
    drop(wire.output.take());
    // Something to write.
    wire.send_request("state/read", json!({})).await;
    tokio::time::timeout(Duration::from_secs(15), &mut wire.served)
        .await
        .expect("koda serve didn't end when stdout broke")
        .unwrap()
        .unwrap();
}

/// A slow reader loses nothing: what waited arrives in order, every delta
/// included, once it reads, as Codex's stdio keeps everything.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_reader_loses_nothing() {
    let pieces: Vec<String> = (0..600).map(|i| format!("w{i} ")).collect();
    let refs: Vec<&str> = pieces.iter().map(String::as_str).collect();
    let server = model(vec![text_in(&refs)]).await;
    // A 4 KiB pipe to stdout fills after a few dozen lines.
    let mut wire = Wire::spawn(config(&server, false), 4 * 1024);
    let init = wire
        .send_request(
            "initialize",
            json!({"clientInfo": {"name": "serve-test", "version": "0"}}),
        )
        .await;
    let start = wire.send_request("session/start", json!({})).await;
    wire.response(init).await.unwrap();
    let started = wire.response(start).await.unwrap();
    let session = started["session"]["id"].as_str().unwrap().to_string();

    // Nobody reads while the turn runs.
    wire.send_request(
        "turn/start",
        json!({"sessionId": session, "input": "Talk a lot"}),
    )
    .await;
    model_calls(&server, 1).await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    let lines = wire.until_method("turn/completed").await;
    assert_eq!(turn_completed(&lines)["status"], "completed");
    let streamed: String = lines
        .iter()
        .filter(|l| l["method"] == "item/agentMessage/delta")
        .map(|l| l["params"]["delta"].as_str().unwrap())
        .collect();
    let whole: String = pieces.concat();
    let reply = lines
        .iter()
        .find(|l| l["method"] == "item/completed" && l["params"]["item"]["type"] == "agentMessage")
        .expect("no completed reply");
    assert_eq!(reply["params"]["item"]["text"], whole);
    assert_eq!(streamed, whole, "a delta was lost");
    wire.finish().await;
}

/// A character split across two reads arrives whole.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_character_split_across_reads_arrives_whole() {
    let server = model(vec![text("Bien.")]).await;
    let mut wire = Wire::open(config(&server, false)).await;
    let session = wire.session.clone();
    let line = json!({"jsonrpc": "2.0", "id": 100, "method": "turn/start",
                      "params": {"sessionId": session, "input": "héllo 日本語"}})
    .to_string();
    let bytes = line.as_bytes();
    let cut = line.find('日').unwrap() + 1; // inside the character
    let input = wire.input.as_mut().unwrap();
    input.write_all(&bytes[..cut]).await.unwrap();
    input.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    input.write_all(&bytes[cut..]).await.unwrap();
    input.write_all(b"\n").await.unwrap();

    wire.response(100).await.unwrap();
    let lines = wire.until_method("turn/completed").await;
    assert_eq!(turn_completed(&lines)["status"], "completed");
    let requests = chat_requests(&server).await;
    assert!(
        requests[0].to_string().contains("héllo 日本語"),
        "{}",
        requests[0]
    );
    wire.finish().await;
}

/// A turn that fails after partial output ends once, failed, with every
/// started item closed and its usage.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_failing_after_partial_output_ends_once() {
    let server = model(vec![
        tool_call("a", "Bash", json!({"command": "true"})),
        ResponseTemplate::new(400).set_body_string("boom"),
    ])
    .await;
    let mut wire = Wire::open(config(&server, false)).await;
    wire.turn("Run something").await;
    let lines = wire.until_method("turn/completed").await;
    let turn = turn_completed(&lines);
    assert_eq!(turn["status"], "failed", "{turn}");
    assert!(turn["error"]["message"].is_string(), "{turn}");
    assert_eq!(turn["usage"]["modelCalls"], 1, "{turn}");
    assert_eq!(
        count(&lines, "item/started"),
        count(&lines, "item/completed"),
        "{lines:?}"
    );
    assert_eq!(count(&lines, "turn/completed"), 1);
    // The session takes the next turn.
    wire.call("state/read", json!({})).await.unwrap();
    wire.finish().await;
}
