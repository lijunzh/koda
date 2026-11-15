//! Sessions and turns through the client, end to end: the host builds a
//! real session against a scripted model server, and the test drives it
//! with requests and reads what comes back on the event queue.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use koda_client::events::Outgoing;
use koda_client::protocol::agent::{AgentListParams, AgentSource};
use koda_client::protocol::background::{
    BackgroundTerminalsCleanParams, BackgroundTerminalsListParams,
    BackgroundTerminalsTerminateParams,
};
use koda_client::protocol::initialize::ClientInfo;
use koda_client::protocol::item::{CompactionStatus, CompactionTrigger, Item, TurnItem};
use koda_client::protocol::memory::{MemoryScope, SessionMemoryAddParams, SessionMemoryReadParams};
use koda_client::protocol::model::{ModelListParams, ModelProviderListParams};
use koda_client::protocol::rpc::{ErrorObject, RequestId, codes};
use koda_client::protocol::session::{
    SessionCompactStartParams, SessionDeleteParams, SessionListParams, SessionSettings,
    SessionSettingsUpdateParams, SessionSettingsUpdateResponse, SessionStartParams,
    SessionUnsubscribeParams, SessionUnsubscribeStatus,
};
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
    /// What `session/start` answered.
    opened: koda_client::protocol::session::Session,
    project: PathBuf,
    _dir: tempfile::TempDir,
}

fn config(server: &FakeLlmServer) -> KodaConfig {
    let mut config = KodaConfig::default_for_testing(ProviderType::LMStudio);
    config.base_url = server.url();
    config.model = "stub".to_string();
    config.model_settings.model = "stub".to_string();
    config.max_context_tokens = 100_000;
    config.model_settings.max_context_tokens = 100_000;
    config
}

async fn client(config: KodaConfig, dir: &tempfile::TempDir) -> InProcessClient {
    client_with(config, dir, None).await
}

async fn client_with(
    config: KodaConfig,
    dir: &tempfile::TempDir,
    agent: Option<std::sync::Arc<koda_core::agent::KodaAgent>>,
) -> InProcessClient {
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let db = Database::init(&dir.path().join("config")).await.unwrap();
    InProcessClient::start(StartArgs {
        config,
        config_file: koda_client::ConfigFile::in_dir(&dir.path().join("config")),
        project_root: project,
        db,
        client_info: ClientInfo {
            name: "turns-test".to_string(),
            version: "0".to_string(),
        },
        agent,
    })
    .await
    .unwrap()
}

impl Koda {
    /// A client with one new session open.
    async fn start(config: KodaConfig) -> Self {
        Self::start_in(config, tempfile::tempdir().unwrap()).await
    }

    /// [`Koda::start`] in `dir`, which may hold files the session reads.
    async fn start_in(config: KodaConfig, dir: tempfile::TempDir) -> Self {
        let client = client(config, &dir).await;
        let started = client
            .request::<methods::SessionStart>(SessionStartParams::default())
            .await
            .unwrap();
        let mut koda = Self {
            client,
            session: started.session.id.clone(),
            opened: started.session,
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

    async fn read_memory(
        &self,
    ) -> Result<koda_client::protocol::memory::SessionMemoryReadResponse, RequestError> {
        self.client
            .request::<methods::SessionMemoryRead>(SessionMemoryReadParams {
                session_id: self.session.clone(),
            })
            .await
    }

    async fn add_memory(
        &self,
        scope: MemoryScope,
        text: &str,
    ) -> Result<koda_client::protocol::memory::SessionMemoryAddResponse, RequestError> {
        self.client
            .request::<methods::SessionMemoryAdd>(SessionMemoryAddParams {
                session_id: self.session.clone(),
                scope,
                text: text.to_string(),
            })
            .await
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
        opened: resumed.session,
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

// ── In-process handles and closing a session (TUI, PR 5c) ───────────────

impl Koda {
    async fn unsubscribe(&self) -> SessionUnsubscribeStatus {
        self.client
            .request::<methods::SessionUnsubscribe>(SessionUnsubscribeParams {
                session_id: self.session.clone(),
            })
            .await
            .unwrap()
            .status
    }

    /// Whether the host still has the session open, as a request that
    /// needs it tells: one for a closed session is refused.
    async fn is_open(&self) -> bool {
        use koda_client::protocol::background::BackgroundTerminalsListParams;
        self.client
            .request::<methods::SessionBackgroundTerminalsList>(BackgroundTerminalsListParams {
                session_id: self.session.clone(),
            })
            .await
            .is_ok()
    }
}

/// `session/unsubscribe` closes a session: its requests are refused, a turn
/// on it is refused, and it opens again by id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsubscribe_closes_a_session_and_it_opens_again() {
    let server = model(vec![text("Hello.")]).await;
    let mut koda = Koda::start(config(&server)).await;
    koda.turn("hi").await;
    koda.until("turn/completed").await;
    assert!(koda.is_open().await);

    assert_eq!(
        koda.unsubscribe().await,
        SessionUnsubscribeStatus::Unsubscribed
    );
    assert!(!koda.is_open().await);
    let refused = server_error(koda.turn_with("again", None).await);
    assert!(refused.message.contains("no open session"), "{refused:?}");
    assert_eq!(
        koda.unsubscribe().await,
        SessionUnsubscribeStatus::NotLoaded
    );

    let reopened = koda
        .client
        .request::<methods::SessionStart>(SessionStartParams {
            session_id: Some(koda.session.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(reopened.session.id, koda.session);
}

/// Unsubscribing stops the session's running turn, which still ends with
/// its `turn/completed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsubscribe_stops_the_running_turn() {
    let slow = text("Too late.").set_delay(Duration::from_secs(20));
    let server = model(vec![slow]).await;
    let mut koda = Koda::start(config(&server)).await;
    koda.turn("hi").await;
    koda.until("turn/started").await;

    let asked = Instant::now();
    assert_eq!(
        koda.unsubscribe().await,
        SessionUnsubscribeStatus::Unsubscribed
    );
    let seen = koda.until("turn/completed").await;
    assert_eq!(turn_completed(&seen).status, TurnStatus::Interrupted);
    assert!(
        asked.elapsed() < Duration::from_secs(10),
        "{:?}",
        asked.elapsed()
    );
}

/// `configure_session` changes what the next turn runs with: here the
/// server it goes to.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configure_moves_the_next_turn_to_another_server() {
    let first = model(vec![]).await;
    let second = model(vec![text("From the second server.")]).await;
    let mut koda = Koda::start(config(&first)).await;

    koda.client
        .configure_session(&koda.session, config(&second))
        .await
        .unwrap();
    koda.turn("where are you").await;
    koda.until("turn/completed").await;
    assert_eq!(chat_requests(&second).await.len(), 1);
    assert!(chat_requests(&first).await.is_empty());

    let error = koda
        .client
        .configure_session("no-such-session", config(&second))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("no open session"), "{error}");
}

/// A front end that builds its own agent has every session use it: the
/// model gets that agent's system prompt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shared_agent_is_what_sessions_run_with() {
    let server = model(vec![text("Hello.")]).await;
    let dir = tempfile::tempdir().unwrap();
    let config = config(&server);
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let mut agent = koda_core::agent::KodaAgent::new(&config, project, &["/zebra-command"])
        .await
        .unwrap();
    agent.rebuild_system_prompt(&config, &["/zebra-command"]);
    let client = client_with(config, &dir, Some(std::sync::Arc::new(agent))).await;
    let opened = client
        .request::<methods::SessionStart>(SessionStartParams::default())
        .await
        .unwrap()
        .session;
    let mut koda = Koda {
        client,
        session: opened.id.clone(),
        opened,
        project: dir.path().join("project"),
        _dir: dir,
    };
    koda.turn("hi").await;
    koda.until("turn/completed").await;
    let sent = chat_requests(&server).await;
    assert!(
        sent[0].to_string().contains("/zebra-command"),
        "not the shared agent's prompt: {}",
        sent[0]
    );
}

// ── Sessions (PR 6a) ─────────────────────────────────────────────────────

impl Koda {
    async fn list(&self) -> Vec<koda_client::protocol::session::Session> {
        self.client
            .request::<methods::SessionList>(SessionListParams::default())
            .await
            .unwrap()
            .data
    }
}

/// `session/start` describes the session as stored and as running;
/// `session/list` gives stored sessions, newest first, without what an open
/// one runs with.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sessions_are_listed_newest_first_with_their_first_prompt() {
    let server = model(vec![text("One."), text("Two.")]).await;
    let mut koda = Koda::start(config(&server)).await;
    koda.turn("the first session's prompt").await;
    koda.until("turn/completed").await;
    let first = koda.session.clone();

    let second = koda
        .client
        .request::<methods::SessionStart>(SessionStartParams::default())
        .await
        .unwrap()
        .session;
    assert_eq!(second.preview, "", "no prompt yet");
    assert_eq!(second.model.as_deref(), Some("stub"));
    assert!(second.tools.as_ref().is_some_and(|t| !t.is_empty()));
    assert!(second.created_at > 1_700_000_000, "{second:?}");

    let listed = koda.list().await;
    let ids: Vec<&str> = listed.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, [second.id.as_str(), first.as_str()]);
    let stored = &listed[1];
    assert_eq!(stored.preview, "the first session's prompt");
    assert_eq!(stored.name.as_deref(), Some("the first session's prompt"));
    assert!(stored.updated_at >= stored.created_at);
    assert!(
        stored.cwd.as_ref().is_some_and(|c| c.ends_with("project")),
        "{stored:?}"
    );
    assert!(
        stored.model.is_none() && stored.tools.is_none(),
        "{stored:?}"
    );

