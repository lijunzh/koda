//! Sessions and turns through the client, end to end: the host builds a
//! real session against a scripted model server, and the test drives it
//! with requests and reads what comes back on the event queue.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use koda_client::events::Outgoing;
use koda_client::protocol::initialize::ClientInfo;
use koda_client::protocol::item::Item;
use koda_client::protocol::rpc::{ErrorObject, RequestId, codes};
use koda_client::protocol::session::SessionStartParams;
use koda_client::protocol::state::TrustMode;
use koda_client::protocol::turn::{
    TurnInterruptParams, TurnStartParams, TurnStatus, TurnSteerParams,
};
use koda_client::protocol::{ServerNotification, ServerRequest, methods};
use koda_client::{InProcessClient, RequestError, StartArgs};
use koda_core::config::{KodaConfig, ProviderType};
use koda_core::db::Database;
use koda_test_utils::network::{FakeLlmServer, Request, ResponseTemplate};
use serde_json::{Value, json};

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
    sse(&[
        json!({"choices": [{"delta": {"content": reply}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 200, "completion_tokens": 20}}),
    ])
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

/// A model server that answers each chat request with the next reply.
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

/// The chat requests the model received, as JSON.
async fn chat_requests(server: &FakeLlmServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .iter()
        .filter(|r: &&Request| r.url.path().ends_with("/chat/completions"))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

// ── The client ──────────────────────────────────────────────────────────

struct Koda {
    client: InProcessClient,
    session: String,
    project: PathBuf,
    _dir: tempfile::TempDir,
}

fn config(server: &FakeLlmServer) -> KodaConfig {
    let mut config = KodaConfig::default_for_testing(ProviderType::LMStudio);
    config.base_url = server.url();
    config.model = "stub".to_string();
    config.max_context_tokens = 100_000;
    config.model_settings.max_context_tokens = 100_000;
    config
}

async fn client(config: KodaConfig, dir: &tempfile::TempDir) -> InProcessClient {
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let db = Database::init(&dir.path().join("config")).await.unwrap();
    InProcessClient::start(StartArgs {
        config,
        project_root: project,
        db,
        raw_events: false,
        client_info: ClientInfo {
            name: "turns-test".to_string(),
            version: "0".to_string(),
        },
    })
    .await
    .unwrap()
}

impl Koda {
    /// A client with one new session open.
    async fn start(config: KodaConfig) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let client = client(config, &dir).await;
        let started = client
            .request::<methods::SessionStart>(SessionStartParams::default())
            .await
            .unwrap();
        let mut koda = Self {
            client,
            session: started.session.id,
            project: dir.path().join("project"),
            _dir: dir,
        };
        koda.until("session/started").await;
        // The scripted server reports no context window, so Koda says what it
        // assumed, once the session has started.
        let warned = koda.until("warning").await;
        assert_eq!(names(&warned), "warning");
        match &warned[0] {
            Outgoing::Notification(ServerNotification::Warning(w)) => {
                assert!(
                    w.message.contains("didn't report its context window"),
                    "{w:?}"
                );
            }
            other => panic!("expected a warning, got {other:?}"),
        }
        koda
    }

    async fn turn(&self, input: &str) -> String {
        self.turn_with(input, None).await.unwrap()
    }

    async fn turn_with(
        &self,
        input: &str,
        client_message_id: Option<&str>,
    ) -> Result<String, RequestError> {
        let started = self
            .client
            .request::<methods::TurnStart>(TurnStartParams {
                session_id: self.session.clone(),
                input: input.to_string(),
                images: Vec::new(),
                client_message_id: client_message_id.map(str::to_string),
                mode: None,
            })
            .await?;
        assert_eq!(started.turn.status, TurnStatus::InProgress);
        Ok(started.turn.id)
    }

    async fn interrupt(&self, turn_id: &str) -> Result<(), RequestError> {
        self.client
            .request::<methods::TurnInterrupt>(TurnInterruptParams {
                session_id: self.session.clone(),
                turn_id: turn_id.to_string(),
            })
            .await
            .map(|_| ())
    }

    async fn steer(&self, turn_id: &str, input: &str, id: &str) -> Result<String, RequestError> {
        self.client
            .request::<methods::TurnSteer>(TurnSteerParams {
                session_id: self.session.clone(),
                expected_turn_id: turn_id.to_string(),
                input: input.to_string(),
                client_message_id: Some(id.to_string()),
            })
            .await
            .map(|r| r.turn_id)
    }

    /// Every message up to and including the first named `method`.
    async fn until(&mut self, method: &str) -> Vec<Outgoing> {
        let mut seen = Vec::new();
        loop {
            let message = tokio::time::timeout(WAIT, self.client.next_event())
                .await
                .unwrap_or_else(|_| panic!("no {method} within {WAIT:?}; saw {}", names(&seen)))
                .expect("the host stopped");
            let done = name(&message) == method;
            seen.push(message);
            if done {
                return seen;
            }
        }
    }

    /// The server request the turn is waiting on.
    async fn request(&mut self) -> ServerRequest {
        loop {
            let message = tokio::time::timeout(WAIT, self.client.next_event())
                .await
                .expect("no server request")
                .expect("the host stopped");
            if let Outgoing::Request(request) = message {
                return request;
            }
        }
    }

    async fn answer(&self, id: &RequestId, result: Value) {
        self.client
            .resolve_server_request(id.clone(), result)
            .await
            .unwrap();
    }
}

fn name(message: &Outgoing) -> &'static str {
    match message {
        Outgoing::Notification(n) => n.method(),
        Outgoing::Request(r) => r.method(),
    }
}

