//! `koda serve` on a Unix socket: several clients at once, each hearing its
//! own sessions; sessions that outlive their client; one server per socket,
//! private to the user. The JSON-RPC itself is `koda serve --stdio`'s,
//! checked in `serve.rs`.
#![cfg(unix)]

use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use koda_core::config::{KodaConfig, ProviderType};
use koda_server::socket::{Options, ServeError, serve_with_config};
use koda_server::stdio::ServeArgs;
use koda_test_utils::network::{FakeLlmServer, ResponseTemplate};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

const WAIT: Duration = Duration::from_secs(30);

fn text(reply: &str) -> ResponseTemplate {
    let chunks = [
        json!({"choices": [{"delta": {"content": reply}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 200, "completion_tokens": 20}}),
    ];
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

fn write_file(id: &str, path: &str) -> ResponseTemplate {
    let args = json!({"file_path": path, "content": "hi"});
    let chunks = [
        json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": id,
            "type": "function",
            "function": {"name": "Write", "arguments": args.to_string()},
        }]}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 100, "completion_tokens": 10}}),
    ];
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
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

/// [`config`] in Safe mode, where a write waits for an approval.
fn safe(server: &FakeLlmServer) -> KodaConfig {
    let mut config = config(server);
    config.trust = koda_core::trust::TrustMode::Safe;
    config
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

/// A running `koda serve` on a socket in a temp folder.
struct Server {
    socket: PathBuf,
    stop: Option<oneshot::Sender<()>>,
    served: JoinHandle<Result<(), ServeError>>,
    _dir: Option<tempfile::TempDir>,
}

impl Server {
    async fn start(config: KodaConfig, unload_delay: Duration) -> Self {
        Self::start_waiting(config, unload_delay, Duration::from_secs(60)).await
    }

    /// [`Server::start`], with requests waiting `approval_wait` for an answer.
    async fn start_waiting(
        config: KodaConfig,
        unload_delay: Duration,
        approval_wait: Duration,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut server =
            Self::start_in_waiting(dir.path(), config, unload_delay, approval_wait).await;
        server._dir = Some(dir);
        server
    }

    /// Serve with its state in `dir`, which the caller keeps.
    async fn start_in(dir: &Path, config: KodaConfig, unload_delay: Duration) -> Self {
        Self::start_in_waiting(dir, config, unload_delay, Duration::from_secs(60)).await
    }

    async fn start_in_waiting(
        dir: &Path,
        config: KodaConfig,
        unload_delay: Duration,
        approval_wait: Duration,
    ) -> Self {
        Self::start_in_with(
            dir,
            config,
            Options {
                unload_delay,
                idle_exit: None,
                approval_wait: Some(approval_wait),
            },
        )
        .await
    }

    /// [`Server::start`] with these options.
    async fn start_with(config: KodaConfig, options: Options) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut server = Self::start_in_with(dir.path(), config, options).await;
        server._dir = Some(dir);
        server
    }

    async fn start_in_with(dir: &Path, config: KodaConfig, options: Options) -> Self {
        let socket = dir.join("serve").join("koda.sock");
        let (stop, stopped) = oneshot::channel::<()>();
        let served = tokio::spawn(serve_with_config(
            args(dir),
            config,
            socket.clone(),
            options,
            async {
                let _ = stopped.await;
            },
        ));
        let deadline = Instant::now() + WAIT;
        while UnixStream::connect(&socket).await.is_err() {
            assert!(!served.is_finished(), "koda serve stopped before listening");
            assert!(Instant::now() < deadline, "koda serve never listened");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Self {
            socket,
            stop: Some(stop),
            served,
            _dir: None,
        }
    }

    async fn stop(mut self) {
        let _ = self.stop.take().unwrap().send(());
        tokio::time::timeout(WAIT, &mut self.served)
            .await
            .expect("koda serve didn't stop")
            .unwrap()
            .unwrap();
        assert!(!self.socket.exists(), "the socket is removed on stop");
    }
}

fn args(dir: &Path) -> ServeArgs {
    let project = dir.join("project");
    std::fs::create_dir_all(&project).unwrap();
    ServeArgs {
        config_file: koda_server::ConfigFile::in_dir(&dir.join("config")),
        project_root: project,
        home: dir.join("config"),
    }
}

/// One client on the socket.
struct Client {
    input: OwnedWriteHalf,
    output: BufReader<OwnedReadHalf>,
    next_id: i64,
    pending: VecDeque<Value>,
}

impl Client {
    async fn connect(server: &Server) -> Self {
        let stream = UnixStream::connect(&server.socket).await.unwrap();
        let (output, input) = stream.into_split();
        Self {
            input,
            output: BufReader::new(output),
            next_id: 1,
            pending: VecDeque::new(),
        }
    }

    /// Connected and initialized.
    async fn ready(server: &Server) -> Self {
        let mut client = Self::connect(server).await;
        let init = client
            .send_request(
                "initialize",
                json!({"clientInfo": {"name": "socket-test", "version": "0"}}),
            )
            .await;
        client.response(init).await.unwrap();
        client
    }

    async fn send_request(&mut self, method: &str, params: Value) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.input
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
        id
    }

    async fn read(&mut self) -> Option<Value> {
        if let Some(line) = self.pending.pop_front() {
            return Some(line);
        }
        let mut line = String::new();
        let read = tokio::time::timeout(WAIT, self.output.read_line(&mut line))
            .await
            .expect("koda serve wrote nothing in time")
            .unwrap();
        (read > 0).then(|| serde_json::from_str(&line).unwrap())
    }

    /// The answer to request `id`; every line before it is kept, for
    /// [`Client::seen`].
    async fn response(&mut self, id: i64) -> Result<Value, Value> {
        let mut before = Vec::new();
        let answer = loop {
            let line = self.read().await.expect("the socket closed");
            if line["id"] == id && line.get("method").is_none() {
                break line;
            }
            before.push(line);
        };
        self.pending.extend(before);
        match answer.get("error") {
            Some(error) => Err(error.clone()),
            None => Ok(answer["result"].clone()),
        }
    }

    /// Answer Koda's request `id`.
    async fn answer(&mut self, id: &Value, result: Value) {
        let line = json!({"jsonrpc": "2.0", "id": id, "result": result});
        self.input
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        let id = self.send_request(method, params).await;
        self.response(id).await
    }

    /// Lines up to and including the first `method` message.
    async fn until_method(&mut self, method: &str) -> Vec<Value> {
        let mut seen = Vec::new();
        loop {
            let line = self.read().await.expect("the socket closed");
            let done = line["method"] == method;
            seen.push(line);
            if done {
                return seen;
            }
        }
    }

    /// The lines set aside so far, taken.
    fn seen(&mut self) -> Vec<Value> {
        self.pending.drain(..).collect()
    }

    async fn start_session(&mut self) -> String {
        let started = self.request("session/start", json!({})).await.unwrap();
        started["session"]["id"].as_str().unwrap().to_string()
    }
}