    let one = koda
        .client
        .request::<methods::SessionList>(SessionListParams {
            limit: Some(1),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(one.data.len(), 1);
}

/// `session/delete` closes an open session first, stopping its turn, then
/// deletes it and says so with `session/deleted`, as Codex's `thread/delete`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_an_open_session_stops_its_turn_and_deletes_it() {
    let slow = text("Too late.").set_delay(Duration::from_secs(20));
    let server = model(vec![slow]).await;
    let mut koda = Koda::start(config(&server)).await;
    koda.turn("hi").await;
    koda.until("turn/started").await;

    let asked = Instant::now();
    koda.client
        .request::<methods::SessionDelete>(SessionDeleteParams {
            session_id: koda.session.clone(),
        })
        .await
        .unwrap();
    assert!(
        asked.elapsed() < Duration::from_secs(10),
        "{:?}",
        asked.elapsed()
    );
    // The turn ends before the session is deleted.
    let seen = koda.until("turn/completed").await;
    assert_eq!(turn_completed(&seen).status, TurnStatus::Interrupted);
    koda.until("session/deleted").await;
    assert!(koda.list().await.is_empty());
    assert!(!koda.is_open().await);
    let refused = server_error(koda.turn_with("again", None).await);
    assert!(refused.message.contains("no open session"), "{refused:?}");

    let gone = server_error(
        koda.client
            .request::<methods::SessionDelete>(SessionDeleteParams {
                session_id: koda.session.clone(),
            })
            .await,
    );
    assert!(gone.message.contains("no session"), "{gone:?}");
}

// ── Compaction ──────────────────────────────────────────────────────────

/// A reply to a streamed call, with the prompt size the provider reports.
fn text_using(reply: &str, prompt_tokens: u64) -> ResponseTemplate {
    sse(&[
        json!({"choices": [{"delta": {"content": reply}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": prompt_tokens, "completion_tokens": 20}}),
    ])
}

/// The summary call's reply: compaction doesn't stream.
fn summary(text: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "application/json")
        .set_body_string(
            json!({
                "id": "c", "object": "chat.completion", "model": "stub",
                "choices": [{"index": 0, "finish_reason": "stop",
                             "message": {"role": "assistant", "content": text}}],
                "usage": {"prompt_tokens": 300, "completion_tokens": 30},
            })
            .to_string(),
        )
}

impl Koda {
    async fn compact(&self) -> Result<(), RequestError> {
        self.client
            .request::<methods::SessionCompactStart>(SessionCompactStartParams {
                session_id: self.session.clone(),
            })
            .await
            .map(|_| ())
    }

    /// The id of the next turn to start.
    async fn next_turn(&mut self) -> String {
        match self.until("turn/started").await.pop() {
            Some(Outgoing::Notification(ServerNotification::TurnStarted(started))) => {
                started.turn.id
            }
            other => panic!("expected turn/started, got {other:?}"),
        }
    }
}

/// The turn's compaction item, as it completed.
fn compaction(messages: &[Outgoing]) -> (CompactionTrigger, CompactionStatus, Option<String>) {
    let found = completed(messages).into_iter().find_map(|item| match item {
        Item::ContextCompaction {
            trigger,
            status,
            reason,
            ..
        } => Some((*trigger, *status, reason.clone())),
        _ => None,
    });
    found.unwrap_or_else(|| panic!("no compaction in {}", names(messages)))
}

/// `session/compact/start` stops the running turn, then compacts as a turn
/// of its own, which can be stopped but not steered, as in Codex.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_compaction_replaces_the_running_turn_and_runs_as_its_own() {
    let server = model(vec![
        text("One."),
        text("Two."),
        text("Three."),
        text("Too late.").set_delay(Duration::from_secs(20)),
        summary("<summary>SUMMARY-MARKER covers the turns.</summary>")
            .set_delay(Duration::from_secs(2)),
        text("Four."),
    ])
    .await;
    let mut koda = Koda::start(config(&server)).await;
    for prompt in ["first", "second", "third"] {
        koda.turn(prompt).await;
        koda.until("turn/completed").await;
    }
    let running = koda.turn("slow").await;
    koda.until("turn/started").await;
    // Its call is out, so the summary call gets the summary.
    let deadline = Instant::now() + WAIT;
    while chat_requests(&server).await.len() < 4 {
        assert!(Instant::now() < deadline, "the slow turn's call never came");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let asked = Instant::now();
    koda.compact().await.unwrap();
    assert!(asked.elapsed() < Duration::from_secs(5), "answered at once");
    let replaced = koda.until("turn/completed").await;
    assert_eq!(turn_completed(&replaced).status, TurnStatus::Interrupted);

    let compacting = koda.next_turn().await;
    assert_ne!(compacting, running);
    let refused = server_error(koda.steer(&compacting, "and also", "s1").await);
    assert_eq!(refused.message, "cannot steer a compact turn");
    let again = server_error(koda.compact().await);
    assert_eq!(again.message, "a compaction is already running");

    let seen = koda.until("turn/completed").await;
    assert_eq!(
        turn_completed(&seen).status,
        TurnStatus::Completed,
        "{:?}",
        turn_completed(&seen).error
    );
    let (trigger, status, _) = compaction(&seen);
    assert_eq!(trigger, CompactionTrigger::Manual);
    assert_eq!(status, CompactionStatus::Completed);

    koda.turn("after").await;
    koda.until("turn/completed").await;
    let sent = chat_requests(&server).await.last().unwrap().to_string();
    assert!(sent.contains("SUMMARY-MARKER"), "no summary: {sent}");
    assert!(
        !sent.contains("\"first\""),
        "the oldest turn survived: {sent}"
    );
    koda.client.shutdown().await;
}

/// A compaction with too little to compact completes its turn and says why.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_compaction_with_nothing_to_do_completes() {
    let server = model(vec![text("One.")]).await;
    let mut koda = Koda::start(config(&server)).await;
    koda.turn("only").await;
    koda.until("turn/completed").await;

    koda.compact().await.unwrap();
    let seen = koda.until("turn/completed").await;
    assert_eq!(turn_completed(&seen).status, TurnStatus::Completed);
    let (trigger, status, reason) = compaction(&seen);
    assert_eq!(trigger, CompactionTrigger::Manual);
    assert_eq!(status, CompactionStatus::Skipped);
    assert_eq!(reason.as_deref(), Some("too_short"));
    assert_eq!(chat_requests(&server).await.len(), 1, "no summary call");
    koda.client.shutdown().await;
}

/// With `post_turn_compact_percent` set, a final reply that leaves the
/// context past it compacts inside the turn, before `turn/completed`, as
/// Codex's `model_post_turn_compact_threshold_percent` does. Off, it doesn't.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_context_compacts_after_the_reply_when_turned_on() {
    for percent in [0, 50] {
        let server = model(vec![
            text("One."),
            text("Two."),
            text("Three."),
            text_using("Big.", 60_000),
            summary("<summary>SUMMARY-MARKER covers the turns.</summary>"),
        ])
        .await;
        let mut config = config(&server);
        config.post_turn_compact_percent = percent;
        let mut koda = Koda::start(config).await;
        for prompt in ["first", "second", "third", "fourth"] {
            koda.turn(prompt).await;
            let seen = koda.until("turn/completed").await;
            assert_eq!(turn_completed(&seen).status, TurnStatus::Completed);
            if prompt != "fourth" {
                continue;
            }
            let compacted = completed(&seen)
                .iter()
                .any(|item| matches!(item, Item::ContextCompaction { .. }));
            if percent == 0 {
                assert!(!compacted, "compacted with the setting off");
                assert_eq!(chat_requests(&server).await.len(), 4);
                continue;
            }
            let (trigger, status, _) = compaction(&seen);
            assert_eq!(trigger, CompactionTrigger::PostTurn);
            assert_eq!(status, CompactionStatus::Completed);
            // The reply came first; the compaction ran before the turn ended.
            let reply = seen
                .iter()
                .position(|m| matches!(m, Outgoing::Notification(ServerNotification::ItemCompleted(n)) if matches!(n.item, Item::AgentMessage { .. })))
                .unwrap();
            let compaction_at = seen
                .iter()
                .position(|m| matches!(m, Outgoing::Notification(ServerNotification::ItemStarted(n)) if matches!(n.item, Item::ContextCompaction { .. })))
                .unwrap();
            assert!(reply < compaction_at, "{}", names(&seen));
            assert_eq!(chat_requests(&server).await.len(), 5);
        }
        koda.client.shutdown().await;
    }
}

// ── Reasoning across models ─────────────────────────────────────────────

/// A local model's reasoning goes back to it on later turns, and to no other
/// model: a strict server (OpenAI rejects a message field it doesn't know)
/// would refuse the request, and another model would read it as its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reasoning_goes_back_only_to_the_model_that_wrote_it() {
    let server = FakeLlmServer::spawn().await;
    server
        .mount_fn("POST", r".*/chat/completions$", |request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let replayed = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m.get("reasoning_content").is_some() || m.get("reasoning").is_some());
            if body["model"] == "strict" && replayed {
                return ResponseTemplate::new(400).set_body_json(json!({"error": {
                    "message": "Unrecognized request argument supplied: reasoning_content",
                    "type": "invalid_request_error",
                }}));
            }
            sse(&[
                json!({"choices": [{"delta": {"reasoning_content": "LOCAL-THOUGHTS"}}]}),
                json!({"choices": [{"delta": {"content": "Done."}}]}),
                json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
                json!({"choices": [], "usage": {"prompt_tokens": 200, "completion_tokens": 20}}),
            ])
        })
        .await;
    let mut koda = Koda::start(config(&server)).await;
    let using = |model: &str| {
        let mut config = config(&server);
        config.model = model.to_string();
        config
    };

    for (model, prompt) in [("stub", "first"), ("stub", "second"), ("strict", "third")] {
        koda.client
            .configure_session(&koda.session, using(model))
            .await
            .unwrap();
        koda.turn(prompt).await;
        let seen = koda.until("turn/completed").await;
        assert_eq!(
            turn_completed(&seen).status,
            TurnStatus::Completed,
            "{model}: {:?}",
            turn_completed(&seen).error
        );
    }
    let sent = chat_requests(&server).await;
    assert!(
        sent[1].to_string().contains("LOCAL-THOUGHTS"),
        "the same model gets its reasoning back: {}",
        sent[1]
    );
    assert!(
        !sent[2].to_string().contains("LOCAL-THOUGHTS"),
        "another model doesn't: {}",
        sent[2]
    );
    assert!(
        sent[2].to_string().contains("Done."),
        "the replies still go"
    );
    koda.client.shutdown().await;
}

