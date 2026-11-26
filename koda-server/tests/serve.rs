//! `koda serve --stdio` over pipes: the eight acceptance scenarios of the
//! wire protocol review, driven with JSON-RPC lines against a scripted
//! model. The host's own rules (a busy session, stale ids, an interrupt at
//! an approval) are pinned in `turns.rs`; these check that they hold over
//! the wire, and the transport's own: bad lines, EOF, a broken or slow
//! reader, and characters split across reads.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use koda_core::config::{KodaConfig, ProviderType};
use koda_server::stdio::{ServeArgs, serve_with_config};
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
    served: JoinHandle<Result<(), koda_server::in_process::RequestError>>,
    next_id: i64,
    /// Lines read while waiting for a request's answer, not yet taken:
    /// an answer travels apart from the session's messages, so they can
    /// come before it and still belong to what the test reads next.
    pending: std::collections::VecDeque<Value>,
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
            config_file: koda_server::ConfigFile::in_dir(&dir.path().join("config")),
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
            pending: std::collections::VecDeque::new(),
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
        self.try_send_line(line).await.unwrap();
    }

    /// [`Wire::send_line`], for a connection that may have ended already.
    async fn try_send_line(&mut self, line: &str) -> std::io::Result<()> {
        let input = self.input.as_mut().expect("stdin is closed");
        input.write_all(line.as_bytes()).await?;
        input.write_all(b"\n").await
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

    /// The next line Koda wrote, those set aside first; `None` at EOF.
    async fn next(&mut self) -> Option<Value> {
        if let Some(line) = self.pending.pop_front() {
            return Some(line);
        }
        self.read().await
    }

    /// The next line from Koda's stdout; `None` at EOF.
    async fn read(&mut self) -> Option<Value> {
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

    /// Lines up to and including both the answer to request `id` and the
    /// first `method` message, whichever order they come in.
    async fn until_both(&mut self, id: i64, method: &str) -> Vec<Value> {
        let (mut answered, mut seen_method) = (false, false);
        let mut seen = Vec::new();
        while !(answered && seen_method) {
            let line = self.next().await.expect("Koda's stdout ended");
            answered |= line["id"] == id && line.get("method").is_none();
            seen_method |= line["method"] == method;
            seen.push(line);
        }
        seen
    }

    /// Lines up to and including the first `method` notification or request.
    async fn until_method(&mut self, method: &str) -> Vec<Value> {
        self.until(|l| l["method"] == method).await
    }

    /// The answer to request `id`: its result, or its error. Lines before it
    /// are set aside for the next read, not dropped.
    async fn response(&mut self, id: i64) -> Result<Value, Value> {
        let is_answer = |l: &Value| l["id"] == id && l.get("method").is_none();
        let answer = match self.pending.iter().position(is_answer) {
            Some(at) => self.pending.remove(at).unwrap(),
            None => loop {
                let line = self.read().await.expect("Koda's stdout ended");
                if is_answer(&line) {
                    break line;
                }
                self.pending.push_back(line);
            },
        };
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
            .call(
                "turn/start",
                json!({"sessionId": session, "input": [{"type": "text", "text": input}]}),
            )
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

/// The turn in the `turn/completed` among `lines`: not always the last
/// line, since a request's answer travels apart from the turn's messages.
fn turn_completed(lines: &[Value]) -> &Value {
    let done = lines
        .iter()
        .find(|l| l["method"] == "turn/completed")
        .unwrap_or_else(|| panic!("no turn/completed in {lines:?}"));
    &done["params"]["turn"]
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
    let interrupt = wire
        .send_request(
            "turn/interrupt",
            json!({"sessionId": session, "turnId": turn}),
        )
        .await;
    // The answer comes once the turn has stopped, but on its own channel,
    // so it may arrive before or after the turn's last messages: keep them
    // all.
    let mut lines = wire.until_both(interrupt, "turn/completed").await;
    let answer = lines
        .iter()
        .find(|l| l["id"] == interrupt && l.get("method").is_none())
        .unwrap();
    assert!(answer.get("error").is_none(), "{answer}");
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
            json!({"sessionId": "no-such-session", "input": [{"type": "text", "text": "hi"}]}),
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
            json!({"sessionId": session, "input": [{"type": "text", "text": "Something else"}]}),
        )
        .await
        .unwrap_err();
    assert_eq!(error["message"], "a turn is already running");
    let steered = wire
        .call(
            "turn/steer",
            json!({"sessionId": session, "expectedTurnId": turn,
                   "input": [{"type": "text", "text": "STEER-MARKER"}], "clientUserMessageId": "s1"}),
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
        koda_server::protocol::PROTOCOL_VERSION
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
    // Something to write. The connection may have ended already: the
    // notifications after `session/start`'s answer can meet the broken
    // stdout first, and then stdin is gone too.
    let request =
        json!({"jsonrpc": "2.0", "id": wire.next_id, "method": "state/read", "params": {}});
    if let Err(e) = wire.try_send_line(&request.to_string()).await {
        assert_eq!(e.kind(), std::io::ErrorKind::BrokenPipe, "{e}");
    }
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
        json!({"sessionId": session, "input": [{"type": "text", "text": "Talk a lot"}]}),
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
                      "params": {"sessionId": session, "input": [{"type": "text", "text": "héllo 日本語"}]}})
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

// ── Goals ───────────────────────────────────────────────────────────────

impl Wire {
    /// Set the session's goal; the goal as answered.
    async fn set_goal(&mut self, params: Value) -> Value {
        let mut params = params;
        params["sessionId"] = json!(self.session.clone());
        self.call("session/goal/set", params).await.unwrap()["goal"].clone()
    }

    async fn goal(&mut self) -> Value {
        let session = self.session.clone();
        self.call("session/goal/get", json!({"sessionId": session}))
            .await
            .unwrap()["goal"]
            .clone()
    }
}

/// The last `session/goal/updated` among `lines`: the goal it reports.
fn goal_updated(lines: &[Value]) -> &Value {
    &lines
        .iter()
        .rev()
        .find(|l| l["method"] == "session/goal/updated")
        .unwrap_or_else(|| panic!("no session/goal/updated in {lines:?}"))["params"]["goal"]
}

/// Setting a goal on an idle session starts a turn on it, and Koda goes on
/// turn after turn until the model marks it complete, then stops.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_goal_goes_on_until_the_model_completes_it() {
    let server = model(vec![
        text("Made some progress."),
        tool_call("call_1", "UpdateGoal", json!({"status": "complete"})),
        text("Done: the goal is complete."),
    ])
    .await;
    let mut wire = Wire::open(config(&server, false)).await;
    let goal = wire
        .set_goal(json!({"objective": "Ship the <release>"}))
        .await;
    assert_eq!(goal["status"], "active");

    // Two turns, both Koda's own: no turn/start was sent.
    let first = wire.until_method("turn/completed").await;
    assert_eq!(count(&first, "turn/started"), 1);
    assert_eq!(goal_updated(&first)["status"], "active", "{first:?}");
    let second = wire.until_method("turn/completed").await;
    assert_eq!(count(&second, "turn/started"), 1);
    let lines = wire.until_method("session/goal/updated").await;
    let done = goal_updated(&lines);
    assert_eq!(done["status"], "complete", "{done}");
    assert!(done["tokensUsed"].as_i64().unwrap() > 0, "{done}");

    // Each turn began with Codex's continuation, the objective as data.
    let requests = chat_requests(&server).await;
    let first_call = requests[0].to_string();
    assert!(first_call.contains("Continue working toward the active thread goal"));
    assert!(first_call.contains("<external_goal_objective>\\nShip the &lt;release&gt;"));
    assert!(
        first_call.contains("\"name\":\"UpdateGoal\""),
        "the tools are offered"
    );
    // Koda's continuation is the model's to read, not a message of the user's.
    assert!(
        !first
            .iter()
            .any(|l| l.to_string().contains("Continue working toward")),
        "{first:?}"
    );

    // Complete: no other turn starts.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(chat_requests(&server).await.len(), 3);
    assert_eq!(wire.goal().await["status"], "complete");
    wire.finish().await;
}

/// A goal turn that fails blocks the goal, so a failing model doesn't loop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_goal_turn_blocks_the_goal() {
    let server = model(vec![ResponseTemplate::new(400).set_body_string("boom")]).await;
    let mut wire = Wire::open(config(&server, false)).await;
    wire.set_goal(json!({"objective": "Ship it"})).await;
    let lines = wire
        .until(|l| {
            l["method"] == "session/goal/updated" && l["params"]["goal"]["status"] == "blocked"
        })
        .await;
    assert_eq!(turn_completed(&lines)["status"], "failed");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(chat_requests(&server).await.len(), 1);
    wire.finish().await;
}