fn session_of(line: &Value) -> Option<&str> {
    let params = &line["params"];
    params["sessionId"]
        .as_str()
        .or_else(|| params["session"]["id"].as_str())
}

/// Two clients at once: each is answered, and hears only its own sessions,
/// as Codex routes a thread's messages to its subscribers.
#[tokio::test(flavor = "multi_thread")]
async fn each_client_hears_its_own_sessions() {
    let server = model(vec![text("for alpha")]).await;
    let koda = Server::start(config(&server), Duration::from_secs(60)).await;
    let mut alpha = Client::ready(&koda).await;
    let mut beta = Client::ready(&koda).await;
    let alpha_session = alpha.start_session().await;
    let beta_session = beta.start_session().await;
    assert_ne!(alpha_session, beta_session);

    alpha
        .request(
            "turn/start",
            json!({"sessionId": alpha_session, "input": "hi"}),
        )
        .await
        .unwrap();
    let heard = alpha.until_method("turn/completed").await;
    assert!(
        heard
            .iter()
            .any(|l| l["method"] == "item/agentMessage/delta"),
        "{heard:?}"
    );
    assert!(
        heard
            .iter()
            .all(|l| session_of(l).is_none_or(|s| s == alpha_session)),
        "{heard:?}"
    );

    // Beta heard its own session start, and nothing of alpha's.
    beta.request("state/read", json!({})).await.unwrap();
    let beta_heard = beta.seen();
    assert!(
        beta_heard
            .iter()
            .all(|l| session_of(l) != Some(alpha_session.as_str())),
        "{beta_heard:?}"
    );
    assert!(
        beta_heard
            .iter()
            .any(|l| l["method"] == "session/started"
                && session_of(l) == Some(beta_session.as_str())),
        "the new session's first message reaches its client: {beta_heard:?}"
    );
    koda.stop().await;
}