// ── Settings ────────────────────────────────────────────────────────────

impl Koda {
    async fn update(
        &self,
        model: Option<&str>,
        mode: Option<TrustMode>,
    ) -> Result<SessionSettingsUpdateResponse, RequestError> {
        self.client
            .request::<methods::SessionSettingsUpdate>(SessionSettingsUpdateParams {
                session_id: self.session.clone(),
                model: model.map(str::to_string),
                effort: None,
                mode,
            })
            .await
    }

    /// The settings `session/settings/updated` reported.
    async fn settings_updated(&mut self) -> SessionSettings {
        match self.until("session/settings/updated").await.pop() {
            Some(Outgoing::Notification(ServerNotification::SessionSettingsUpdated(n))) => {
                assert_eq!(n.session_id, self.session);
                n.settings
            }
            other => panic!("expected session/settings/updated, got {other:?}"),
        }
    }
}

/// The model each chat request named.
async fn models_asked(server: &FakeLlmServer) -> Vec<String> {
    chat_requests(server)
        .await
        .iter()
        .map(|r| r["model"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// `session/settings/update` changes the model for the next turn, and says
/// so with `session/settings/updated`; the mode is saved with the session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_settings_update_reaches_the_next_turn() {
    let server = model(vec![text("One."), text("Two.")]).await;
    let mut koda = Koda::start(config(&server)).await;
    koda.turn("first").await;
    koda.until("turn/completed").await;

    koda.update(Some("other-model"), Some(TrustMode::Plan))
        .await
        .unwrap();
    let settings = koda.settings_updated().await;
    assert_eq!(settings.model, "other-model");
    assert_eq!(settings.model_provider, "lm-studio");
    assert_eq!(settings.mode, TrustMode::Plan);

    koda.turn("second").await;
    koda.until("turn/completed").await;
    assert_eq!(models_asked(&server).await, ["stub", "other-model"]);

    let db = Database::init(&koda._dir.path().join("config"))
        .await
        .unwrap();
    use koda_core::session_store::SessionStore;
    let stored = db.get_session(&koda.session).await.unwrap().unwrap();
    assert_eq!(stored.model.as_deref(), Some("other-model"));
    assert_eq!(stored.mode.as_deref(), Some("plan"));
    koda.client.shutdown().await;
}

/// A turn keeps the settings it started with; an update sent meanwhile is
/// answered at once and applied once the turn has ended.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_running_turn_keeps_its_settings() {
    let server = model(vec![
        text("Slow.").set_delay(Duration::from_millis(800)),
        text("Next."),
    ])
    .await;
    let mut koda = Koda::start(config(&server)).await;
    koda.turn("slow").await;
    koda.until("turn/started").await;
    koda.update(Some("later-model"), None).await.unwrap();
    let seen = koda.until("turn/completed").await;
    assert!(
        !names(&seen).contains("session/settings/updated"),
        "applied before the turn ended: {}",
        names(&seen)
    );
    assert_eq!(koda.settings_updated().await.model, "later-model");

    koda.turn("next").await;
    koda.until("turn/completed").await;
    assert_eq!(models_asked(&server).await, ["stub", "later-model"]);
    koda.client.shutdown().await;
}

/// A session's provider doesn't change: another provider's model is
/// refused, and so is an unknown provider at the start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_keeps_its_provider() {
    let server = model(vec![]).await;
    let koda = Koda::start(config(&server)).await;
    let refused = server_error(koda.update(Some("gemini-flash"), None).await);
    assert!(
        refused.message.contains("start a new session"),
        "{refused:?}"
    );
    let unknown = server_error(
        koda.client
            .request::<methods::SessionStart>(SessionStartParams {
                model_provider: Some("nope".into()),
                ..Default::default()
            })
            .await,
    );
    assert!(unknown.message.contains("unknown provider"), "{unknown:?}");
    koda.client.shutdown().await;
}