/// An interrupt stops the turn, not the goal, and hands the session back:
/// no turn starts on the goal until the user's next turn has ended, as in
/// Claude Code. Pausing the goal during a turn lets it end, and no other
/// starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_interrupt_waits_for_the_user_and_a_pause_stops_the_goal() {
    let server = model(vec![
        write_file("call_1", "a.txt"),
        text("Noted."),
        write_file("call_2", "b.txt"),
    ])
    .await;
    let mut wire = Wire::open(config(&server, true)).await;
    let session = wire.session.clone();
    wire.set_goal(json!({"objective": "Write the files"})).await;

    let started = wire.until_method("turn/started").await;
    let turn = started.last().unwrap()["params"]["turn"]["id"].clone();
    wire.approval().await;
    wire.call(
        "turn/interrupt",
        json!({"sessionId": session, "turnId": turn}),
    )
    .await
    .unwrap();
    let ended = wire.until_method("turn/completed").await;
    assert_eq!(turn_completed(&ended)["status"], "interrupted");

    // The goal stays active and waits.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(chat_requests(&server).await.len(), 1, "no turn by itself");
    assert_eq!(wire.goal().await["status"], "active");

    // The user's turn runs, and the goal goes on once it ends.
    let mine = wire.turn("Carry on").await;
    let lines = wire.until_method("turn/completed").await;
    assert_eq!(turn_completed(&lines)["id"], mine.as_str());
    let started = wire.until_method("turn/started").await;
    let next = started.last().unwrap()["params"]["turn"]["id"].clone();
    assert_ne!(next, mine.as_str());
    wire.approval().await;
    let paused = wire.set_goal(json!({"status": "paused"})).await;
    assert_eq!(paused["status"], "paused");
    wire.call(
        "turn/interrupt",
        json!({"sessionId": session, "turnId": next}),
    )
    .await
    .unwrap();
    wire.until_method("turn/completed").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(chat_requests(&server).await.len(), 3, "no fourth turn");
    assert_eq!(wire.goal().await["status"], "paused");
    wire.finish().await;
}