/// A session outlives the client that opened it: another client attaches
/// to it as it runs, as Codex subscribes a client that resumes a loaded
/// thread. Once it has had no client and no turn for the unload delay it
/// closes, as Codex unloads a thread, and opening it again starts it anew.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_outlives_its_client_until_it_is_idle() {
    let server = model(vec![]).await;
    let koda = Server::start(config(&server), Duration::from_millis(300)).await;
    let mut first = Client::ready(&koda).await;
    let session = first.start_session().await;
    drop(first);

    // Still open: attaching gives it as it runs, without starting it again.
    let mut second = Client::ready(&koda).await;
    let attached = second
        .request("session/start", json!({"sessionId": session}))
        .await
        .unwrap();
    assert_eq!(attached["session"]["id"], session.as_str());
    assert_eq!(attached["session"]["model"], "stub", "{attached}");
    assert!(attached["session"]["tools"].is_array(), "{attached}");
    assert!(
        second
            .seen()
            .iter()
            .all(|l| l["method"] != "session/started"),
        "attached, not started again"
    );

    // `session/unsubscribe` leaves it to the idle rule.
    let left = second
        .request("session/unsubscribe", json!({"sessionId": session}))
        .await
        .unwrap();
    assert_eq!(left["status"], "unsubscribed");
    let again = second
        .request("session/unsubscribe", json!({"sessionId": session}))
        .await
        .unwrap();
    assert_eq!(again["status"], "notLoaded");

    // Closed once idle past the delay: opening it starts it again.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let reopened = second
        .request("session/start", json!({"sessionId": session}))
        .await
        .unwrap();
    assert_eq!(reopened["session"]["id"], session.as_str());
    assert!(
        second
            .seen()
            .iter()
            .any(|l| l["method"] == "session/started" && session_of(l) == Some(session.as_str())),
        "started anew"
    );
    koda.stop().await;
}

/// Each connection sends `initialize` first, once.
#[tokio::test(flavor = "multi_thread")]
async fn a_connection_initializes_once_before_anything_else() {
    let server = model(vec![]).await;
    let koda = Server::start(config(&server), Duration::from_secs(60)).await;
    let mut client = Client::connect(&koda).await;
    let early = client.request("state/read", json!({})).await;
    assert!(
        early.is_err_and(|e| e["message"] == "Not initialized"),
        "requests wait for initialize"
    );
    let init = json!({"clientInfo": {"name": "t", "version": "0"}});
    let answer = client.request("initialize", init.clone()).await.unwrap();
    assert_eq!(answer["serverInfo"]["name"], "koda");
    let twice = client.request("initialize", init).await;
    assert!(twice.is_err_and(|e| e["message"] == "Already initialized"));
    // Another connection initializes on its own.
    let mut other = Client::ready(&koda).await;
    other.request("state/read", json!({})).await.unwrap();
    koda.stop().await;
}