/// `session/start` takes a provider, endpoint and model; a resumed session
/// reopens on its own unless the client names others, as Codex's
/// `thread/resume` does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resumed_session_reopens_on_its_own_model() {
    let first = model(vec![text("Here.")]).await;
    let second = model(vec![text("There."), text("Back.")]).await;
    let mut koda = Koda::start(config(&first)).await;
    let started = koda
        .client
        .request::<methods::SessionStart>(SessionStartParams {
            model_provider: Some("lm-studio".into()),
            base_url: Some(second.url()),
            model: Some("second-model".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let other = started.session.id;
    assert_eq!(started.session.model.as_deref(), Some("second-model"));
    koda.session = other.clone();
    koda.turn("hello").await;
    koda.until("turn/completed").await;
    koda.unsubscribe().await;

    let resumed = koda
        .client
        .request::<methods::SessionStart>(SessionStartParams {
            session_id: Some(other),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(resumed.session.model.as_deref(), Some("second-model"));
    koda.turn("again").await;
    koda.until("turn/completed").await;
    assert_eq!(
        models_asked(&second).await,
        ["second-model", "second-model"]
    );
    assert!(chat_requests(&first).await.is_empty());
    koda.client.shutdown().await;
}

/// `model/list` gives a provider's models; `modelProvider/list` the
/// providers, with whether a key is set, never the key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn models_and_providers_are_listed() {
    let server = model(vec![]).await;
    server
        .mount_fn("GET", r".*/models$", |_| {
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": [{"id": "m-one"}, {"id": "m-two"}]}))
        })
        .await;
    let koda = Koda::start(config(&server)).await;
    let models = koda
        .client
        .request::<methods::ModelList>(ModelListParams {
            model_provider: Some("lm-studio".into()),
            base_url: Some(server.url()),
        })
        .await
        .unwrap();
    let ids: Vec<&str> = models.data.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["m-one", "m-two"]);

    let providers = koda
        .client
        .request::<methods::ModelProviderList>(ModelProviderListParams {})
        .await
        .unwrap();
    assert_eq!(providers.default_provider_id, "lm-studio");
    let lm = providers.data.iter().find(|p| p.id == "lm-studio").unwrap();
    assert_eq!(lm.name, "LM Studio");
    assert!(!lm.requires_api_key);
    assert!(providers.data.iter().any(|p| p.id == "anthropic"));
    let raw = serde_json::to_string(&providers).unwrap();
    assert!(!raw.contains("sk-"), "a key leaked: {raw}");
    koda.client.shutdown().await;
}

// ── Config and keys ─────────────────────────────────────────────────────

impl Koda {
    async fn write_config(
        &self,
        edits: &[(&str, Value)],
    ) -> Result<koda_client::protocol::config::ConfigWriteResponse, RequestError> {
        use koda_client::protocol::config::{ConfigBatchWriteParams, ConfigEdit, MergeStrategy};
        self.client
            .request::<methods::ConfigBatchWrite>(ConfigBatchWriteParams {
                edits: edits
                    .iter()
                    .map(|(key, value)| ConfigEdit {
                        key_path: key.to_string(),
                        value: value.clone(),
                        merge_strategy: MergeStrategy::Replace,
                    })
                    .collect(),
                expected_version: None,
            })
            .await
    }

    async fn new_session(
        &self,
        params: SessionStartParams,
    ) -> koda_client::protocol::session::Session {
        self.client
            .request::<methods::SessionStart>(params)
            .await
            .unwrap()
            .session
    }
}

/// A new session that names nothing starts from config.toml's choice, as a
/// Codex thread starts from `config.toml`; what a session names wins, and a
/// named provider other than the saved one takes nothing from it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_session_starts_from_the_saved_choice() {
    let first = model(vec![]).await;
    let saved = model(vec![]).await;
    let koda = Koda::start(config(&first)).await;
    let written = koda
        .write_config(&[
            ("model_provider", json!("lm-studio")),
            ("model_providers.lm-studio.base_url", json!(saved.url())),
            ("model", json!("saved-model")),
        ])
        .await
        .unwrap();
    assert_eq!(written.config.model.as_deref(), Some("saved-model"));
    let read = koda
        .client
        .request::<methods::ConfigRead>(Default::default())
        .await
        .unwrap();
    assert_eq!(read.config, written.config);

    let plain = koda.new_session(SessionStartParams::default()).await;
    assert_eq!(plain.model.as_deref(), Some("saved-model"));

    // A named model keeps the saved provider and endpoint.
    let named = koda
        .new_session(SessionStartParams {
            model: Some("named-model".into()),
            ..Default::default()
        })
        .await;
    assert_eq!(named.model.as_deref(), Some("named-model"));

    // Another provider takes nothing from the saved choice: its own
    // default model, not the saved one. (A local one, at the stub, so
    // nothing leaves the machine.)
    let other = koda
        .new_session(SessionStartParams {
            model_provider: Some("ollama".into()),
            base_url: Some(first.url()),
            ..Default::default()
        })
        .await;
    assert_eq!(other.provider.as_deref(), Some("ollama"));
    assert_ne!(other.model.as_deref(), Some("saved-model"));
    koda.client.shutdown().await;
}

/// `config/batchWrite` takes only Koda's keys, as strings or `null`, and
/// keeps an alias as an alias.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_writes_take_koda_keys_and_keep_aliases() {
    let server = model(vec![]).await;
    let koda = Koda::start(config(&server)).await;
    let unknown = server_error(koda.write_config(&[("theme", json!("dark"))]).await);
    assert!(
        unknown.message.contains("unknown config key"),
        "{unknown:?}"
    );
    let number = server_error(koda.write_config(&[("model", json!(5))]).await);
    assert!(number.message.contains("string or null"), "{number:?}");

    let written = koda
        .write_config(&[
            ("model_provider", json!("anthropic")),
            ("model", json!("claude-opus")),
        ])
        .await
        .unwrap();
    assert_eq!(written.config.model.as_deref(), Some("claude-opus"));
    assert_eq!(written.config.model_provider.as_deref(), Some("anthropic"));
    koda.client.shutdown().await;
}

/// A key set through the host reaches its sessions and shows in
/// `modelProvider/list`, never in any response; removing it forgets it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_api_key_is_stored_and_never_returned() {
    use koda_client::protocol::config::{
        ModelProviderApiKeyRemoveParams, ModelProviderApiKeySetParams,
    };
    let server = model(vec![]).await;
    let koda = Koda::start(config(&server)).await;
    // Groq, so no other test's provider sees a key appear mid-run.
    let secret = "gsk-test-not-a-real-key-4242";
    let set = koda
        .client
        .request::<methods::ModelProviderApiKeySet>(ModelProviderApiKeySetParams {
            provider_id: "groq".into(),
            api_key: secret.into(),
        })
        .await
        .unwrap();
    assert!(!serde_json::to_string(&set).unwrap().contains(secret));
    let has_key = |list: &koda_client::protocol::model::ModelProviderListResponse| {
        list.data
            .iter()
            .find(|p| p.id == "groq")
            .unwrap()
            .has_api_key
    };
    let list = koda
        .client
        .request::<methods::ModelProviderList>(ModelProviderListParams {})
        .await
        .unwrap();
    assert!(has_key(&list));
    assert!(!serde_json::to_string(&list).unwrap().contains(secret));
    let db = Database::init(&koda._dir.path().join("config"))
        .await
        .unwrap();
    assert_eq!(
        koda_core::keystore::get_key(&db, "GROQ_API_KEY")
            .await
            .unwrap()
            .as_deref(),
        Some(secret)
    );

    koda.client
        .request::<methods::ModelProviderApiKeyRemove>(ModelProviderApiKeyRemoveParams {
            provider_id: "groq".into(),
        })
        .await
        .unwrap();
    let list = koda
        .client
        .request::<methods::ModelProviderList>(ModelProviderListParams {})
        .await
        .unwrap();
    assert!(!has_key(&list));
    let local = server_error(
        koda.client
            .request::<methods::ModelProviderApiKeySet>(ModelProviderApiKeySetParams {
                provider_id: "lm-studio".into(),
                api_key: "x".into(),
            })
            .await,
    );
    assert!(local.message.contains("takes no API key"), "{local:?}");
    koda.client.shutdown().await;
}

/// `auto-detect` resolves on the host to the local server's loaded model.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_detect_takes_the_servers_loaded_model() {
    let server = model(vec![]).await;
    server
        .mount_fn("GET", r".*/models$", |_| {
            ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "loaded-model"}]}))
        })
        .await;
    let mut config = config(&server);
    config.model = "auto-detect".into();
    config.model_settings.model = "auto-detect".into();
    let koda = Koda::start(config).await;
    let started = koda.new_session(SessionStartParams::default()).await;
    assert_eq!(started.model.as_deref(), Some("loaded-model"));
    koda.client.shutdown().await;
}