/// After an interrupt, `/goal resume` goes on with the goal at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resuming_after_an_interrupt_goes_on_with_the_goal() {
    let server = model(vec![
        write_file("call_1", "a.txt"),
        tool_call("call_2", "UpdateGoal", json!({"status": "complete"})),
        text("Done."),
    ])
    .await;
    let mut wire = Wire::open(config(&server, true)).await;
    let session = wire.session.clone();
    wire.set_goal(json!({"objective": "Write the files"})).await;
    let started = wire.until_method("turn/started").await;
    let turn = started.last().unwrap()["params"]["turn"]["id"].clone();
    wire.approval().await;
    wire.call(
        "turn/interrupt",
        json!({"sessionId": session, "turnId": turn}),
    )
    .await
    .unwrap();
    wire.until_method("turn/completed").await;

    wire.set_goal(json!({"status": "active"})).await;
    let lines = wire
        .until(|l| {
            l["method"] == "session/goal/updated" && l["params"]["goal"]["status"] == "complete"
        })
        .await;
    assert_eq!(count(&lines, "turn/started"), 1);
    wire.finish().await;
}

/// Reaching the goal's budget tells the running turn to wrap up, and marks
/// the goal budget-limited once the turn ends; no other turn starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reaching_the_budget_wraps_up_the_turn_and_stops_the_goal() {
    let server = model(vec![
        tool_call("call_1", "GetGoal", json!({})),
        text("Wrapping up."),
    ])
    .await;
    let mut wire = Wire::open(config(&server, false)).await;
    wire.set_goal(json!({"objective": "Ship it", "tokenBudget": 100}))
        .await;
    let lines = wire
        .until(|l| {
            l["method"] == "session/goal/updated"
                && l["params"]["goal"]["status"] == "budgetLimited"
        })
        .await;
    assert_eq!(count(&lines, "turn/started"), 1);
    let requests = chat_requests(&server).await;
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]
            .to_string()
            .contains("The active thread goal has reached its token budget"),
        "the turn wasn't told"
    );
    assert!(
        !requests[0]
            .to_string()
            .contains("has reached its token budget")
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(chat_requests(&server).await.len(), 2);
    wire.finish().await;
}