/// One server per socket: a second is refused while the first runs, and a
/// socket a crashed server left behind is replaced.
#[tokio::test(flavor = "multi_thread")]
async fn one_server_per_socket_and_a_stale_one_is_replaced() {
    let server = model(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let koda = Server::start_in(dir.path(), config(&server), Duration::from_secs(60)).await;
    let second = serve_with_config(
        args(dir.path()),
        config(&server),
        koda.socket.clone(),
        Options::default(),
        std::future::pending(),
    )
    .await;
    assert!(matches!(second, Err(ServeError::InUse(_))), "{second:?}");
    let socket = koda.socket.clone();
    koda.stop().await;

    // A socket file with nobody behind it, as a crash leaves one.
    drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
    assert!(socket.exists());
    let again = Server::start_in(dir.path(), config(&server), Duration::from_secs(60)).await;
    Client::ready(&again).await;
    again.stop().await;
}

/// Only its owner reaches the socket, in a folder only they can change.
#[tokio::test(flavor = "multi_thread")]
async fn the_socket_is_private_to_its_owner() {
    let server = model(vec![]).await;
    let koda = Server::start(config(&server), Duration::from_secs(60)).await;
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&koda.socket), 0o600);
    assert_eq!(mode(koda.socket.parent().unwrap()), 0o700);
    koda.stop().await;

    // A folder others may write to is refused.
    let dir = tempfile::tempdir().unwrap();
    let open = dir.path().join("serve");
    std::fs::create_dir(&open).unwrap();
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
    let refused = serve_with_config(
        args(dir.path()),
        config(&server),
        open.join("koda.sock"),
        Options::default(),
        std::future::pending(),
    )
    .await;
    assert!(
        matches!(&refused, Err(ServeError::Socket { error, .. })
            if error.kind() == std::io::ErrorKind::PermissionDenied),
        "{refused:?}"
    );
}

/// An approval left waiting when its client goes is sent to a client that
/// attaches to the session, as Codex replays a pending request; a second
/// client attached hears that the first answered it.
#[tokio::test(flavor = "multi_thread")]
async fn an_approval_waits_for_a_client_that_attaches() {
    let server = model(vec![write_file("w1", "a.txt"), text("Written.")]).await;
    let koda = Server::start(safe(&server), Duration::from_secs(60)).await;
    let mut first = Client::ready(&koda).await;
    let session = first.start_session().await;
    first
        .request(
            "turn/start",
            json!({"sessionId": session, "input": "write it"}),
        )
        .await
        .unwrap();
    let asked = first.until_method("item/toolCall/requestApproval").await;
    let request_id = asked.last().unwrap()["id"].clone();
    drop(first);

    // Two clients attach; each is sent the waiting approval after the
    // session.
    let mut second = Client::ready(&koda).await;
    let attached = second
        .request("session/start", json!({"sessionId": session}))
        .await
        .unwrap();
    assert_eq!(attached["session"]["id"], session.as_str());
    let resent = second.until_method("item/toolCall/requestApproval").await;
    assert_eq!(resent.last().unwrap()["id"], request_id);
    let mut third = Client::ready(&koda).await;
    third
        .request("session/start", json!({"sessionId": session}))
        .await
        .unwrap();
    third.until_method("item/toolCall/requestApproval").await;

    second
        .answer(&request_id, json!({"decision": {"type": "approve"}}))
        .await;
    let done = second.until_method("turn/completed").await;
    assert_eq!(
        done.last().unwrap()["params"]["turn"]["status"],
        "completed"
    );
    let project = koda
        .socket
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("project");
    assert_eq!(
        std::fs::read_to_string(project.join("a.txt")).unwrap(),
        "hi"
    );
    // The other client is told it was answered, so it can drop its prompt.
    let heard = third.until_method("serverRequest/resolved").await;
    assert_eq!(heard.last().unwrap()["params"]["requestId"], request_id);
    koda.stop().await;
}

/// An approval no client answers is refused once the wait runs out, and
/// the turn goes on without it.
#[tokio::test(flavor = "multi_thread")]
async fn an_approval_nobody_answers_is_refused_after_the_wait() {
    let server = model(vec![write_file("w1", "a.txt"), text("Not written.")]).await;
    let koda = Server::start_waiting(
        safe(&server),
        Duration::from_secs(60),
        Duration::from_millis(500),
    )
    .await;
    let mut client = Client::ready(&koda).await;
    let session = client.start_session().await;
    client
        .request(
            "turn/start",
            json!({"sessionId": session, "input": "write it"}),
        )
        .await
        .unwrap();
    client.until_method("item/toolCall/requestApproval").await;
    // Nobody answers.
    let done = client.until_method("turn/completed").await;
    assert_eq!(
        done.last().unwrap()["params"]["turn"]["status"],
        "completed"
    );
    let project = koda
        .socket
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("project");
    assert!(!project.join("a.txt").exists(), "the write was refused");
    assert!(
        done.iter().any(|l| l["method"] == "serverRequest/resolved"),
        "{done:?}"
    );
    koda.stop().await;
}