// ── MCP servers ─────────────────────────────────────────────────────────

use koda_client::protocol::mcp::{
    ListMcpServerStatusParams, McpServerConnectionStatus, McpServerStartupState, McpServerStatus,
    McpServerStatusUpdatedNotification,
};

/// A stdio MCP server with one tool, `lookup`, written to `dir`. It waits
/// `delay` seconds before it answers, and with a `gate` it exits at once
/// unless that file exists.
fn mcp_server(dir: &std::path::Path, delay: &str, gate: Option<&std::path::Path>) -> String {
    let gate = gate
        .map(|g| format!("[ -e '{}' ] || exit 1\n", g.display()))
        .unwrap_or_default();
    let script = format!(
        r#"{gate}sleep {delay}
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"protocolVersion":"2024-11-05","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"notes","version":"0.1.0"}}}}}}\n' "$id" ;;
    *'"method":"tools/list"'*)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"tools":[{{"name":"lookup","description":"Look a note up","inputSchema":{{"type":"object","properties":{{}}}}}}]}}}}\n' "$id" ;;
    *'"method":"tools/call"'*)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"content":[{{"type":"text","text":"lookup-ran"}}]}}}}\n' "$id" ;;
  esac
done
"#
    );
    let path = dir.join(format!("mcp-{}.sh", uuid_like()));
    std::fs::write(&path, script).unwrap();
    path.display().to_string()
}

/// A name no other script in the directory has.
fn uuid_like() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed).to_string()
}

/// Whether the model was offered the tool `name` in request `n` (from 0).
async fn offered(server: &FakeLlmServer, n: usize, name: &str) -> bool {
    let requests = chat_requests(server).await;
    requests[n]["tools"]
        .as_array()
        .is_some_and(|tools| tools.iter().any(|t| t["function"]["name"] == name))
}

impl Koda {
    async fn mcp_status(&self, session: Option<&str>) -> Vec<McpServerStatus> {
        self.client
            .request::<methods::McpServerStatusList>(ListMcpServerStatusParams {
                session_id: session.map(str::to_string),
                server_name: None,
            })
            .await
            .unwrap()
            .data
    }

    async fn reload_mcp(&self) {
        self.client
            .request::<methods::McpServerRefresh>(Default::default())
            .await
            .unwrap();
    }

    /// Open a session and make it the one turns go to.
    async fn switch_to_new_session(&mut self) {
        self.session = self.new_session(SessionStartParams::default()).await.id;
    }

    /// The current session's next startup report for `server` with `status`.
    async fn until_mcp(
        &mut self,
        server: &str,
        status: McpServerStartupState,
    ) -> McpServerStatusUpdatedNotification {
        loop {
            let message = tokio::time::timeout(WAIT, self.client.next_event())
                .await
                .unwrap_or_else(|_| panic!("{server} not {status:?} within {WAIT:?}"))
                .expect("the host stopped");
            if let Outgoing::Notification(ServerNotification::McpServerStatusUpdated(update)) =
                message
                && update.session_id == self.session
                && update.name == server
                && update.status == status
            {
                return update;
            }
        }
    }

    /// The session's servers, once `done` holds for them.
    async fn mcp_status_when(
        &self,
        done: impl Fn(&[McpServerStatus]) -> bool,
    ) -> Vec<McpServerStatus> {
        let deadline = Instant::now() + WAIT;
        loop {
            let listed = self.mcp_status(Some(&self.session)).await;
            if done(&listed) {
                return listed;
            }
            assert!(Instant::now() < deadline, "never as expected: {listed:?}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// A stored server is listed as not started; a session starts it, says so
/// as it goes, and lists it connected with its tools.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_starts_the_stored_servers_and_reports_them() {
    let server = model(vec![]).await;
    let mut koda = Koda::start(config(&server)).await;
    let script = mcp_server(koda._dir.path(), "0", None);
    koda.write_config(&[(
        "mcp_servers.notes",
        json!({"command": "sh", "args": [script]}),
    )])
    .await
    .unwrap();

    let stored = koda.mcp_status(None).await;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].name, "notes");
    assert_eq!(
        stored[0].runtime_status,
        McpServerConnectionStatus::NotStarted
    );
    // The session opened before has none until a reload.
    assert!(koda.mcp_status(Some(&koda.session)).await.is_empty());

    koda.switch_to_new_session().await;
    koda.until_mcp("notes", McpServerStartupState::Starting)
        .await;
    let ready = koda.until_mcp("notes", McpServerStartupState::Ready).await;
    assert_eq!(ready.session_id, koda.session);
    let listed = koda.mcp_status(Some(&koda.session)).await;
    assert_eq!(
        listed[0].runtime_status,
        McpServerConnectionStatus::Connected
    );
    let tool = &listed[0].tools["lookup"];
    assert_eq!(tool.description.as_deref(), Some("Look a note up"));
    koda.client.shutdown().await;
}

/// Opening a session doesn't wait for a slow server, but its first turn
/// does, so the server's tools reach the model, as Codex's tool listing
/// waits for servers still starting.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_server_holds_the_first_turn_not_the_session_start() {
    let server = model(vec![text("Done.")]).await;
    let mut koda = Koda::start(config(&server)).await;
    let script = mcp_server(koda._dir.path(), "2", None);
    koda.write_config(&[(
        "mcp_servers.notes",
        json!({"command": "sh", "args": [script]}),
    )])
    .await
    .unwrap();

    let opening = Instant::now();
    koda.switch_to_new_session().await;
    assert!(
        opening.elapsed() < Duration::from_secs(2),
        "the start waited"
    );
    let listed = koda.mcp_status(Some(&koda.session)).await;
    assert_eq!(
        listed[0].runtime_status,
        McpServerConnectionStatus::Starting
    );

    koda.turn("use your tools").await;
    koda.until("turn/completed").await;
    assert!(offered(&server, 0, "notes__lookup").await);
    koda.client.shutdown().await;
}

/// A required server that fails stops the session from opening, with the
/// reason; one that isn't required fails on its own and is listed so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_required_server_that_fails_stops_the_session_from_opening() {
    let server = model(vec![]).await;
    let mut koda = Koda::start(config(&server)).await;
    let broken = json!({"command": "sh", "args": ["-c", "exit 1"], "required": true});
    koda.write_config(&[("mcp_servers.broken", broken)])
        .await
        .unwrap();
    let refused = server_error(
        koda.client
            .request::<methods::SessionStart>(SessionStartParams::default())
            .await,
    );
    assert!(
        refused.message.contains("required MCP server") && refused.message.contains("broken"),
        "{refused:?}"
    );

    koda.write_config(&[(
        "mcp_servers.broken",
        json!({"command": "sh", "args": ["-c", "exit 1"]}),
    )])
    .await
    .unwrap();
    koda.switch_to_new_session().await;
    let failed = koda
        .until_mcp("broken", McpServerStartupState::Failed)
        .await;
    assert!(failed.error.is_some());
    let listed = koda.mcp_status(Some(&koda.session)).await;
    assert_eq!(listed[0].runtime_status, McpServerConnectionStatus::Failed);
    assert!(listed[0].tools_error.is_some());
    koda.client.shutdown().await;
}

/// A server added while a session is open reaches it on a reload, and one
/// removed leaves it, each by the next turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reload_brings_an_open_session_in_line_with_the_stored_servers() {
    let server = model(vec![text("One."), text("Two.")]).await;
    let mut koda = Koda::start(config(&server)).await;
    let script = mcp_server(koda._dir.path(), "0", None);
    koda.write_config(&[(
        "mcp_servers.notes",
        json!({"command": "sh", "args": [script]}),
    )])
    .await
    .unwrap();
    koda.reload_mcp().await;
    koda.until_mcp("notes", McpServerStartupState::Ready).await;
    koda.turn("first").await;
    koda.until("turn/completed").await;
    assert!(offered(&server, 0, "notes__lookup").await);

    koda.write_config(&[("mcp_servers.notes", Value::Null)])
        .await
        .unwrap();
    koda.reload_mcp().await;
    koda.mcp_status_when(|listed| listed.is_empty()).await;
    koda.turn("second").await;
    koda.until("turn/completed").await;
    assert!(!offered(&server, 1, "notes__lookup").await);
    koda.client.shutdown().await;
}

