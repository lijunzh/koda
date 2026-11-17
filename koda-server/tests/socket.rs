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
        let dir = tempfile::tempdir().unwrap();
        let mut server = Self::start_in(dir.path(), config, unload_delay).await;
        server._dir = Some(dir);
        server
    }

    /// Serve with its state in `dir`, which the caller keeps.
    async fn start_in(dir: &Path, config: KodaConfig, unload_delay: Duration) -> Self {
        let socket = dir.join("serve").join("koda.sock");
        let (stop, stopped) = oneshot::channel::<()>();
        let served = tokio::spawn(serve_with_config(
            args(dir),
            config,
            socket.clone(),
            Options { unload_delay },
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

/// A session outlives the client that opened it, and closes once it has had
/// no client and no turn for the unload delay, as Codex unloads a thread.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_outlives_its_client_until_it_is_idle() {
    let server = model(vec![]).await;
    let koda = Server::start(config(&server), Duration::from_millis(800)).await;
    let mut first = Client::ready(&koda).await;
    let session = first.start_session().await;
    drop(first);

    // Still open: it can't be opened again yet.
    let mut second = Client::ready(&koda).await;
    let early = second
        .request("session/start", json!({"sessionId": session}))
        .await;
    assert!(
        early
            .as_ref()
            .is_err_and(|e| e.to_string().contains("already open")),
        "{early:?}"
    );

    // Closed once idle past the delay: it opens again, for this client.
    let deadline = Instant::now() + WAIT;
    loop {
        tokio::time::sleep(Duration::from_millis(300)).await;
        match second
            .request("session/start", json!({"sessionId": session}))
            .await
        {
            Ok(reopened) => {
                assert_eq!(reopened["session"]["id"], session.as_str());
                break;
            }
            Err(e) => {
                assert!(Instant::now() < deadline, "never closed: {e}");
            }
        }
    }
    // `session/unsubscribe` leaves it to the same rule.
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