/// A reply that reads `path`.
fn read_file(id: &str, path: &str) -> ResponseTemplate {
    let args = json!({"file_path": path});
    let chunks = [
        json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": id,
            "type": "function",
            "function": {"name": "Read", "arguments": args.to_string()},
        }]}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 100, "completion_tokens": 10}}),
    ];
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

/// One server for every project: `session/start` takes the session's
/// folder (`cwd`) and its own config values (`config`), as Codex's
/// `thread/start` does, and `session/fork` takes them for the copy.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_works_in_its_own_folder_with_its_own_config() {
    let server = model(vec![read_file("r1", "notes.txt"), text("Read it.")]).await;
    let koda = Server::start(config(&server), Duration::from_secs(60)).await;
    let folders = tempfile::tempdir().unwrap();
    let (alpha, beta) = (folders.path().join("alpha"), folders.path().join("beta"));
    for (folder, text) in [(&alpha, "from alpha"), (&beta, "from beta")] {
        std::fs::create_dir(folder).unwrap();
        std::fs::write(folder.join("notes.txt"), text).unwrap();
    }
    let alpha = alpha.canonicalize().unwrap();
    let beta = beta.canonicalize().unwrap();

    let mut client = Client::ready(&koda).await;
    let started = client
        .request(
            "session/start",
            json!({"cwd": beta.display().to_string(), "config": {"mode": "plan"}}),
        )
        .await
        .unwrap();
    assert_eq!(started["session"]["cwd"], beta.display().to_string());
    assert_eq!(started["session"]["mode"], "plan", "its own config");
    let session = started["session"]["id"].as_str().unwrap().to_string();
    client
        .request(
            "turn/start",
            json!({"sessionId": session, "input": "read the notes"}),
        )
        .await
        .unwrap();
    let heard = client.until_method("turn/completed").await;
    let read = heard
        .iter()
        .find(|l| l["method"] == "item/completed" && l["params"]["item"]["id"] == "r1")
        .unwrap_or_else(|| panic!("no Read: {heard:?}"));
    assert!(read.to_string().contains("from beta"), "{read}");

    // A fork opens where it's told, and belongs there.
    let forked = client
        .request(
            "session/fork",
            json!({"sessionId": session, "cwd": alpha.display().to_string()}),
        )
        .await
        .unwrap();
    assert_eq!(forked["session"]["cwd"], alpha.display().to_string());

    // A folder that isn't one, and config Koda doesn't take, are refused.
    for (params, says) in [
        (json!({"cwd": "relative/path"}), "absolute"),
        (
            json!({"cwd": "/no/such/folder/anywhere"}),
            "/no/such/folder",
        ),
        (json!({"config": {"no_such_key": 1}}), "no_such_key"),
        (json!({"config": {"mode": null}}), "mode"),
    ] {
        let refused = client.request("session/start", params.clone()).await;
        assert!(
            refused
                .as_ref()
                .is_err_and(|e| e.to_string().contains(says)),
            "{params}: {refused:?}"
        );
    }
    koda.stop().await;
}