/// A reload during a turn waits for the turn to end: the running turn
/// keeps its servers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reload_during_a_turn_waits_for_it_to_end() {
    let server = model(vec![
        tool_call(
            "call_1",
            "Write",
            json!({"file_path": "a.txt", "content": "hi"}),
        ),
        text("Done."),
    ])
    .await;
    let mut config = config(&server);
    config.trust = koda_core::trust::TrustMode::Safe;
    let mut koda = Koda::start(config).await;
    koda.turn("write a file").await;
    let ServerRequest::ToolCallApproval { id, .. } = koda.request().await else {
        panic!("expected an approval");
    };

    let script = mcp_server(koda._dir.path(), "0", None);
    koda.write_config(&[(
        "mcp_servers.notes",
        json!({"command": "sh", "args": [script]}),
    )])
    .await
    .unwrap();
    koda.reload_mcp().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(koda.mcp_status(Some(&koda.session)).await.is_empty());

    koda.answer(&id, json!({"decision": {"type": "reject"}}))
        .await;
    koda.until("turn/completed").await;
    koda.until_mcp("notes", McpServerStartupState::Ready).await;
    koda.client.shutdown().await;
}

/// A reload starts a failed server again, so a server that wasn't ready
/// when the session opened can join without a restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reload_tries_a_failed_server_again() {
    let server = model(vec![]).await;
    let mut koda = Koda::start(config(&server)).await;
    let gate = koda._dir.path().join("ready");
    let script = mcp_server(koda._dir.path(), "0", Some(&gate));
    koda.write_config(&[(
        "mcp_servers.notes",
        json!({"command": "sh", "args": [script]}),
    )])
    .await
    .unwrap();
    koda.switch_to_new_session().await;
    koda.until_mcp("notes", McpServerStartupState::Failed).await;

    std::fs::write(&gate, "").unwrap();
    koda.reload_mcp().await;
    koda.until_mcp("notes", McpServerStartupState::Ready).await;
    let listed = koda.mcp_status(Some(&koda.session)).await;
    assert_eq!(
        listed[0].runtime_status,
        McpServerConnectionStatus::Connected
    );
    koda.client.shutdown().await;
}

/// Servers are config in Codex's shape: a bearer token is named by its
/// environment variable, never stored, and a bad server refuses the whole
/// batch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn servers_are_config_in_codexs_shape() {
    let server = model(vec![]).await;
    let koda = Koda::start(config(&server)).await;
    let mail = json!({"url": "https://mail.example/mcp", "bearer_token_env_var": "MAIL_TOKEN"});
    let written = koda
        .write_config(&[("mcp_servers.mail", mail)])
        .await
        .unwrap();
    let saved = &written.config.mcp_servers["mail"];
    assert_eq!(saved.url.as_deref(), Some("https://mail.example/mcp"));
    assert_eq!(saved.bearer_token_env_var.as_deref(), Some("MAIL_TOKEN"));
    // Writing a server leaves the model choice alone.
    assert_eq!(written.config.model, None);

    for bad in [
        json!({"url": "https://x.example/mcp", "bearer_token": "secret"}),
        json!({"command": "x", "url": "https://x.example/mcp"}),
        json!("x"),
    ] {
        let refused = server_error(
            koda.write_config(&[
                ("mcp_servers.other", json!({"command": "ok"})),
                ("mcp_servers.bad", bad.clone()),
            ])
            .await,
        );
        assert_eq!(refused.code, codes::INVALID_REQUEST, "{bad}: {refused:?}");
    }
    let read = koda
        .client
        .request::<methods::ConfigRead>(Default::default())
        .await
        .unwrap();
    let names: Vec<&String> = read.config.mcp_servers.keys().collect();
    assert_eq!(names, ["mail"], "a refused batch saved nothing");
    koda.client.shutdown().await;
}

// ── Memory ──────────────────────────────────────────────────────────────

/// A session says which instruction files its prompt carries
/// (`instructionSources`, as Codex's `thread/start` does), and
/// `session/memory/read` and `session/memory/add` read and save Koda's
/// notes, which the next turn's prompt carries.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_requests_read_and_save_the_notes_the_model_reads() {
    let dir = tempfile::tempdir().unwrap();
    // Koda's notes and global instructions live under the config directory,
    // which here is the test's own, never the real one.
    let xdg = dir.path().join("xdg");
    koda_core::runtime_env::set("XDG_CONFIG_HOME", xdg.display().to_string());
    std::fs::create_dir_all(xdg.join("koda")).unwrap();
    std::fs::write(xdg.join("koda/AGENTS.md"), "Global rule.").unwrap();
    std::fs::create_dir_all(dir.path().join("project")).unwrap();
    std::fs::write(dir.path().join("project/AGENTS.md"), "Project rule.").unwrap();
    let server = model(vec![text("Noted.")]).await;
    let mut koda = Koda::start_in(config(&server), dir).await;

    // The session's sources, global first.
    let sources = koda.opened.instruction_sources.clone().unwrap();
    assert_eq!(sources.len(), 2, "{sources:?}");
    assert!(sources[0].ends_with("xdg/koda/AGENTS.md"), "{sources:?}");
    assert!(sources[1].ends_with("project/AGENTS.md"), "{sources:?}");

    let notes = koda.read_memory().await.unwrap().notes;
    assert_eq!(notes.len(), 2);
    assert_eq!(notes[0].scope, MemoryScope::Project);
    assert_eq!(notes[1].scope, MemoryScope::Global);
    assert!(notes.iter().all(|n| n.text.is_none()), "{notes:?}");
    assert!(
        notes[1].path.ends_with("xdg/koda/memories/MEMORY.md"),
        "{notes:?}"
    );

    let saved = koda
        .add_memory(MemoryScope::Project, "## Build\nuse make")
        .await
        .unwrap();
    assert_eq!(saved.path, notes[0].path);
    // The same heading replaces its section.
    koda.add_memory(MemoryScope::Project, "## Build\nuse nextest")
        .await
        .unwrap();
    koda.add_memory(MemoryScope::Global, "prefer short replies")
        .await
        .unwrap();
    let notes = koda.read_memory().await.unwrap().notes;
    let project = notes[0].text.as_deref().unwrap();
    assert!(
        project.contains("use nextest") && !project.contains("use make"),
        "{project}"
    );
    assert!(
        notes[1]
            .text
            .as_deref()
            .unwrap()
            .contains("prefer short replies")
    );

    // Blank text and a session that isn't open are refused.
    let blank = server_error(koda.add_memory(MemoryScope::Global, "  ").await);
    assert!(blank.message.contains("blank"), "{blank:?}");
    let unknown = server_error(
        koda.client
            .request::<methods::SessionMemoryRead>(SessionMemoryReadParams {
                session_id: "nope".to_string(),
            })
            .await,
    );
    assert!(unknown.message.contains("no open session"), "{unknown:?}");

    // The next turn's prompt carries both sources and both notes.
    koda.turn("hi").await;
    koda.until("turn/completed").await;
    let prompt = chat_requests(&server).await[0]["messages"].to_string();
    for text in [
        "Global rule.",
        "Project rule.",
        "use nextest",
        "prefer short replies",
    ] {
        assert!(prompt.contains(text), "{text} isn't in {prompt}");
    }
    koda.client.shutdown().await;
}

// ── Background processes and agents ─────────────────────────────────────

