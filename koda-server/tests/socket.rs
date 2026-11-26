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
        Self::ready_with(server, json!({})).await
    }

    /// Connected and initialized, saying it takes `capabilities`.
    async fn ready_with(server: &Server, capabilities: Value) -> Self {
        let mut client = Self::connect(server).await;
        let init = client
            .send_request(
                "initialize",
                json!({
                    "clientInfo": {"name": "socket-test", "version": "0"},
                    "capabilities": capabilities,
                }),
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

/// Two clients at once: each is answered and hears its own sessions' turns,
/// as Codex routes a thread's messages to its subscribers, and both hear
/// what changes the list of sessions.
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
            json!({"sessionId": alpha_session, "input": [{"type": "text", "text": "hi"}]}),
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
    // What changes the list of sessions (a start, a rename, a delete) goes
    // to every client, as Codex sends them; a session's turns only to its
    // own.
    let lists = |l: &Value| {
        matches!(
            l["method"].as_str(),
            Some(
                "session/started"
                    | "session/name/updated"
                    | "session/deleted"
                    | "session/status/changed"
            )
        )
    };
    assert!(
        heard
            .iter()
            .all(|l| lists(l) || session_of(l).is_none_or(|s| s == alpha_session)),
        "{heard:?}"
    );
    assert!(
        heard
            .iter()
            .any(|l| l["method"] == "session/started"
                && session_of(l) == Some(beta_session.as_str())),
        "alpha hears of beta's new session: {heard:?}"
    );

    // Beta heard its own session start, and of alpha's turn nothing.
    beta.request("state/read", json!({})).await.unwrap();
    let beta_heard = beta.seen();
    assert!(
        beta_heard
            .iter()
            .all(|l| lists(l) || session_of(l) != Some(alpha_session.as_str())),
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

/// A notification a connection opted out of in `initialize` never reaches
/// it, as Codex's `optOutNotificationMethods`; another connection, and
/// requests and responses, are unaffected.
#[tokio::test(flavor = "multi_thread")]
async fn a_connection_opts_out_of_notifications_it_names() {
    let server = model(vec![text("streamed reply"), text("again")]).await;
    let koda = Server::start(config(&server), Duration::from_secs(60)).await;
    let mut quiet = Client::ready_with(
        &koda,
        json!({"optOutNotificationMethods": ["item/agentMessage/delta"]}),
    )
    .await;
    let mut loud = Client::ready(&koda).await;

    let mut heard = Vec::new();
    for client in [&mut quiet, &mut loud] {
        let session = client.start_session().await;
        client
            .request(
                "turn/start",
                json!({"sessionId": session, "input": [{"type": "text", "text": "hi"}]}),
            )
            .await
            .unwrap();
        let mut lines = client.seen();
        lines.extend(client.until_method("turn/completed").await);
        heard.push(lines);
    }
    let deltas = |lines: &[Value]| {
        lines
            .iter()
            .filter(|l| l["method"] == "item/agentMessage/delta")
            .count()
    };
    assert_eq!(deltas(&heard[0]), 0, "{:?}", heard[0]);
    assert!(deltas(&heard[1]) > 0, "{:?}", heard[1]);
    // The reply still arrives whole on the item that completes it.
    assert!(
        heard[0]
            .iter()
            .any(|l| l["method"] == "item/completed"
                && l["params"]["item"]["text"] == "streamed reply"),
        "{:?}",
        heard[0]
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

/// `fuzzyFileSearch` over the socket: the client's roots, searched by the
/// server, in Codex's shape; nothing outside them.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_searches_its_folder_for_files() {
    let server = model(vec![]).await;
    let koda = Server::start(config(&server), Duration::from_secs(60)).await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("app");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/main.rs"), "").unwrap();
    std::fs::write(root.join("README.md"), "").unwrap();
    std::fs::write(dir.path().join("main-outside.rs"), "").unwrap();
    let root = root.display().to_string();
    let mut client = Client::ready(&koda).await;

    let found = client
        .request(
            "fuzzyFileSearch",
            json!({"query": "main", "roots": [root], "cancellationToken": "composer"}),
        )
        .await
        .unwrap();
    assert_eq!(
        found,
        json!({"files": [{
            "root": root,
            "path": "src/main.rs",
            "match_type": "file",
            "file_name": "main.rs",
            "score": found["files"][0]["score"],
            "indices": [4, 5, 6, 7],
        }]})
    );
    let folder = client
        .request("fuzzyFileSearch", json!({"query": "src", "roots": [root]}))
        .await
        .unwrap();
    assert_eq!(folder["files"][0]["path"], "src");
    assert_eq!(folder["files"][0]["match_type"], "directory");
    let empty = client
        .request("fuzzyFileSearch", json!({"query": "", "roots": [root]}))
        .await
        .unwrap();
    assert_eq!(empty, json!({"files": []}));
    let relative = client
        .request(
            "fuzzyFileSearch",
            json!({"query": "main", "roots": ["app"]}),
        )
        .await;
    assert!(relative.is_err(), "a relative root is refused");
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
            json!({"sessionId": session, "input": [{"type": "text", "text": "write it"}]}),
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

/// A client that joins a session mid-turn is told the turn as it stands,
/// with the tool call still open in it, then the approval it waits on, then
/// everything after, each once, as Codex merges a thread's active turn into
/// `thread/resume`'s answer.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_joins_mid_turn_gets_the_turn_as_it_stands() {
    let server = model(vec![write_file("w1", "a.txt"), text("Written.")]).await;
    let koda = Server::start(safe(&server), Duration::from_secs(60)).await;
    let mut first = Client::ready(&koda).await;
    let session = first.start_session().await;
    first
        .request(
            "turn/start",
            json!({"sessionId": session, "input": [{"type": "text", "text": "write it"}]}),
        )
        .await
        .unwrap();
    let asked = first.until_method("item/toolCall/requestApproval").await;
    let request_id = asked.last().unwrap()["id"].clone();

    // The first client stays: the session is open and its turn running.
    let mut second = Client::connect(&koda).await;
    let init = second
        .send_request(
            "initialize",
            json!({"clientInfo": {"name": "joiner", "version": "0"}}),
        )
        .await;
    second.response(init).await.unwrap();
    let join = second
        .send_request("session/start", json!({"sessionId": session}))
        .await;
    // Nothing of the session reaches it before the answer.
    let mut before = Vec::new();
    let joined = loop {
        let line = second.read().await.expect("the join is answered");
        if line["id"] == join {
            break line["result"].clone();
        }
        before.push(line);
    };
    assert!(
        before
            .iter()
            .all(|l| session_of(l) != Some(session.as_str()) || l["method"] == "session/started"),
        "{before:?}"
    );

    let turns = joined["session"]["turns"].as_array().unwrap();
    let turn = turns.last().unwrap();
    assert_eq!(turn["status"], "inProgress", "{turn}");
    let items = turn["items"].as_array().unwrap();
    let kinds: Vec<&str> = items
        .iter()
        .map(|i| i["item"]["type"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["userMessage", "toolCall"], "{turn}");
    let call = &items[1];
    assert_eq!(call["item"]["status"], "inProgress", "{call}");
    assert_eq!(call["item"]["id"], "w1");
    assert!(call["completedAtMs"].is_null(), "{call}");
    assert_eq!(joined["session"]["model"], "stub", "as it runs: {joined}");
    assert_eq!(
        joined["session"]["status"],
        json!({"type": "active", "activeFlags": ["waitingOnApproval"]}),
        "{joined}"
    );

    // What its turns run with, as `session/settings/updated` says it.
    let settings = &joined["session"]["settings"];
    assert_eq!(settings["model"], "stub", "{joined}");
    assert_eq!(settings["mode"], "safe", "{joined}");

    // Then what it wasn't there to hear (the token counts, on this turn),
    // the approval still waiting, and once that is answered, the call's
    // end and the rest of the turn, the call's start not again.
    let resent = second.until_method("item/toolCall/requestApproval").await;
    assert_eq!(resent.last().unwrap()["id"], request_id);
    let usage = resent
        .iter()
        .find(|l| l["method"] == "session/tokenUsage/updated")
        .expect("the token counts follow the answer");
    assert_eq!(usage["params"]["turnId"], turn["id"], "{usage}");
    assert!(
        usage["params"]["tokenUsage"]["total"]["totalTokens"].as_u64() > Some(0),
        "{usage}"
    );
    first
        .answer(&request_id, json!({"decision": {"type": "approve"}}))
        .await;
    let rest = second.until_method("turn/completed").await;
    let of_the_call: Vec<&str> = rest
        .iter()
        .filter(|l| l["params"]["item"]["id"] == "w1")
        .map(|l| l["method"].as_str().unwrap())
        .collect();
    assert_eq!(of_the_call, ["item/completed"], "{rest:?}");
    assert!(
        rest.iter()
            .any(|l| l["method"] == "serverRequest/resolved"
                && l["params"]["requestId"] == request_id),
        "{rest:?}"
    );
    koda.stop().await;
}

/// A client that only reads an open session mid-turn gets the running turn
/// as a joiner would, its open tool call included, from `session/read` and
/// `session/turns/list` alike, as Codex's `thread/read` merges a thread's
/// active turn.
#[tokio::test(flavor = "multi_thread")]
async fn reading_a_session_mid_turn_has_its_open_items() {
    let server = model(vec![write_file("w1", "a.txt"), text("Written.")]).await;
    let koda = Server::start(safe(&server), Duration::from_secs(60)).await;
    let mut first = Client::ready(&koda).await;
    let session = first.start_session().await;
    first
        .request(
            "turn/start",
            json!({"sessionId": session, "input": [{"type": "text", "text": "write it"}]}),
        )
        .await
        .unwrap();
    let asked = first.until_method("item/toolCall/requestApproval").await;
    let request_id = asked.last().unwrap()["id"].clone();

    let mut reader = Client::ready(&koda).await;
    let read = reader
        .request(
            "session/read",
            json!({"sessionId": session, "includeTurns": true}),
        )
        .await
        .unwrap();
    let listed = reader
        .request("session/turns/list", json!({"sessionId": session}))
        .await
        .unwrap();
    for turn in [
        read["session"]["turns"].as_array().unwrap().last().unwrap(),
        &listed["data"][0],
    ] {
        assert_eq!(turn["status"], "inProgress", "{turn}");
        let items = turn["items"].as_array().unwrap();
        let kinds: Vec<&str> = items
            .iter()
            .map(|i| i["item"]["type"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["userMessage", "toolCall"], "{turn}");
        assert_eq!(items[1]["item"]["status"], "inProgress", "{turn}");
        assert!(items[1]["completedAtMs"].is_null(), "{turn}");
    }

    first
        .answer(&request_id, json!({"decision": {"type": "approve"}}))
        .await;
    first.until_method("turn/completed").await;
    let listed = reader
        .request("session/turns/list", json!({"sessionId": session}))
        .await
        .unwrap();
    let turn = &listed["data"][0];
    assert_eq!(turn["status"], "completed", "{turn}");
    assert_eq!(turn["items"][1]["item"]["status"], "completed", "{turn}");
    koda.stop().await;
}

/// Every client hears what a session is doing as it changes, the ones that
/// never opened it too, and a list says it, as Codex's `thread/status/changed`
/// and `Thread.status` do.
#[tokio::test(flavor = "multi_thread")]
async fn every_client_hears_what_a_session_is_doing() {
    let server = model(vec![write_file("w1", "a.txt"), text("Written.")]).await;
    let koda = Server::start(safe(&server), Duration::from_millis(300)).await;
    let mut owner = Client::ready(&koda).await;
    let mut watcher = Client::ready(&koda).await;
    let session = owner.start_session().await;
    let status_of = |listed: &Value| listed["data"][0]["status"].clone();
    let listed = watcher.request("session/list", json!({})).await.unwrap();
    assert_eq!(status_of(&listed), json!({"type": "idle"}), "{listed}");

    owner
        .request(
            "turn/start",
            json!({"sessionId": session, "input": [{"type": "text", "text": "write it"}]}),
        )
        .await
        .unwrap();
    let asked = owner.until_method("item/toolCall/requestApproval").await;
    let request_id = asked.last().unwrap()["id"].clone();
    // It reaches the watcher after the request that caused it reached the
    // owner; a list then says the same.
    let waiting = json!({"type": "active", "activeFlags": ["waitingOnApproval"]});
    let mut seen = Vec::new();
    while seen.last() != Some(&waiting) {
        let heard = watcher.until_method("session/status/changed").await;
        let last = heard.last().unwrap();
        assert_eq!(session_of(last), Some(session.as_str()), "{last}");
        seen.push(last["params"]["status"].clone());
    }
    let listed = watcher.request("session/list", json!({})).await.unwrap();
    assert_eq!(status_of(&listed), waiting, "{listed}");

    owner
        .answer(&request_id, json!({"decision": {"type": "approve"}}))
        .await;
    owner.until_method("turn/completed").await;
    drop(owner);
    // Answered, the turn runs on and ends; with no client it then closes.
    let gone = json!({"type": "notLoaded"});
    while seen.last() != Some(&gone) {
        let heard = watcher.until_method("session/status/changed").await;
        seen.push(heard.last().unwrap()["params"]["status"].clone());
    }
    assert_eq!(
        seen,
        [
            json!({"type": "active", "activeFlags": []}),
            waiting,
            json!({"type": "active", "activeFlags": []}),
            json!({"type": "idle"}),
            gone,
        ]
    );
    let listed = watcher.request("session/list", json!({})).await.unwrap();
    assert_eq!(status_of(&listed), json!({"type": "notLoaded"}), "{listed}");
    koda.stop().await;
}

/// A session's requests take effect in the order they were sent, as Codex
/// queues a thread's: of several updates sent without waiting, each is said
/// in the order sent, and the last is what the session ends with.
#[tokio::test(flavor = "multi_thread")]
async fn a_sessions_requests_take_effect_in_the_order_sent() {
    let server = model(vec![]).await;
    let koda = Server::start(safe(&server), Duration::from_secs(60)).await;
    let mut client = Client::ready(&koda).await;
    let session = client.start_session().await;
    let modes = ["plan", "safe", "plan", "safe", "plan", "safe", "plan"];
    let mut sent = Vec::new();
    for mode in modes {
        sent.push(
            client
                .send_request(
                    "session/settings/update",
                    json!({"sessionId": session, "mode": mode}),
                )
                .await,
        );
    }
    for id in sent {
        client.response(id).await.unwrap();
    }
    client.request("state/read", json!({})).await.unwrap();
    let said: Vec<String> = client
        .seen()
        .iter()
        .filter(|l| l["method"] == "session/settings/updated")
        .map(|l| {
            l["params"]["sessionSettings"]["mode"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(said, modes, "each said in the order sent");
    koda.stop().await;
}

/// Changing the mode with `turn/start` is a change of settings like any
/// other: the session's clients hear it.
#[tokio::test(flavor = "multi_thread")]
async fn a_mode_change_on_turn_start_is_said() {
    let server = model(vec![text("ok")]).await;
    let koda = Server::start(safe(&server), Duration::from_secs(60)).await;
    let mut client = Client::ready(&koda).await;
    let session = client.start_session().await;
    client
        .request(
            "turn/start",
            json!({"sessionId": session, "input": [{"type": "text", "text": "hi"}], "mode": "plan"}),
        )
        .await
        .unwrap();
    let heard = client.until_method("turn/completed").await;
    let updated = heard
        .iter()
        .find(|l| l["method"] == "session/settings/updated")
        .expect("the new mode is said");
    assert_eq!(
        updated["params"]["sessionSettings"]["mode"], "plan",
        "{updated}"
    );
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
            json!({"sessionId": session, "input": [{"type": "text", "text": "write it"}]}),
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
            json!({"sessionId": session, "input": [{"type": "text", "text": "read the notes"}]}),
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
                title: None,
            },
            capabilities: None,
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
            input: vec![koda_server::protocol::turn::UserInput::text("write it")],
            client_user_message_id: None,
            mode: None,
            output_schema: None,
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

/// A newer Koda replaces a server whose clients are only connected: none
/// of them can go on with a server of another version, so they don't hold
/// it back, and each is told before it goes.
#[tokio::test(flavor = "multi_thread")]
async fn a_newer_koda_replaces_a_server_whose_clients_are_idle() {
    let server = model(vec![]).await;
    let mut koda = Server::start(config(&server), Duration::from_secs(60)).await;
    let mut other = Client::ready(&koda).await;
    other.start_session().await;
    let mut newer = Client::ready(&koda).await;

    let answer = newer
        .request("serve/stop", json!({"ifIdle": true, "forUpdate": "9.9.9"}))
        .await
        .unwrap();
    assert_eq!(answer, json!({"stopped": true}), "{answer}");
    let told = other.until_method("serve/updating").await;
    assert_eq!(
        told.last().unwrap()["params"],
        json!({"version": "9.9.9", "working": 0})
    );
    tokio::time::timeout(WAIT, &mut koda.served)
        .await
        .expect("koda serve didn't stop")
        .unwrap()
        .unwrap();
    assert!(!koda.socket.exists());
}

/// A running turn holds it back: asked only if idle, the server says how
/// many sessions are working and changes nothing; asked to wait, it
/// refuses new turns with Codex's draining error, tells every client, and
/// stops once the turn ends.
#[tokio::test(flavor = "multi_thread")]
async fn a_newer_koda_waits_for_running_turns_when_asked() {
    let server = model(vec![write_file("w1", "a.txt"), text("Written.")]).await;
    let mut koda = Server::start(safe(&server), Duration::from_secs(60)).await;
    let mut other = Client::ready(&koda).await;
    let session = other.start_session().await;
    other
        .request(
            "turn/start",
            json!({"sessionId": session, "input": [{"type": "text", "text": "write it"}]}),
        )
        .await
        .unwrap();
    let asked = other.until_method("item/toolCall/requestApproval").await;
    let request_id = asked.last().unwrap()["id"].clone();

    let mut newer = Client::ready(&koda).await;
    let busy = newer
        .request("serve/stop", json!({"ifIdle": true, "forUpdate": "9.9.9"}))
        .await
        .unwrap();
    assert_eq!(busy["stopped"], false, "{busy}");
    assert_eq!(busy["working"], 1, "{busy}");
    assert!(busy.get("draining").is_none(), "{busy}");
    // Nothing changed: new turns still start elsewhere.
    let second = other.start_session().await;
    other
        .request(
            "session/settings/update",
            json!({"sessionId": second, "mode": "plan"}),
        )
        .await
        .unwrap();

    let draining = newer
        .request(
            "serve/stop",
            json!({"forUpdate": "9.9.9", "afterTurns": true}),
        )
        .await
        .unwrap();
    assert_eq!(draining["stopped"], false, "{draining}");
    assert_eq!(draining["draining"], true, "{draining}");
    let told = other.until_method("serve/updating").await;
    assert_eq!(
        told.last().unwrap()["params"],
        json!({"version": "9.9.9", "working": 1})
    );
    let refused = other
        .request(
            "turn/start",
            json!({"sessionId": second, "input": [{"type": "text", "text": "anything"}]}),
        )
        .await
        .unwrap_err();
    assert_eq!(refused["data"]["reason"], "serverShuttingDown", "{refused}");
    assert_eq!(
        refused["message"],
        "Server is draining; retry after reconnecting"
    );
    // Another newer client finds it already waiting.
    let mut another = Client::ready(&koda).await;
    let again = another
        .request("serve/stop", json!({"ifIdle": true, "forUpdate": "9.9.9"}))
        .await
        .unwrap();
    assert_eq!(again["draining"], true, "{again}");
    assert!(!koda.served.is_finished(), "it waits for the turn");

    other
        .answer(&request_id, json!({"decision": {"type": "approve"}}))
        .await;
    let rest = other.until_method("turn/completed").await;
    assert_eq!(
        rest.last().unwrap()["params"]["turn"]["status"],
        "completed"
    );
    tokio::time::timeout(WAIT, &mut koda.served)
        .await
        .expect("koda serve didn't stop after the turn")
        .unwrap()
        .unwrap();
    assert!(!koda.socket.exists());
}

/// Told to stop now, the server stops the running turn: stored, it reads
/// as interrupted when the session is opened again.
#[tokio::test(flavor = "multi_thread")]
async fn a_newer_koda_can_stop_running_turns() {
    let dir = tempfile::tempdir().unwrap();
    let server = model(vec![write_file("w1", "a.txt"), text("Written.")]).await;
    let mut koda = Server::start_in(dir.path(), safe(&server), Duration::from_secs(60)).await;
    let mut other = Client::ready(&koda).await;
    let session = other.start_session().await;
    other
        .request(
            "turn/start",
            json!({"sessionId": session, "input": [{"type": "text", "text": "write it"}]}),
        )
        .await
        .unwrap();
    other.until_method("item/toolCall/requestApproval").await;

    let mut newer = Client::ready(&koda).await;
    let answer = newer
        .request("serve/stop", json!({"forUpdate": "9.9.9"}))
        .await
        .unwrap();
    assert_eq!(answer["stopped"], true, "{answer}");
    assert_eq!(answer["working"], 1, "{answer}");
    let told = other.until_method("serve/updating").await;
    assert_eq!(told.last().unwrap()["params"]["working"], 0);
    tokio::time::timeout(WAIT, &mut koda.served)
        .await
        .expect("koda serve didn't stop")
        .unwrap()
        .unwrap();

    let koda = Server::start_in(dir.path(), safe(&server), Duration::from_secs(60)).await;
    let mut reader = Client::ready(&koda).await;
    let read = reader
        .request(
            "session/read",
            json!({"sessionId": session, "includeTurns": true}),
        )
        .await
        .unwrap();
    assert_eq!(
        read["session"]["turns"][0]["status"], "interrupted",
        "{read}"
    );
    koda.stop().await;
}

/// A client that hangs up while its `session/start` is being answered
/// doesn't stay the session's subscriber: another client opens the session,
/// and once that one leaves it is idle, so the server may stop.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_leaves_while_opening_a_session_doesnt_keep_it() {
    let server = model(vec![]).await;
    let mut koda = Server::start(config(&server), Duration::from_millis(300)).await;
    let mut first = Client::ready(&koda).await;
    let session = first.start_session().await;
    drop(first);
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // It opens the stored session and hangs up before the answer.
    let mut leaver = Client::ready(&koda).await;
    leaver
        .send_request("session/start", json!({"sessionId": session}))
        .await;
    drop(leaver);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut next = Client::ready(&koda).await;
    let opened = next
        .request("session/start", json!({"sessionId": session}))
        .await
        .unwrap_or_else(|e| panic!("the session can't be opened again: {e}"));
    assert_eq!(opened["session"]["id"], session.as_str());
    drop(next);

    tokio::time::sleep(Duration::from_millis(1500)).await;
    let mut asking = Client::ready(&koda).await;
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
}

/// A session records who started it, by the name its connection gave, and
/// a fork what it was forked from and that it isn't one to keep.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_records_the_client_that_started_it() {
    use koda_state::session_store::SessionStore;
    let server = model(vec![]).await;
    let koda = Server::start(config(&server), Duration::from_secs(60)).await;
    let mut client = Client::ready(&koda).await;
    let session = client.start_session().await;
    let fork = client
        .request(
            "session/fork",
            json!({"sessionId": session, "ephemeral": true}),
        )
        .await
        .unwrap()["session"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let home = koda._dir.as_ref().unwrap().path().join("config");
    let db = koda_state::db::Database::init(&home).await.unwrap();
    let started = db.get_session(&session).await.unwrap().unwrap();
    assert_eq!(started.source.as_deref(), Some("appServer"));
    assert_eq!(started.originator.as_deref(), Some("socket-test"));
    assert!(!started.ephemeral);
    let forked = db.get_session(&fork).await.unwrap().unwrap();
    assert_eq!(forked.forked_from_id.as_deref(), Some(session.as_str()));
    assert_eq!(forked.originator.as_deref(), Some("socket-test"));
    assert!(forked.ephemeral);
    koda.stop().await;
}

/// A client that lists sessions hears of a rename and a delete another
/// client makes, of a session it never opened.
#[tokio::test(flavor = "multi_thread")]
async fn every_client_hears_a_rename_and_a_delete() {
    let server = model(vec![]).await;
    let koda = Server::start(config(&server), Duration::from_secs(60)).await;
    let mut owner = Client::ready(&koda).await;
    let mut watcher = Client::ready(&koda).await;
    let session = owner.start_session().await;

    owner
        .request(
            "session/name/set",
            json!({"sessionId": session, "name": "renamed"}),
        )
        .await
        .unwrap();
    let renamed = watcher.until_method("session/name/updated").await;
    let last = renamed.last().unwrap();
    assert_eq!(session_of(last), Some(session.as_str()), "{last}");

    owner
        .request("session/delete", json!({"sessionId": session}))
        .await
        .unwrap();
    let deleted = watcher.until_method("session/deleted").await;
    assert_eq!(session_of(deleted.last().unwrap()), Some(session.as_str()));
    koda.stop().await;
}

/// `session/list` covers every folder unless told one, leaves out what a
/// person didn't start unless asked, and pages from the last session.
#[tokio::test(flavor = "multi_thread")]
async fn sessions_are_listed_across_folders_by_kind() {
    let server = model(vec![]).await;
    let koda = Server::start(config(&server), Duration::from_secs(60)).await;
    let dir = koda._dir.as_ref().unwrap().path().to_path_buf();
    let other = dir.join("other");
    std::fs::create_dir_all(&other).unwrap();
    let mut client = Client::ready(&koda).await;
    let here = client.start_session().await;
    let started = client
        .request("session/start", json!({"cwd": other}))
        .await
        .unwrap();
    let there = started["session"]["id"].as_str().unwrap().to_string();
    // A folder is listed by the `cwd` its sessions report: links aren't
    // followed.
    let other = started["session"]["cwd"].as_str().unwrap().to_string();
    let side = client
        .request(
            "session/fork",
            json!({"sessionId": here, "ephemeral": true}),
        )
        .await
        .unwrap()["session"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let ids = |listed: &Value| -> Vec<String> {
        listed["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap().to_string())
            .collect()
    };
    let all = client.request("session/list", json!({})).await.unwrap();
    let listed = ids(&all);
    assert!(listed.contains(&here) && listed.contains(&there), "{all}");
    assert!(
        !listed.contains(&side),
        "an ephemeral session isn't listed: {all}"
    );
    assert_eq!(all["data"][0]["source"], "appServer", "{all}");
    assert_eq!(all["data"][0]["model"], "stub", "{all}");

    let one_folder = client
        .request("session/list", json!({"cwd": other}))
        .await
        .unwrap();
    assert_eq!(ids(&one_folder), std::slice::from_ref(&there));
    let both = client
        .request(
            "session/list",
            json!({"cwd": [other, all["data"][1]["cwd"]]}),
        )
        .await
        .unwrap();
    assert_eq!(ids(&both).len(), 2, "{both}");

    // Nothing here came from `koda exec`.
    let exec = client
        .request("session/list", json!({"sourceKinds": ["exec"]}))
        .await
        .unwrap();
    assert!(ids(&exec).is_empty(), "{exec}");

    // One per page, oldest first: the second page goes on from the first.
    let page = json!({"limit": 1, "sortDirection": "asc", "sortKey": "created_at"});
    let first = client.request("session/list", page.clone()).await.unwrap();
    assert_eq!(ids(&first).len(), 1);
    let mut next = page;
    next["cursor"] = first["nextCursor"].clone();
    let second = client.request("session/list", next).await.unwrap();
    assert_eq!(ids(&second).len(), 1);
    assert_ne!(ids(&first), ids(&second));
    assert!(second["nextCursor"].is_null(), "{second}");
    koda.stop().await;
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

/// A client that stops reading (a TUI suspended with `Ctrl+Z`) is let go
/// once its messages pile up, as Codex drops a slow socket client, instead
/// of the server keeping them all; its session stays open for the next
/// client.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_stops_reading_is_let_go_and_its_session_goes_on() {
    const ASKED: usize = 30_000;
    let server = model(vec![]).await;
    let koda = Server::start(config(&server), Duration::from_secs(60)).await;
    let mut stuck = Client::ready(&koda).await;
    let session = stuck.start_session().await;

    // It asks and asks, and reads none of the answers.
    for id in 0..ASKED {
        let line =
            json!({"jsonrpc": "2.0", "id": 1_000 + id, "method": "state/read", "params": {}});
        if stuck
            .input
            .write_all(format!("{line}\n").as_bytes())
            .await
            .is_err()
        {
            break;
        }
    }

    // What its socket already held, then the end, well short of every
    // answer. The last line may be cut off.
    let mut answers = 0;
    let mut line = String::new();
    loop {
        line.clear();
        let read = tokio::time::timeout(WAIT, stuck.output.read_line(&mut line))
            .await
            .expect("the stuck client was never let go");
        match read {
            Ok(0) | Err(_) => break,
            Ok(_) => answers += usize::from(line.contains("\"result\"")),
        }
    }
    assert!(answers < ASKED, "{answers} answers: the client was kept");

    // Its session is still open: the next client attaches to it.
    let mut other = Client::ready(&koda).await;
    let attached = other
        .request("session/start", json!({"sessionId": session}))
        .await
        .unwrap();
    assert_eq!(attached["session"]["id"], session.as_str());
    assert!(
        other
            .seen()
            .iter()
            .all(|l| l["method"] != "session/started"),
        "attached, not started again"
    );
    koda.stop().await;
}

/// A message over 64 MiB, as Codex's WebSocket limit, is refused and ends
/// its connection rather than being read whole; other clients carry on.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_over_the_limit_ends_its_connection() {
    let server = model(vec![]).await;
    let koda = Server::start(config(&server), Duration::from_secs(60)).await;
    let mut big = Client::ready(&koda).await;
    let chunk = vec![b'a'; 1 << 20];
    for _ in 0..65 {
        if big.input.write_all(&chunk).await.is_err() {
            break;
        }
    }
    let refused = big.read().await.expect("an error before the end");
    assert_eq!(refused["error"]["code"], -32600, "{refused}");
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("64 MiB"),
        "{refused}"
    );
    assert_eq!(big.read().await, None, "the connection ends");

    let mut other = Client::ready(&koda).await;
    other.start_session().await;
    koda.stop().await;
}