/// The client end: [`koda_server::socket::connect`] gives what a host in
/// the process gives, so a client runs a turn over the socket, answers an
/// approval, and hangs up leaving the session on the server.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_runs_a_turn_over_the_socket() {
    use koda_server::events::Outgoing;
    use koda_server::protocol::{ServerNotification, ServerRequest, methods};
    let server = model(vec![write_file("w1", "a.txt"), text("Written.")]).await;
    let koda = Server::start(safe(&server), Duration::from_secs(60)).await;
    let mut connected = koda_server::socket::connect(&koda.socket).await.unwrap();
    let handle = connected.requests.clone();
    let info = handle
        .request::<methods::Initialize>(koda_server::protocol::initialize::InitializeParams {
            client_info: koda_server::protocol::initialize::ClientInfo {
                name: "socket-client".into(),
                version: "0".into(),
            },
        })
        .await
        .unwrap();
    assert_eq!(info.server_info.name, "koda");
    let started = handle
        .request::<methods::SessionStart>(Default::default())
        .await
        .unwrap();
    let session = started.session.id.clone();
    handle
        .request::<methods::TurnStart>(koda_server::protocol::turn::TurnStartParams {
            session_id: session.clone(),
            input: "write it".into(),
            images: Vec::new(),
            mentions: Vec::new(),
            client_message_id: None,
            mode: None,
        })
        .await
        .unwrap();
    let mut completed = None;
    while completed.is_none() {
        let event = tokio::time::timeout(WAIT, connected.events.recv())
            .await
            .expect("no event in time")
            .expect("the connection closed");
        match event {
            Outgoing::Request(request @ ServerRequest::ToolCallApproval { .. }) => {
                handle
                    .respond(
                        request.id().clone(),
                        Ok(json!({"decision": {"type": "approve"}})),
                    )
                    .await
                    .unwrap();
            }
            Outgoing::Notification(ServerNotification::TurnCompleted(done)) => {
                completed = Some(done.turn);
            }
            _ => {}
        }
    }
    assert_eq!(
        completed.unwrap().status,
        koda_server::protocol::turn::TurnStatus::Completed
    );
    let project = koda
        .socket
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("project");
    assert_eq!(
        std::fs::read_to_string(project.join("a.txt")).unwrap(),
        "hi"
    );

    // Hanging up leaves the session on the server: another client attaches.
    handle.shutdown(connected.host).await;
    let mut other = Client::ready(&koda).await;
    let attached = other
        .request("session/start", json!({"sessionId": session}))
        .await
        .unwrap();
    assert_eq!(attached["session"]["id"], session.as_str());
    koda.stop().await;
}

/// `serve/stop` stops the server, as `claude daemon stop` does; with
/// `ifIdle` only when nothing would be lost, which a TUI of another version
/// asks before it starts its own server.
#[tokio::test(flavor = "multi_thread")]
async fn serve_stop_stops_the_server_unless_asked_only_when_idle() {
    let server = model(vec![]).await;
    let mut koda = Server::start(config(&server), Duration::from_secs(60)).await;
    let mut asking = Client::ready(&koda).await;
    let mut other = Client::ready(&koda).await;
    let session = other.start_session().await;

    let busy = asking
        .request("serve/stop", json!({"ifIdle": true}))
        .await
        .unwrap();
    assert_eq!(busy["stopped"], false, "{busy}");
    assert!(
        busy["reason"].as_str().unwrap().contains(&session),
        "a session with a client is in use: {busy}"
    );
    // Its client gone, the session stays open but idle: stored, so losing
    // nothing. Another client still counts.
    drop(other);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut third = Client::ready(&koda).await;
    let still = asking
        .request("serve/stop", json!({"ifIdle": true}))
        .await
        .unwrap();
    assert_eq!(still["stopped"], false, "{still}");
    assert!(still["reason"].as_str().unwrap().contains("other client"));
    drop(third.request("state/read", json!({})).await);
    drop(third);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let idle = asking
        .request("serve/stop", json!({"ifIdle": true}))
        .await
        .unwrap();
    assert_eq!(idle["stopped"], true, "{idle}");
    tokio::time::timeout(WAIT, &mut koda.served)
        .await
        .expect("koda serve didn't stop")
        .unwrap()
        .unwrap();
    assert!(!koda.socket.exists());
}

/// Started with an idle exit, as the TUI starts it, the server stops once
/// it has had no client and no work that long, and not while a client is
/// connected.
#[tokio::test(flavor = "multi_thread")]
async fn a_server_with_an_idle_exit_stops_once_idle() {
    let server = model(vec![]).await;
    let mut koda = Server::start_with(
        config(&server),
        Options {
            unload_delay: Duration::from_secs(60),
            idle_exit: Some(Duration::from_millis(400)),
            approval_wait: None,
        },
    )
    .await;
    let client = Client::ready(&koda).await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(
        !koda.served.is_finished(),
        "not while a client is connected"
    );
    drop(client);
    tokio::time::timeout(WAIT, &mut koda.served)
        .await
        .expect("koda serve didn't stop when idle")
        .unwrap()
        .unwrap();
    assert!(!koda.socket.exists());
}