/// Background processes outlive the turn that started them, and the client
/// lists and stops them with `session/backgroundTerminals/*`, as Codex's
/// `thread/backgroundTerminals/*`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_processes_are_listed_and_stopped_through_the_client() {
    let bash = |id, seconds| {
        tool_call(
            id,
            "Bash",
            json!({"command": format!("sleep {seconds}"), "background": true}),
        )
    };
    let server = model(vec![
        bash("call_1", 41),
        bash("call_2", 42),
        text("Started."),
    ])
    .await;
    let mut config = config(&server);
    config.trust = koda_core::trust::TrustMode::Safe;
    let mut koda = Koda::start(config).await;
    koda.turn("Start two sleeps in the background").await;
    for _ in 0..2 {
        let ServerRequest::ToolCallApproval { id, .. } = koda.request().await else {
            panic!("expected an approval");
        };
        koda.answer(&id, json!({"decision": {"type": "approve"}}))
            .await;
    }
    let messages = koda.until("turn/completed").await;
    assert_eq!(turn_completed(&messages).status, TurnStatus::Completed);

    // Both still run after the turn, oldest first.
    let list = || {
        koda.client
            .request::<methods::SessionBackgroundTerminalsList>(BackgroundTerminalsListParams {
                session_id: koda.session.clone(),
            })
    };
    let running = list().await.unwrap().data;
    assert_eq!(running.len(), 2, "{running:?}");
    assert!(running[0].command.contains("sleep 41"), "{running:?}");
    assert!(running[1].command.contains("sleep 42"), "{running:?}");
    assert!(running[0].cwd.ends_with("project"), "{running:?}");
    assert_eq!(running[0].process_id, running[0].os_pid.to_string());
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    assert!(
        (now_ms - running[0].started_at).abs() < 60_000,
        "{running:?}"
    );

    let terminate = |process_id: &str| {
        koda.client
            .request::<methods::SessionBackgroundTerminalsTerminate>(
                BackgroundTerminalsTerminateParams {
                    session_id: koda.session.clone(),
                    process_id: process_id.to_string(),
                },
            )
    };
    assert!(terminate(&running[0].process_id).await.unwrap().terminated);
    // Not again, and not a process Koda didn't start.
    assert!(!terminate(&running[0].process_id).await.unwrap().terminated);
    assert!(!terminate("1").await.unwrap().terminated);
    let bad = server_error(terminate("sleep").await);
    assert!(bad.message.contains("no background process"), "{bad:?}");
    let left = list().await.unwrap().data;
    assert_eq!(left.len(), 1, "{left:?}");
    assert!(left[0].command.contains("sleep 42"), "{left:?}");

    let cleaned = koda
        .client
        .request::<methods::SessionBackgroundTerminalsClean>(BackgroundTerminalsCleanParams {
            session_id: koda.session.clone(),
        })
        .await
        .unwrap();
    assert_eq!(cleaned.terminated, 1);
    assert!(list().await.unwrap().data.is_empty());
    koda.client.shutdown().await;
}

/// `agent/list` names the sub-agents the model can call: Koda's own, the
/// user's and the project's, a project's `.koda/agents/` winning.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_list_names_builtin_user_and_project_agents() {
    let dir = tempfile::tempdir().unwrap();
    let xdg = dir.path().join("xdg");
    koda_core::runtime_env::set("XDG_CONFIG_HOME", xdg.display().to_string());
    let agent = |folder: &std::path::Path, name: &str, description: &str| {
        std::fs::create_dir_all(folder).unwrap();
        std::fs::write(
            folder.join(format!("{name}.json")),
            json!({"name": name, "description": description, "system_prompt": "You help."})
                .to_string(),
        )
        .unwrap();
    };
    let project = dir.path().join("project");
    agent(&xdg.join("koda/agents"), "mine", "The user's");
    agent(&xdg.join("koda/agents"), "shared", "The user's version");
    agent(
        &project.join(".koda/agents"),
        "shared",
        "The project's version",
    );
    let server = model(vec![]).await;
    let koda = Koda::start_in(config(&server), dir).await;

    let listed = koda
        .client
        .request::<methods::AgentList>(AgentListParams {
            session_id: koda.session.clone(),
        })
        .await
        .unwrap()
        .data;
    let find = |name: &str| listed.iter().find(|a| a.name == name);
    assert_eq!(
        find("explore").map(|a| a.source),
        Some(AgentSource::Builtin)
    );
    assert_eq!(find("mine").map(|a| a.source), Some(AgentSource::User));
    let shared = find("shared").unwrap();
    assert_eq!(shared.source, AgentSource::Project);
    assert_eq!(shared.description, "The project's version");
    let names: Vec<&str> = listed.iter().map(|a| a.name.as_str()).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted);
    koda.client.shutdown().await;
}

/// `skills/list`, Codex's request: the host's project by default, with the
/// built-in, user and project skills in Codex's scopes, the files that
/// didn't load in `errors`, and any other folder asked about.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skills_list_names_each_skill_its_scope_and_the_files_that_failed() {
    use koda_client::protocol::skills::{SkillScope, SkillsListParams};
    let dir = tempfile::tempdir().unwrap();
    let xdg = dir.path().join("xdg");
    koda_core::runtime_env::set("XDG_CONFIG_HOME", xdg.display().to_string());
    let skill = |root: &std::path::Path, folder: &str, text: &str| {
        let folder = root.join(folder);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("SKILL.md"), text).unwrap();
        folder.join("SKILL.md")
    };
    let project = dir.path().join("project");
    let other = dir.path().join("other");
    let user_skills = xdg.join("koda/skills");
    let project_skills = project.join(".koda/skills");
    skill(
        &user_skills,
        "mine",
        "---\nname: mine\ndescription: The user's\n---\nBody",
    );
    let ours = skill(
        &project_skills,
        "ours",
        "---\nname: ours\ndescription: The project's\nuser_invocable: false\n---\nBody",
    );
    let broken = skill(&project_skills, "broken", "No frontmatter here.");
    skill(
        &other.join(".koda/skills"),
        "elsewhere",
        "---\nname: elsewhere\ndescription: Another folder's\n---\nBody",
    );
    let server = model(vec![]).await;
    let koda = Koda::start_in(config(&server), dir).await;

    let listed = koda
        .client
        .request::<methods::SkillsList>(SkillsListParams::default())
        .await
        .unwrap()
        .data;
    assert_eq!(listed.len(), 1);
    let here = &listed[0];
    let find = |name: &str| here.skills.iter().find(|s| s.name == name).unwrap();
    assert_eq!(find("code-review").scope, SkillScope::System);
    assert_eq!(find("code-review").path, None);
    assert_eq!(find("koda-docs").scope, SkillScope::System);
    assert_eq!(find("mine").scope, SkillScope::User);
    let ours_listed = find("ours");
    assert_eq!(ours_listed.scope, SkillScope::Repo);
    assert_eq!(
        ours_listed.path.as_deref(),
        Some(ours.display().to_string().as_str())
    );
    assert!(!ours_listed.user_invocable && ours_listed.enabled);
    assert!(here.skills.iter().all(|s| s.name != "elsewhere"));
    assert_eq!(here.errors.len(), 1, "{:?}", here.errors);
    assert_eq!(here.errors[0].path, broken.display().to_string());
    assert!(
        here.errors[0].message.contains("frontmatter"),
        "{:?}",
        here.errors
    );

    let elsewhere = koda
        .client
        .request::<methods::SkillsList>(SkillsListParams {
            cwds: vec![other.display().to_string()],
        })
        .await
        .unwrap()
        .data;
    assert_eq!(elsewhere[0].cwd, other.display().to_string());
    let names: Vec<&str> = elsewhere[0]
        .skills
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        names.contains(&"elsewhere") && names.contains(&"mine"),
        "{names:?}"
    );
    assert!(!names.contains(&"ours"), "{names:?}");
    assert!(elsewhere[0].errors.is_empty());
    koda.client.shutdown().await;
}

// ── What a client renders from (6h-a) ───────────────────────────────────