/// The model creates the goal during the user's turn, when asked to; Koda
/// goes on with it once that turn ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_goal_the_model_creates_goes_on_after_the_turn() {
    let server = model(vec![
        tool_call(
            "call_1",
            "CreateGoal",
            json!({"objective": "Fix the tests"}),
        ),
        text("I'll keep at it."),
        tool_call("call_2", "UpdateGoal", json!({"status": "complete"})),
        text("All fixed."),
    ])
    .await;
    let mut wire = Wire::open(config(&server, false)).await;
    wire.turn("Make it your goal to fix the tests").await;
    let lines = wire
        .until(|l| {
            l["method"] == "session/goal/updated" && l["params"]["goal"]["status"] == "complete"
        })
        .await;
    assert_eq!(
        count(&lines, "turn/started"),
        2,
        "the user's turn, then Koda's"
    );
    // The model's own change was reported during its turn.
    let created = lines
        .iter()
        .find(|l| l["method"] == "session/goal/updated")
        .unwrap();
    assert_eq!(created["params"]["goal"]["objective"], "Fix the tests");
    assert!(created["params"]["turnId"].is_string());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(chat_requests(&server).await.len(), 4);
    wire.finish().await;
}

// ── The goal evaluator (`goal_judge = "evaluator"`) ─────────────────────

fn judged(config: KodaConfig) -> KodaConfig {
    KodaConfig {
        goal_judge: koda_config::GoalJudge::Evaluator,
        ..config
    }
}