fn names(messages: &[Outgoing]) -> String {
    messages.iter().map(name).collect::<Vec<_>>().join(", ")
}

/// The completed items, in order.
fn completed(messages: &[Outgoing]) -> Vec<&Item> {
    messages
        .iter()
        .filter_map(|m| match m {
            Outgoing::Notification(ServerNotification::ItemCompleted(n)) => Some(&n.item),
            _ => None,
        })
        .collect()
}

fn turn_completed(messages: &[Outgoing]) -> &koda_client::protocol::turn::Turn {
    match messages.last() {
        Some(Outgoing::Notification(ServerNotification::TurnCompleted(n))) => &n.turn,
        other => panic!("expected turn/completed last, got {other:?}"),
    }
}

fn server_error(result: Result<impl std::fmt::Debug, RequestError>) -> ErrorObject {
    match result {
        Err(RequestError::Server { error, .. }) => error,
        other => panic!("expected a server error, got {other:?}"),
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_runs_from_start_to_finish() {
    let server = model(vec![text("Hello there."), text("Again.")]).await;
    let mut koda = Koda::start(config(&server)).await;

    let turn_id = koda.turn_with("Say hi", Some("m1")).await.unwrap();
    let messages = koda.until("turn/completed").await;

    // The response came first, then the turn's notifications in order.
    match &messages[0] {
        Outgoing::Notification(ServerNotification::TurnStarted(n)) => {
            assert_eq!(n.turn.id, turn_id);
            assert_eq!(n.session_id, koda.session);
        }
        other => panic!("expected turn/started first, got {other:?}"),
    }
    let items = completed(&messages);
    assert!(
        matches!(items[0], Item::UserMessage { text, client_message_ids, .. }
            if text == "Say hi" && client_message_ids == &["m1".to_string()]),
        "{items:?}"
    );
    assert!(
        matches!(items.last().unwrap(), Item::AgentMessage { text, .. } if text == "Hello there."),
        "{items:?}"
    );
    let turn = turn_completed(&messages);
    assert_eq!(turn.id, turn_id);
    assert_eq!(turn.status, TurnStatus::Completed);
    assert_eq!(turn.usage.as_ref().unwrap().model_calls, 1);

    // The session is idle again and takes the next turn, with the history.
    koda.turn("And again").await;
    let messages = koda.until("turn/completed").await;
    assert_eq!(turn_completed(&messages).status, TurnStatus::Completed);
    let requests = chat_requests(&server).await;
    assert_eq!(requests.len(), 2);
    let history = requests[1]["messages"].to_string();
    assert!(history.contains("Say hi") && history.contains("Hello there."));
    koda.client.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_interrupt_stops_the_turn_and_is_answered_once_it_has() {
    let server = model(vec![text("too late").set_delay(Duration::from_secs(60))]).await;
    let mut koda = Koda::start(config(&server)).await;
    let turn_id = koda.turn("Take your time").await;
    koda.until("turn/started").await;

    let asked = Instant::now();
    koda.interrupt(&turn_id).await.unwrap();
    assert!(
        asked.elapsed() < Duration::from_secs(10),
        "{:?}",
        asked.elapsed()
    );
    // The answer came after the turn ended: turn/completed is already queued.
    let messages = koda.until("turn/completed").await;
    assert_eq!(turn_completed(&messages).status, TurnStatus::Interrupted);

    // Once stopped, there is nothing to interrupt.
    let error = server_error(koda.interrupt(&turn_id).await);
    assert_eq!(error.message, "no active turn to interrupt");
    koda.client.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approvals_are_answered_through_the_client() {
    let server = model(vec![
        tool_call(
            "call_1",
            "Write",
            json!({"file_path": "yes.txt", "content": "hi"}),
        ),
        tool_call(
            "call_2",
            "Write",
            json!({"file_path": "no.txt", "content": "hi"}),
        ),
        text("Done."),
    ])
    .await;
    let mut config = config(&server);
    config.trust = koda_core::trust::TrustMode::Safe;
    let mut koda = Koda::start(config).await;
    koda.turn("Write two files").await;

    let first = koda.request().await;
    let ServerRequest::ToolCallApproval { id, params } = &first else {
        panic!("expected an approval, got {first:?}");
    };
    assert_eq!(params.tool, "Write");
    assert_eq!(params.session_id, koda.session);
    koda.answer(id, json!({"decision": {"type": "approve"}}))
        .await;

    let second = koda.request().await;
    let ServerRequest::ToolCallApproval { id: second_id, .. } = &second else {
        panic!("expected an approval, got {second:?}");
    };
    // Each request has its own id.
    assert_ne!(id, second_id);
    koda.answer(second_id, json!({"decision": {"type": "reject"}}))
        .await;

    let messages = koda.until("turn/completed").await;
    assert_eq!(turn_completed(&messages).status, TurnStatus::Completed);
    assert!(koda.project.join("yes.txt").exists());
    assert!(!koda.project.join("no.txt").exists());
    // Every answered request was resolved for other clients.
    let resolved = messages
        .iter()
        .filter(|m| name(m) == "serverRequest/resolved")
        .count();
    assert_eq!(resolved, 1, "{}", names(&messages));
    koda.client.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_question_gets_the_clients_answer() {
    let server = model(vec![
        tool_call("call_1", "AskUser", json!({"question": "Which file?"})),
        text("Thanks."),
    ])
    .await;
    let mut koda = Koda::start(config(&server)).await;
    koda.turn("Ask me something").await;

    let request = koda.request().await;
    let ServerRequest::UserInput { id, params } = &request else {
        panic!("expected a question, got {request:?}");
    };
    assert_eq!(params.question, "Which file?");
    koda.answer(id, json!({"answer": "notes.txt"})).await;

    let messages = koda.until("turn/completed").await;
    assert_eq!(turn_completed(&messages).status, TurnStatus::Completed);
    let requests = chat_requests(&server).await;
    assert!(requests[1]["messages"].to_string().contains("notes.txt"));
    koda.client.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_loop_cap_waits_for_the_clients_decision() {
    let server = model(vec![
        tool_call("call_1", "Read", json!({"file_path": "a.txt"})),
        text("Carried on."),
    ])
    .await;
    let mut config = config(&server);
    config.max_iterations = 1;
    let mut koda = Koda::start(config).await;
    std::fs::write(koda.project.join("a.txt"), "a\n").unwrap();
    koda.turn("Read a.txt").await;

    let request = koda.request().await;
    let ServerRequest::LoopCap { id, params } = &request else {
        panic!("expected the loop cap, got {request:?}");
    };
    assert_eq!(params.cap, 1);
    koda.answer(id, json!({"decision": "continue50"})).await;

    let messages = koda.until("turn/completed").await;
    assert_eq!(turn_completed(&messages).status, TurnStatus::Completed);
    assert_eq!(chat_requests(&server).await.len(), 2);
    koda.client.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_steer_reaches_the_model_as_a_user_message() {
    let server = model(vec![
        tool_call("call_1", "Read", json!({"file_path": "a.txt"}))
            .set_delay(Duration::from_secs(2)),
        text("Checked both."),
    ])
    .await;
    let mut koda = Koda::start(config(&server)).await;
    std::fs::write(koda.project.join("a.txt"), "a\n").unwrap();
    let turn_id = koda.turn_with("Read a.txt", Some("m1")).await.unwrap();
    koda.until("turn/started").await;

    let steered = koda.steer(&turn_id, "Also mention b", "s1").await.unwrap();
    assert_eq!(steered, turn_id);
    let messages = koda.until("turn/completed").await;
    assert_eq!(turn_completed(&messages).status, TurnStatus::Completed);

    let delivered: Vec<_> = completed(&messages)
        .into_iter()
        .filter_map(|item| match item {
            Item::UserMessage {
                text,
                client_message_ids,
                ..
            } => Some((text.as_str(), client_message_ids.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        delivered,
        [
            ("Read a.txt", vec!["m1".to_string()]),
            ("Also mention b", vec!["s1".to_string()]),
        ]
    );
    let requests = chat_requests(&server).await;
    assert!(
        requests[1]["messages"]
            .to_string()
            .contains("Also mention b")
    );
    koda.client.shutdown().await;
}

/// An interrupt while the turn waits at an approval: the open request is
/// resolved, steering text it never delivered gets no item and doesn't go
/// out with the next turn, and a second turn is refused while it runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_interrupt_at_an_approval_closes_what_the_turn_left_open() {
    let server = model(vec![
        tool_call(
            "call_1",
            "Write",
            json!({"file_path": "a.txt", "content": "hi"}),
        ),
        text("Next turn."),
    ])
    .await;
    let mut config = config(&server);
    config.trust = koda_core::trust::TrustMode::Safe;
    let mut koda = Koda::start(config).await;
    let turn_id = koda.turn("Write a file").await;

    let request = koda.request().await;
    let error = server_error(koda.turn_with("Another", None).await);
    assert_eq!(error.code, codes::INVALID_REQUEST);
    assert_eq!(error.message, "a turn is already running");
    let error = server_error(koda.steer("other-turn", "text", "s0").await);
    assert!(
        error.message.contains("expected active turn id"),
        "{error:?}"
    );
    koda.steer(&turn_id, "never delivered", "s1").await.unwrap();

    koda.interrupt(&turn_id).await.unwrap();
    let messages = koda.until("turn/completed").await;
    assert_eq!(turn_completed(&messages).status, TurnStatus::Interrupted);
    let resolved: Vec<_> = messages
        .iter()
        .filter_map(|m| match m {
            Outgoing::Notification(ServerNotification::ServerRequestResolved(n)) => {
                Some(&n.request_id)
            }
            _ => None,
        })
        .collect();
    assert_eq!(resolved, [request.id()]);
    assert!(
        !completed(&messages)
            .iter()
            .any(|i| matches!(i, Item::UserMessage { text, .. } if text == "never delivered")),
        "{}",
        names(&messages)
    );
    // A late answer to the closed request is ignored.
    koda.answer(request.id(), json!({"decision": {"type": "approve"}}))
        .await;

    koda.turn("Go on").await;
    let messages = koda.until("turn/completed").await;
    assert_eq!(turn_completed(&messages).status, TurnStatus::Completed);
    assert!(!koda.project.join("a.txt").exists());
    let requests = chat_requests(&server).await;
    assert!(
        !requests
            .last()
            .unwrap()
            .to_string()
            .contains("never delivered")
    );
    koda.client.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_without_a_running_turn_or_session_are_refused() {
    let server = model(vec![]).await;
    let koda = Koda::start(config(&server)).await;
    let error = server_error(koda.interrupt("t").await);
    assert_eq!(error.message, "no active turn to interrupt");
    let error = server_error(koda.steer("t", "text", "s1").await);
    assert_eq!(error.message, "no active turn to steer");
    let error = server_error(
        koda.client
            .request::<methods::TurnStart>(TurnStartParams {
                session_id: "no-such-session".to_string(),
                input: "hi".to_string(),
                images: Vec::new(),
                client_message_id: None,
                mode: Some(TrustMode::Safe),
            })
            .await,
    );
    assert!(error.message.contains("no open session"), "{error:?}");
    koda.client.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_stops_a_turn_that_waits_on_the_client() {
    let server = model(vec![tool_call(
        "call_1",
        "Write",
        json!({"file_path": "a.txt", "content": "hi"}),
    )])
    .await;
    let mut config = config(&server);
    config.trust = koda_core::trust::TrustMode::Safe;
    let mut koda = Koda::start(config).await;
    koda.turn("Write a file").await;
    koda.request().await;

    let asked = Instant::now();
    koda.client.shutdown().await;
    assert!(
        asked.elapsed() < Duration::from_secs(8),
        "{:?}",
        asked.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resumed_session_keeps_its_history() {
    let server = model(vec![text("Noted."), text("It was 42.")]).await;
    let dir = tempfile::tempdir().unwrap();

    let mut first = client(config(&server), &dir).await;
    let session = first
        .request::<methods::SessionStart>(SessionStartParams::default())
        .await
        .unwrap()
        .session
        .id;
    first
        .request::<methods::TurnStart>(TurnStartParams {
            session_id: session.clone(),
            input: "Remember 42".to_string(),
            images: Vec::new(),
            client_message_id: None,
            mode: None,
        })
        .await
        .unwrap();
    loop {
        let message = tokio::time::timeout(WAIT, first.next_event())
            .await
            .unwrap()
            .unwrap();
        if name(&message) == "turn/completed" {
            break;
        }
    }
    // One client can't open the same session twice.
    let error = server_error(
        first
            .request::<methods::SessionStart>(SessionStartParams {
                session_id: Some(session.clone()),
                ..Default::default()
            })
            .await,
    );
    assert!(error.message.contains("already open"), "{error:?}");
    first.shutdown().await;

    let second = client(config(&server), &dir).await;
    let resumed = second
        .request::<methods::SessionStart>(SessionStartParams {
            session_id: Some(session.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(resumed.session.id, session);
    let mut koda = Koda {
        client: second,
        session,
        project: dir.path().join("project"),
        _dir: dir,
    };
    koda.turn("What was the number?").await;
    koda.until("turn/completed").await;
    let requests = chat_requests(&server).await;
    assert!(requests[1]["messages"].to_string().contains("Remember 42"));

    let error = server_error(
        koda.client
            .request::<methods::SessionStart>(SessionStartParams {
                session_id: Some("no-such-session".to_string()),
                ..Default::default()
            })
            .await,
    );
    assert_eq!(error.code, codes::INVALID_PARAMS);
    koda.client.shutdown().await;
}