/// An edit's approval carries its diff in Codex's `changes` shape, and so
/// does the call's item.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edits_approval_carries_its_diff() {
    use koda_client::protocol::item::PatchChangeKind;
    let server = model(vec![
        // Koda edits only a file the model has read.
        tool_call("call_r", "Read", json!({"file_path": "a.txt"})),
        tool_call(
            "call_e",
            "Edit",
            json!({"file_path": "a.txt", "old_str": "two", "new_str": "three"}),
        ),
        tool_call(
            "call_w",
            "Write",
            json!({"file_path": "new.txt", "content": "fresh\n"}),
        ),
        text("Done."),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("project")).unwrap();
    std::fs::write(dir.path().join("project/a.txt"), "one\ntwo\n").unwrap();
    let mut config = config(&server);
    config.trust = koda_core::trust::TrustMode::Safe;
    let mut koda = Koda::start_in(config, dir).await;
    koda.turn("Edit a.txt and write new.txt").await;
    let edit = koda.request().await;
    let ServerRequest::ToolCallApproval { id, params } = &edit else {
        panic!("expected an approval, got {edit:?}");
    };
    assert_eq!(params.changes.len(), 1, "{params:?}");
    let change = &params.changes[0];
    assert_eq!(change.path, "a.txt");
    assert_eq!(change.kind, PatchChangeKind::Update { move_path: None });
    assert!(
        change.diff.starts_with("@@ -1,2 +1,2 @@"),
        "{}",
        change.diff
    );
    assert!(change.diff.contains("\n-two\n+three\n"), "{}", change.diff);
    let edit_changes = params.changes.clone();
    koda.answer(id, json!({"decision": {"type": "approve"}}))
        .await;

    // The edit completes before the write is asked about.
    let mut messages = Vec::new();
    let write = loop {
        let message = tokio::time::timeout(WAIT, koda.client.next_event())
            .await
            .unwrap()
            .unwrap();
        match message {
            Outgoing::Request(request) => break request,
            other => messages.push(other),
        }
    };
    let ServerRequest::ToolCallApproval { id, params } = &write else {
        panic!("expected an approval, got {write:?}");
    };
    assert_eq!(params.changes[0].kind, PatchChangeKind::Add);
    assert_eq!(params.changes[0].diff, "fresh\n");
    koda.answer(id, json!({"decision": {"type": "approve"}}))
        .await;
    koda.until("turn/completed").await;

    let edited = completed(&messages)
        .into_iter()
        .find_map(|item| match item {
            Item::ToolCall { id, changes, .. } if id == "call_e" => Some(changes.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(edited, edit_changes);
    assert_eq!(
        std::fs::read_to_string(koda.project.join("a.txt")).unwrap(),
        "one\nthree\n"
    );
    koda.client.shutdown().await;
}

fn token_usage(messages: &[Outgoing]) -> Vec<&koda_client::protocol::session::TokenUsage> {
    messages
        .iter()
        .filter_map(|m| match m {
            Outgoing::Notification(ServerNotification::SessionTokenUsageUpdated(n)) => {
                Some(&n.token_usage)
            }
            _ => None,
        })
        .collect()
}

/// `session/tokenUsage/updated` goes out after each model call, with the
/// provider's counts, as Codex sends its token count.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_usage_is_sent_after_each_model_call() {
    let server = model(vec![
        tool_call("call_1", "List", json!({})),
        text("Listed."),
    ])
    .await;
    let mut koda = Koda::start(config(&server)).await;
    koda.turn("List the files").await;
    let messages = koda.until("turn/completed").await;
    let usage = token_usage(&messages);
    assert_eq!(usage.len(), 2, "{}", names(&messages));
    assert_eq!(usage[0].last.input_tokens, 100);
    assert_eq!(usage[0].total.total_tokens, 110);
    let last = usage[1];
    assert_eq!(last.model_context_window, Some(100_000));
    assert_eq!(last.last.input_tokens, 200);
    assert_eq!(last.last.output_tokens, 20);
    assert_eq!(last.total.input_tokens, 300);
    assert_eq!(last.total.output_tokens, 30);
    koda.client.shutdown().await;
}

/// A reply that doesn't stream, as a sub-agent asks for.
fn whole(message: Value) -> ResponseTemplate {
    let finish = if message["tool_calls"].is_null() {
        "stop"
    } else {
        "tool_calls"
    };
    ResponseTemplate::new(200)
        .insert_header("content-type", "application/json")
        .set_body_string(
            json!({
                "choices": [{"index": 0, "message": message, "finish_reason": finish}],
                "usage": {"prompt_tokens": 50, "completion_tokens": 5},
            })
            .to_string(),
        )
}

/// A resumed session returns its turns exactly as they were sent, a
/// sub-agent's tool calls under its call; `session/read` returns them
/// without opening it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resumed_session_returns_its_turns_as_they_were_sent() {
    use koda_client::protocol::session::SessionReadParams;
    let server = model(vec![
        tool_call(
            "call_agent",
            "InvokeAgent",
            json!({"agent_name": "explore", "prompt": "What does notes.txt say?"}),
        ),
        // The sub-agent, which doesn't stream.
        whole(json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call_read",
                "type": "function",
                "function": {"name": "Read", "arguments": json!({"file_path": "notes.txt"}).to_string()},
            }],
        })),
        whole(json!({"role": "assistant", "content": "It says 42."})),
        // The session again.
        text("The notes say 42."),
        text("Yes."),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("project")).unwrap();
    std::fs::write(dir.path().join("project/notes.txt"), "42\n").unwrap();
    let mut koda = Koda::start_in(config(&server), dir).await;
    koda.turn("Ask a sub-agent about the notes").await;
    let first = koda.until("turn/completed").await;
    koda.turn("Sure?").await;
    let second = koda.until("turn/completed").await;
    // Each turn as the client saw it: the turn as it completed, and its
    // completed items with their parents.
    let live: Vec<koda_client::protocol::turn::Turn> = [first, second]
        .iter()
        .map(|messages| {
            let mut turn = turn_completed(messages).clone();
            // When each item started, from its `item/started`.
            let started: std::collections::HashMap<String, i64> = messages
                .iter()
                .filter_map(|m| match m {
                    Outgoing::Notification(ServerNotification::ItemStarted(n)) => {
                        Some((n.item.id().to_string(), n.started_at_ms))
                    }
                    _ => None,
                })
                .collect();
            turn.items = messages
                .iter()
                .filter_map(|m| match m {
                    Outgoing::Notification(ServerNotification::ItemCompleted(n)) => {
                        Some(TurnItem {
                            parent_item_id: n.parent_item_id.clone(),
                            item: n.item.clone(),
                            started_at_ms: started.get(n.item.id()).copied(),
                            completed_at_ms: Some(n.completed_at_ms),
                        })
                    }
                    _ => None,
                })
                .collect();
            turn
        })
        .collect();
    // The sub-agent's call and text are under its InvokeAgent call.
    assert!(
        live[0].items.iter().any(|i| {
            i.parent_item_id.as_deref() == Some("call_agent")
                && matches!(&i.item, Item::ToolCall { id, .. } if id == "call_read")
        }),
        "{live:#?}"
    );
    assert_eq!(
        koda.unsubscribe().await,
        SessionUnsubscribeStatus::Unsubscribed
    );

    // Read without opening: the stored form, turns only when asked.
    let read = |include_turns| {
        koda.client
            .request::<methods::SessionRead>(SessionReadParams {
                session_id: koda.session.clone(),
                include_turns,
            })
    };
    let bare = read(false).await.unwrap().session;
    assert_eq!(bare.id, koda.session);
    assert!(bare.turns.is_empty());
    assert!(
        bare.model.is_none(),
        "a closed session isn't running with anything"
    );
    let read_back = read(true).await.unwrap().session.turns;

    let resumed = koda
        .client
        .request::<methods::SessionStart>(SessionStartParams {
            session_id: Some(koda.session.clone()),
            ..Default::default()
        })
        .await
        .unwrap()
        .session
        .turns;
    assert_eq!(resumed, read_back);
    // Exactly as sent: ids, statuses, usage and items.
    assert_eq!(resumed, live);
    // A turn run after resuming follows them.
    koda.turn("And now?").await;
    koda.until("turn/completed").await;
    let after = koda
        .client
        .request::<methods::SessionRead>(SessionReadParams {
            session_id: koda.session.clone(),
            include_turns: true,
        })
        .await
        .unwrap()
        .session
        .turns;
    assert_eq!(after.len(), 3);
    assert_eq!(after[..2], live[..]);
    assert_eq!(after[2].status, TurnStatus::Completed);
    let error = server_error(
        koda.client
            .request::<methods::SessionRead>(SessionReadParams {
                session_id: "no-such-session".to_string(),
                include_turns: true,
            })
            .await,
    );
    assert_eq!(error.code, codes::INVALID_PARAMS);
    koda.client.shutdown().await;
}