/// With the evaluator, a check after each turn decides: not yet sends its
/// reason into the next turn, met completes the goal. The check is the
/// turn's own request with the question after it, tools kept but barred,
/// and the working model has no `UpdateGoal`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_evaluator_decides_when_a_goal_is_met() {
    let server = model(vec![
        text("Made some progress."),
        text(r#"{"ok": false, "reason": "The tests haven't been run.", "impossible": false}"#),
        text("Ran the tests: all pass."),
        text(r#"Checking. {"ok": true, "reason": "The tests pass."}"#),
    ])
    .await;
    let mut wire = Wire::open(judged(config(&server, false))).await;
    wire.set_goal(json!({"objective": "Make the tests pass"}))
        .await;

    let lines = wire.until_method("session/goal/judged").await;
    let first = &lines.last().unwrap()["params"];
    assert_eq!(first["verdict"], "notYet", "{first}");
    assert_eq!(first["reason"], "The tests haven't been run.");
    let lines = wire
        .until(|l| {
            l["method"] == "session/goal/updated" && l["params"]["goal"]["status"] == "complete"
        })
        .await;
    let met = lines
        .iter()
        .find(|l| l["method"] == "session/goal/judged")
        .expect("the second check");
    assert_eq!(met["params"]["verdict"], "met");
    tokio::time::sleep(Duration::from_millis(300)).await;

    let requests = chat_requests(&server).await;
    assert_eq!(requests.len(), 4, "two turns, two checks, nothing after");
    let (turn, check) = (&requests[0], &requests[1]);
    let tools = |r: &Value| -> Vec<String> {
        r["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap().to_string())
            .collect()
    };
    assert!(!tools(turn).contains(&"UpdateGoal".to_string()));
    assert!(tools(turn).contains(&"GetGoal".to_string()));
    // The check reads as the turn: the same tools and the same messages,
    // then the reply and the question.
    assert_eq!(tools(check), tools(turn));
    assert_eq!(check["tool_choice"], "none");
    assert!(turn.get("tool_choice").is_none());
    let sent = turn["messages"].as_array().unwrap();
    let asked = check["messages"].as_array().unwrap();
    assert_eq!(&asked[..sent.len()], &sent[..], "the turn's prefix");
    assert_eq!(asked.len(), sent.len() + 2);
    assert_eq!(asked[sent.len()]["content"], "Made some progress.");
    assert!(
        asked[sent.len() + 1]["content"]
            .as_str()
            .unwrap()
            .contains("<goal_condition>\nMake the tests pass\n</goal_condition>")
    );
    // The next turn starts with what the check said remains, and without
    // Codex's completion audit.
    let next = requests[2]["messages"].to_string();
    assert!(next.contains("<goal_check>\\nThe tests haven't been run.\\n</goal_check>"));
    assert!(!next.contains("Completion audit"));
    // The checks' tokens count toward the goal.
    let goal = wire.goal().await;
    assert_eq!(goal["tokensUsed"], 4 * 220, "{goal}");
    wire.finish().await;
}

/// A goal the check finds impossible is blocked, and nothing more runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_goal_the_evaluator_finds_impossible_is_blocked() {
    let server = model(vec![
        text("There's no such service to call."),
        text(r#"{"ok": false, "reason": "The service doesn't exist.", "impossible": true}"#),
    ])
    .await;
    let mut wire = Wire::open(judged(config(&server, false))).await;
    wire.set_goal(json!({"objective": "Call the service"}))
        .await;
    let lines = wire
        .until(|l| {
            l["method"] == "session/goal/updated" && l["params"]["goal"]["status"] == "blocked"
        })
        .await;
    assert!(lines.iter().any(|l| l["params"]["verdict"] == "impossible"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(chat_requests(&server).await.len(), 2);
    wire.finish().await;
}

/// An answer the check can't read counts as not yet: the goal goes on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreadable_check_counts_as_not_yet() {
    let server = model(vec![
        text("Working."),
        text("I think it's probably fine."),
        text("Done now."),
        text(r#"{"ok": true, "reason": "Done."}"#),
    ])
    .await;
    let mut wire = Wire::open(judged(config(&server, false))).await;
    wire.set_goal(json!({"objective": "Finish"})).await;
    wire.until(|l| {
        l["method"] == "session/goal/updated" && l["params"]["goal"]["status"] == "complete"
    })
    .await;
    let requests = chat_requests(&server).await;
    assert_eq!(requests.len(), 4);
    assert!(!requests[2]["messages"].to_string().contains("<goal_check>"));
    wire.finish().await;
}

// ── Background commands ─────────────────────────────────────────────────

fn background(id: &str, command: &str) -> ResponseTemplate {
    tool_call(id, "Bash", json!({"command": command, "background": true}))
}

/// A background command that exits wakes the idle session, as Claude Code
/// tells Claude: Koda starts a turn whose first step tells the model the
/// command, its exit code and its last lines, and clients hear of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_background_command_that_exits_wakes_the_session() {
    let server = model(vec![
        background("a", "sleep 0.5; echo all 12 tests passed; exit 1"),
        text("Started the tests."),
        text("The tests finished."),
    ])
    .await;
    let mut wire = Wire::open(config(&server, false)).await;
    let started = wire.turn("Run the tests in the background").await;
    let first = wire.until_method("turn/completed").await;
    assert_eq!(turn_completed(&first)["id"], started.as_str());

    // Koda's own turn, with nothing sent to start it.
    let lines = wire.until_method("turn/completed").await;
    assert_eq!(count(&lines, "turn/started"), 1);
    let exited = lines
        .iter()
        .find(|l| l["method"] == "session/backgroundTerminals/exited")
        .unwrap_or_else(|| panic!("no exit notice in {lines:?}"));
    let data = &exited["params"]["data"][0];
    assert_eq!(data["exitCode"], 1, "{exited}");
    assert!(data["command"].as_str().unwrap().starts_with("sleep 0.5"));

    let requests = chat_requests(&server).await;
    assert_eq!(requests.len(), 3);
    let result = requests[1]["messages"].to_string();
    assert!(
        result.contains("You'll be told when it exits"),
        "no polling: {result}"
    );
    assert!(
        !result.contains("Time limit"),
        "a session someone attends has none: {result}"
    );
    let told = requests[2]["messages"].to_string();
    assert!(told.contains("<background_command_exited"), "{told}");
    assert!(told.contains("Result: exit code 1"), "{told}");
    assert!(told.contains("all 12 tests passed"), "{told}");
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(chat_requests(&server).await.len(), 3, "told once");
    wire.finish().await;
}

/// A goal waits while its background command runs, as Claude Code's does:
/// no turn starts on it until the command exits, then the exit's turn and
/// the goal's next one follow.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_goal_waits_for_its_background_command() {
    let server = model(vec![
        background("a", "sleep 1"),
        text("Waiting for the build."),
        text("The build is done."),
        tool_call("b", "UpdateGoal", json!({"status": "complete"})),
        text("Goal complete."),
    ])
    .await;
    let mut wire = Wire::open(config(&server, false)).await;
    wire.set_goal(json!({"objective": "Build it"})).await;
    wire.until_method("turn/completed").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(chat_requests(&server).await.len(), 2, "the goal waits");

    let lines = wire
        .until(|l| {
            l["method"] == "session/goal/updated" && l["params"]["goal"]["status"] == "complete"
        })
        .await;
    assert!(
        lines
            .iter()
            .any(|l| l["method"] == "session/backgroundTerminals/exited")
    );
    let requests = chat_requests(&server).await;
    assert_eq!(requests.len(), 5);
    let woken = requests[2]["messages"].to_string();
    assert!(woken.contains("<background_command_exited"));
    let next = requests[3]["messages"].to_string();
    assert!(next.contains("Continue working toward the active thread goal"));
    wire.finish().await;
}

/// A background command that keeps a goal waiting past its check-in time
/// gets a check-in, at most three until the user's next turn, each later
/// one after twice the wait.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_goal_checks_in_on_a_command_that_keeps_it_waiting() {
    let server = model(vec![
        background("a", "sleep 30"),
        text("Waiting."),
        text("Still running; waiting."),
        text("Still running; waiting."),
        text("Still running; waiting."),
    ])
    .await;
    let mut config = config(&server, false);
    config.goal_checkin = Some(Duration::from_millis(200));
    let mut wire = Wire::open(config).await;
    wire.set_goal(json!({"objective": "Ship it"})).await;
    model_calls(&server, 5).await;
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let requests = chat_requests(&server).await;
    assert_eq!(requests.len(), 5, "three check-ins, then none");
    let checkin = |n: usize| -> String {
        let messages = requests[n]["messages"].as_array().unwrap();
        messages.last().unwrap()["content"].to_string()
    };
    assert!(checkin(2).contains("Check-in: background commands have kept this goal waiting"));
    assert!(checkin(2).contains("sleep 30 (running for"));
    assert!(checkin(2).contains("Continue working toward the active thread goal"));
    assert!(!checkin(3).contains("last check-in"));
    assert!(checkin(4).contains("This is the last check-in"));
    wire.finish().await;
}

/// Under the evaluator, no check runs while a background command runs, as
/// Claude Code defers it: the turn the exit starts is checked when it ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_evaluator_waits_for_background_work() {
    let server = model(vec![
        background("a", "sleep 0.5"),
        text("Waiting for the tests."),
        text("The tests pass."),
        text(r#"{"ok": true, "reason": "The tests pass."}"#),
    ])
    .await;
    let mut wire = Wire::open(judged(config(&server, false))).await;
    wire.set_goal(json!({"objective": "Make the tests pass"}))
        .await;
    wire.until(|l| {
        l["method"] == "session/goal/updated" && l["params"]["goal"]["status"] == "complete"
    })
    .await;
    let requests = chat_requests(&server).await;
    assert_eq!(requests.len(), 4, "no check while the command ran");
    assert!(
        requests[2]["messages"]
            .to_string()
            .contains("<background_command_exited")
    );
    assert_eq!(requests[3]["tool_choice"], "none", "the check");
    wire.finish().await;
}

/// A client asking for the goal between its turns doesn't stop it: the
/// question is answered first, then the goal's next turn starts, as it
/// would have without it. `koda exec` asks after every turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn asking_for_the_goal_between_turns_doesnt_stop_it() {
    let server = model(vec![
        text("Working on it."),
        tool_call("b", "UpdateGoal", json!({"status": "complete"})),
        text("Goal complete."),
    ])
    .await;
    let mut wire = Wire::open(config(&server, false)).await;
    wire.set_goal(json!({"objective": "Build it"})).await;
    wire.until_method("turn/completed").await;
    assert_eq!(wire.goal().await["status"], "active");

    wire.until(|l| {
        l["method"] == "session/goal/updated" && l["params"]["goal"]["status"] == "complete"
    })
    .await;
    assert_eq!(chat_requests(&server).await.len(), 3);
    wire.finish().await;
}
