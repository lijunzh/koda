//! `koda serve`: Koda's shared server on a Unix socket, which several
//! clients use at once, as `codex app-server --listen unix://` serves them.
//!
//! One host serves every connection. Each connection speaks what
//! `koda serve --stdio` speaks, one JSON-RPC message per line each way
//! ([`crate::stdio`]); Codex frames its socket as WebSocket, but Koda's
//! clients are local programs, and one framing for both transports keeps a
//! client's code the same for either.
//!
//! - **Routing, as Codex's:** a connection is subscribed to the sessions it
//!   starts or forks. A session's notifications and requests go to its
//!   subscribers; one with no session, to every connection. What a new
//!   session sends before its connection has the answer is held for it.
//! - **A session outlives its client:** a connection that ends, or sends
//!   `session/unsubscribe`, leaves the session open, and a running turn goes
//!   on. Once a session has had no subscriber and no running turn for a
//!   minute (Codex's `thread_unload_delay`), Koda closes it.
//! - **Answers:** each request is answered on the connection that sent it;
//!   a client's answer to a server request (an approval, a question) is taken
//!   from any connection, and the first wins.
//! - **One server per folder, private to the user, as Codex's:** the socket
//!   lives in a folder only its owner may change, is readable by its owner
//!   only, and a lock keeps a second `koda serve` from taking it over. A
//!   connection from another user is refused.
//! - **A client that stops reading is let go, as Codex's is:** once
//!   4,096 messages wait for it (a TUI suspended with
//!   `Ctrl+Z`, a client that hangs), the server drops what waited and closes
//!   the connection, so one stuck client can't grow the server without end.
//!   Its sessions go on; reattaching reads them again and resends their
//!   unanswered requests. Messages over 64 MiB end a connection too
//!   ([`crate::stdio`]).
//! - **`initialize`:** Koda's host is initialized once, when the server
//!   starts; each connection still sends `initialize` first and gets the same
//!   answer, followed by the config file's warnings.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use koda_core::config::KodaConfig;
use koda_protocol::rpc::{self, ErrorObject, ErrorResponse, Message, RequestId, codes};
use koda_protocol::session::{SessionUnsubscribeParams, SessionUnsubscribeStatus};
use koda_protocol::{Method, methods};
use serde_json::Value;
use tokio::io::BufReader;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;

use crate::host::HostCommand;
use crate::in_process::{HostStart, RequestError, RequestHandle, launch, stop_host};
use crate::stdio::{
    Outbox, Read, ServeArgs, error_line, parse, read_message, too_long_line, write_lines,
};

/// The socket's file name, in [`default_socket_path`]'s folder.
const SOCKET_FILE: &str = "koda.sock";
/// The lock that keeps one server per folder.
const LOCK_FILE: &str = "koda-serve.lock";
/// How long a session with no subscriber and no running turn stays open,
/// as Codex's `thread_unload_delay` defaults to.
const UNLOAD_DELAY: Duration = Duration::from_secs(60);
/// How long a request waits for a client unless `approval_wait_timeout_secs`
/// says otherwise: a day, so an approval asked overnight is still there in
/// the morning.
const APPROVAL_WAIT: Duration = Duration::from_secs(24 * 60 * 60);
/// How many messages a new session may send before its connection has the
/// answer that subscribes it; more are dropped, with a warning.
const HELD_LIMIT: usize = 1_000;
/// How many messages may wait for one client before the server lets it go.
/// Codex lets a socket client go at 128; Koda waits longer because a
/// session's first messages, up to [`HELD_LIMIT`], reach its client at once
/// when it subscribes, and a reattaching client is resent what waits for it.
pub(crate) const SLOW_CLIENT_LIMIT: usize = 4_096;

/// Where `koda serve` listens unless told otherwise: `serve/koda.sock` in
/// Koda's folder (`~/.config/koda`).
pub fn default_socket_path(home: &Path) -> PathBuf {
    home.join("serve").join(SOCKET_FILE)
}

/// Why the server couldn't start.
#[derive(Debug)]
pub enum ServeError {
    /// Koda's host didn't start.
    Start(RequestError),
    /// Another `koda serve` holds the socket.
    InUse(PathBuf),
    /// The socket or its folder couldn't be made safely.
    Socket {
        /// The socket.
        path: PathBuf,
        /// What went wrong.
        error: std::io::Error,
    },
}

impl std::fmt::Display for ServeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Start(error) => error.fmt(f),
            Self::InUse(path) => write!(
                f,
                "another koda serve is already listening at {}",
                path.display()
            ),
            Self::Socket { path, error } => {
                write!(f, "couldn't listen at {}: {error}", path.display())
            }
        }
    }
}

impl std::error::Error for ServeError {}

/// What a test may change.
#[doc(hidden)]
pub struct Options {
    /// How long an idle session with no subscriber stays open.
    pub unload_delay: Duration,
    /// Stop once the server has had no client and no work this long, as a
    /// server the TUI started does (`koda serve --idle-exit-secs`); `None`
    /// runs until stopped.
    pub idle_exit: Option<Duration>,
    /// How long a request waits for an answer; the config file's when
    /// `None`.
    pub approval_wait: Option<Duration>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            unload_delay: UNLOAD_DELAY,
            idle_exit: None,
            approval_wait: None,
        }
    }
}

/// Listen at `socket` until `stop` completes, then stop every session and
/// remove the socket. New sessions start from the config file.
///
/// With `idle_exit`, it also stops once it has had no client and no work
/// that long, as a server the TUI started does.
pub async fn serve(
    args: ServeArgs,
    socket: PathBuf,
    idle_exit: Option<Duration>,
    stop: impl Future<Output = ()> + Send,
) -> Result<(), ServeError> {
    let options = Options {
        idle_exit,
        ..Options::default()
    };
    serve_on(args, None, socket, options, stop).await
}

/// [`serve`] with new sessions starting from `config`, for tests that need
/// settings no key gives (a scripted server's address).
#[doc(hidden)]
pub async fn serve_with_config(
    args: ServeArgs,
    config: KodaConfig,
    socket: PathBuf,
    options: Options,
    stop: impl Future<Output = ()> + Send,
) -> Result<(), ServeError> {
    serve_on(args, Some(config), socket, options, stop).await
}

async fn serve_on(
    args: ServeArgs,
    config: Option<KodaConfig>,
    socket: PathBuf,
    options: Options,
    stop: impl Future<Output = ()> + Send,
) -> Result<(), ServeError> {
    let (listener, _lock) = bind(&socket)?;
    // Its sessions' commands run as the user too, and must not be clients.
    koda_sandbox::protect_socket(&socket);
    let approval_wait = options.approval_wait.unwrap_or_else(|| {
        args.config_file
            .load()
            .ok()
            .and_then(|loaded| loaded.config.approval_wait_timeout_secs)
            .map_or(APPROVAL_WAIT, Duration::from_secs)
    });
    let launched = launch(
        HostStart {
            config_file: args.config_file,
            project_root: args.project_root,
            home: args.home,
        },
        config,
        None,
    )
    .await
    .map_err(ServeError::Start)?;
    let handle = launched.requests.clone();
    let initialized = handle
        .request::<methods::Initialize>(koda_protocol::initialize::InitializeParams {
            client_info: koda_protocol::initialize::ClientInfo {
                name: "koda-serve".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                title: None,
            },
            capabilities: None,
        })
        .await
        .map_err(ServeError::Start)?;
    let initialized = serde_json::to_value(initialized).unwrap_or_default();

    let router = Arc::new(Router {
        state: Mutex::default(),
        handle: handle.clone(),
        unload_delay: options.unload_delay,
        approval_wait,
        stopping: tokio::sync::watch::Sender::new(false),
        stop_requested: tokio::sync::Notify::new(),
    });
    let pump = {
        let router = Arc::clone(&router);
        let mut events = launched.events;
        tokio::spawn(async move {
            while let Some(message) = events.recv().await {
                match message.to_json() {
                    Ok(line) => router.deliver(line),
                    Err(e) => tracing::error!("koda serve: a message didn't encode: {e}"),
                }
            }
        })
    };

    tracing::info!("koda serve: listening at {}", socket.display());
    let own_uid = own_uid();
    let next_connection = AtomicU64::new(1);
    let mut connections = tokio::task::JoinSet::new();
    tokio::pin!(stop);
    // How often the idle exit is checked: a quarter of it, at most every 15 s.
    let mut idle_check = tokio::time::interval(
        options
            .idle_exit
            .map_or(Duration::from_secs(15), |idle| {
                (idle / 4).min(Duration::from_secs(15))
            })
            .max(Duration::from_millis(10)),
    );
    let mut idle_since: Option<std::time::Instant> = None;
    loop {
        tokio::select! {
            () = &mut stop => break,
            () = router.stop_requested.notified() => {
                tracing::info!("koda serve: a client asked it to stop");
                break;
            }
            _ = idle_check.tick(), if options.idle_exit.is_some() => {
                let idle = connections.is_empty() && router.busy_reason(None).is_none();
                match (idle, idle_since) {
                    (false, _) => idle_since = None,
                    (true, None) => idle_since = Some(std::time::Instant::now()),
                    (true, Some(since)) => {
                        if options.idle_exit.is_some_and(|limit| since.elapsed() >= limit) {
                            tracing::info!("koda serve: idle, stopping");
                            break;
                        }
                    }
                }
            }
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    match stream.peer_cred() {
                        Ok(peer) if peer.uid() == own_uid => {}
                        Ok(peer) => {
                            tracing::warn!("koda serve: refused a connection from user {}", peer.uid());
                            continue;
                        }
                        Err(e) => {
                            tracing::warn!("koda serve: refused a connection with no credentials: {e}");
                            continue;
                        }
                    }
                    let id = next_connection.fetch_add(1, Ordering::Relaxed);
                    connections.spawn(connection(
                        id,
                        stream,
                        Arc::clone(&router),
                        initialized.clone(),
                    ));
                }
                Err(e) => tracing::warn!("koda serve: accepting a connection failed: {e}"),
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }

    tracing::info!("koda serve: stopping");
    router.stopping.send_replace(true);
    stop_host(&handle.commands, launched.host).await;
    drop(handle);
    let _ = tokio::time::timeout(Duration::from_secs(5), pump).await;
    router.close_all();
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    connections.abort_all();
    let _ = std::fs::remove_file(&socket);
    Ok(())
}

/// The user this process runs as.
fn own_uid() -> u32 {
    // SAFETY: `geteuid` has no preconditions and can't fail.
    unsafe { libc::geteuid() }
}

/// Make the socket's folder private, take the lock, clear a stale socket and
/// listen, as Codex's `bind_private_daemon_socket` does. The lock is held
/// until the returned file drops.
fn bind(socket: &Path) -> Result<(UnixListener, std::fs::File), ServeError> {
    let failed = |error: std::io::Error| ServeError::Socket {
        path: socket.to_path_buf(),
        error,
    };
    let folder = socket
        .parent()
        .ok_or_else(|| failed(std::io::Error::other("the socket has no folder")))?;
    match std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(folder)
    {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(failed(e)),
    }
    // Nobody else may put a socket where clients look for Koda's.
    let meta = std::fs::metadata(folder).map_err(failed)?;
    if !meta.is_dir() || meta.uid() != own_uid() || meta.mode() & 0o022 != 0 {
        return Err(failed(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "{} must be a folder only you can change (chmod 700)",
                folder.display()
            ),
        )));
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(folder.join(LOCK_FILE))
        .map_err(failed)?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            return Err(ServeError::InUse(socket.to_path_buf()));
        }
        Err(std::fs::TryLockError::Error(e)) => return Err(failed(e)),
    }
    // With the lock held, a socket left here is a crashed server's.
    match std::fs::symlink_metadata(socket) {
        Ok(meta) if meta.file_type().is_socket() => {
            std::fs::remove_file(socket).map_err(failed)?;
        }
        Ok(_) => {
            return Err(failed(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "something that isn't a socket is in the way",
            )));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(failed(e)),
    }
    let listener = UnixListener::bind(socket).map_err(failed)?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600)).map_err(failed)?;
    Ok((listener, lock))
}

/// The session a message belongs to: `sessionId`, or the session itself
/// for `session/started`.
fn session_of(message: &Value) -> Option<&str> {
    let params = &message["params"];
    params["sessionId"]
        .as_str()
        .or_else(|| params["session"]["id"].as_str())
}

/// What a client attaching to an open session is told.
struct Attached {
    /// The requests still waiting in it.
    waiting: Vec<String>,
    /// The session as it runs.
    live: Option<Value>,
}

/// The stored session `stored` (with its turns) as it runs now: what
/// `live` says of its model, provider, window, mode and tools.
fn with_live(mut stored: Value, live: Option<&Value>) -> Value {
    if let Some(live) = live {
        for key in [
            "model",
            "provider",
            "contextWindow",
            "mode",
            "tools",
            "instructionSources",
        ] {
            if !live[key].is_null() {
                stored[key] = live[key].clone();
            }
        }
    }
    stored
}

/// Which connections get which messages.
struct Router {
    state: Mutex<RouterState>,
    handle: RequestHandle,
    unload_delay: Duration,
    /// How long a request waits for an answer before Koda answers no.
    approval_wait: Duration,
    /// Set once the server stops: every connection ends.
    stopping: tokio::sync::watch::Sender<bool>,
    /// A client asked the server to stop (`serve/stop`).
    stop_requested: tokio::sync::Notify,
}

#[derive(Default)]
struct RouterState {
    /// Connections that sent `initialize`.
    connections: HashMap<u64, Arc<Outbox>>,
    /// Each open session's subscribers.
    subscribers: HashMap<String, HashSet<u64>>,
    /// What a session sent while not open here: a new session's first
    /// messages, before its connection has the answer that subscribes it.
    held: HashMap<String, Vec<String>>,
    /// Sessions with a turn running.
    running: HashSet<String>,
    /// Bumped whenever a session gains a subscriber or a turn, so a pending
    /// unload knows it's stale.
    activity: HashMap<String, u64>,
    /// The config file's warnings, for each connection after `initialize`.
    warnings: Vec<String>,
    /// Sessions open in the host.
    open: HashSet<String>,
    /// Each session's requests nobody has answered yet: their ids and
    /// lines, oldest first, for a client that attaches.
    pending: HashMap<String, Vec<(Value, String)>>,
    /// Each open session as it runs (`session/started`, then its settings
    /// as they change), for a client that attaches.
    live: HashMap<String, Value>,
}

impl Router {
    fn lock(&self) -> MutexGuard<'_, RouterState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Send one of the host's messages to whoever should have it.
    fn deliver(self: &Arc<Self>, line: String) {
        let value: Value = serde_json::from_str(&line).unwrap_or_default();
        let method = value["method"].as_str().unwrap_or_default();
        let session = session_of(&value).map(str::to_string);
        let mut state = self.lock();
        let Some(session) = session else {
            if method == "configWarning" {
                state.warnings.push(line.clone());
            }
            for outbox in state.connections.values() {
                outbox.push(line.clone());
            }
            return;
        };
        // A request waits until a client answers it, Koda resolves it, or
        // the wait runs out; a client that attaches is sent it again.
        if value.get("id").is_some() && !method.is_empty() {
            state
                .pending
                .entry(session.clone())
                .or_default()
                .push((value["id"].clone(), line.clone()));
            self.answer_no_after_wait(&session, value["id"].clone());
        }
        if method == "serverRequest/resolved" {
            let resolved = &value["params"]["requestId"];
            if let Some(pending) = state.pending.get_mut(&session) {
                pending.retain(|(id, _)| id != resolved);
            }
        }
        match method {
            "session/started" => {
                state
                    .live
                    .insert(session.clone(), value["params"]["session"].clone());
            }
            "session/settings/updated" => {
                if let Some(live) = state.live.get_mut(&session) {
                    let settings = &value["params"]["settings"];
                    live["model"] = settings["model"].clone();
                    live["provider"] = settings["modelProvider"].clone();
                    live["contextWindow"] = settings["contextWindow"].clone();
                    live["mode"] = settings["mode"].clone();
                }
            }
            "turn/started" => {
                state.running.insert(session.clone());
                *state.activity.entry(session.clone()).or_default() += 1;
            }
            "turn/completed" => {
                state.running.remove(&session);
            }
            _ => {}
        }
        let subscribers: Vec<Arc<Outbox>> = state
            .subscribers
            .get(&session)
            .into_iter()
            .flatten()
            .filter_map(|id| state.connections.get(id).cloned())
            .collect();
        if !subscribers.is_empty() {
            for outbox in subscribers {
                outbox.push(line.clone());
            }
        } else if !state.open.contains(&session) {
            let held = state.held.entry(session.clone()).or_default();
            if held.len() < HELD_LIMIT {
                held.push(line);
            } else {
                tracing::warn!(
                    "koda serve: dropped a message for session {session}, not open here"
                );
            }
        } else if value.get("id").is_some() {
            tracing::info!("koda serve: {method} waits for a client of session {session}");
        }
        if method == "turn/completed" {
            drop(state);
            self.unload_if_idle(&session);
        }
    }

    /// Subscribe `connection` to `session`, sending it what the session
    /// held. A connection that ended while the session was opening for it
    /// can't subscribe: the session is open with nobody, and closes by the
    /// idle rule, where it would otherwise count as in use for good.
    fn subscribe(self: &Arc<Self>, connection: u64, session: &str) {
        {
            let mut state = self.lock();
            state.open.insert(session.to_string());
            *state.activity.entry(session.to_string()).or_default() += 1;
            let held = state.held.remove(session).unwrap_or_default();
            if let Some(outbox) = state.connections.get(&connection).cloned() {
                state
                    .subscribers
                    .entry(session.to_string())
                    .or_default()
                    .insert(connection);
                for line in held {
                    outbox.push(line);
                }
                return;
            }
        }
        tracing::info!("koda serve: session {session} opened for a client that has left");
        self.unload_if_idle(session);
    }

    /// Why stopping now would lose something, if it would: a session with a
    /// client, a running turn or a request waiting, or a client other than
    /// `asking`. An open session with none of those is stored and opens
    /// again as it was, as Claude Code's supervisor moves idle sessions over.
    fn busy_reason(&self, asking: Option<u64>) -> Option<String> {
        let state = self.lock();
        let working = state.open.iter().find(|session| {
            state.running.contains(*session)
                || state
                    .subscribers
                    .get(*session)
                    .is_some_and(|ids| !ids.is_empty())
                || state.pending.get(*session).is_some_and(|p| !p.is_empty())
        });
        if let Some(session) = working {
            return Some(format!("session {session} is in use"));
        }
        let others = state
            .connections
            .keys()
            .filter(|id| Some(**id) != asking)
            .count();
        (others > 0).then(|| format!("{others} other client(s) connected"))
    }

    /// Attach `connection` to `session` if the host has it open, as Codex
    /// subscribes a client that resumes a loaded thread: the requests still
    /// waiting in it, to send the client once it has the session.
    fn attach(self: &Arc<Self>, connection: u64, session: &str) -> Option<Attached> {
        let attached = {
            let state = self.lock();
            if !state.open.contains(session) {
                return None;
            }
            Attached {
                waiting: state
                    .pending
                    .get(session)
                    .map(|pending| pending.iter().map(|(_, line)| line.clone()).collect())
                    .unwrap_or_default(),
                live: state.live.get(session).cloned(),
            }
        };
        self.subscribe(connection, session);
        Some(attached)
    }

    /// Answer request `id` of `session` no once [`Router::approval_wait`]
    /// has passed, if nobody has: the approval is refused, the question gets
    /// an empty answer, the loop stops.
    fn answer_no_after_wait(self: &Arc<Self>, session: &str, id: Value) {
        let router = Arc::clone(self);
        let session = session.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(router.approval_wait).await;
            let waiting = router
                .lock()
                .pending
                .get(&session)
                .is_some_and(|pending| pending.iter().any(|(pending, _)| *pending == id));
            let Ok(request_id) = serde_json::from_value::<RequestId>(id) else {
                return;
            };
            if !waiting {
                return;
            }
            let secs = router.approval_wait.as_secs();
            tracing::info!(
                "koda serve: no client answered {request_id:?} of session {session} in {secs}s; \
                 answering no"
            );
            let _ = router
                .handle
                .commands
                .send(HostCommand::Response {
                    id: request_id,
                    result: Err(ErrorObject::new(
                        codes::INTERNAL_ERROR,
                        format!("no client answered within {secs}s (approval_wait_timeout_secs)"),
                    )),
                })
                .await;
        });
    }

    /// Unsubscribe `connection` from `session`. `true` if it was subscribed.
    fn unsubscribe(self: &Arc<Self>, connection: u64, session: &str) -> bool {
        let was = self
            .lock()
            .subscribers
            .get_mut(session)
            .is_some_and(|ids| ids.remove(&connection));
        if was {
            self.unload_if_idle(session);
        }
        was
    }

    /// A connection ended: its subscriptions go, its sessions stay.
    fn disconnect(self: &Arc<Self>, connection: u64) {
        let left: Vec<String> = {
            let mut state = self.lock();
            state.connections.remove(&connection);
            state
                .subscribers
                .iter_mut()
                .filter_map(|(session, ids)| ids.remove(&connection).then(|| session.clone()))
                .collect()
        };
        for session in left {
            self.unload_if_idle(&session);
        }
    }

    /// Close `session` after the unload delay if it still has no subscriber
    /// and no running turn by then, as Codex unloads an idle thread.
    fn unload_if_idle(self: &Arc<Self>, session: &str) {
        fn idle(state: &RouterState, session: &str) -> bool {
            state.subscribers.get(session).is_none_or(HashSet::is_empty)
                && !state.running.contains(session)
        }
        let seen = {
            let state = self.lock();
            if !idle(&state, session) {
                return;
            }
            state.activity.get(session).copied().unwrap_or_default()
        };
        let router = Arc::clone(self);
        let session = session.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(router.unload_delay).await;
            {
                let mut state = router.lock();
                let unchanged = state.activity.get(&session).copied().unwrap_or_default() == seen;
                if !unchanged || !idle(&state, &session) {
                    return;
                }
                state.subscribers.remove(&session);
                state.activity.remove(&session);
                state.open.remove(&session);
                state.pending.remove(&session);
                state.live.remove(&session);
            }
            tracing::info!("koda serve: closing idle session {session}");
            if let Err(e) = router
                .handle
                .request::<methods::SessionUnsubscribe>(SessionUnsubscribeParams {
                    session_id: session.clone(),
                })
                .await
            {
                tracing::warn!("koda serve: closing session {session} failed: {e}");
            }
        });
    }

    fn close_all(&self) {
        for outbox in self.lock().connections.values() {
            outbox.close();
        }
    }
}

/// Serve one client until it hangs up or its socket breaks.
async fn connection(id: u64, stream: UnixStream, router: Arc<Router>, initialized: Value) {
    let (input, output) = stream.into_split();
    let outbox = Arc::new(Outbox::bounded(SLOW_CLIENT_LIMIT));
    let writer = tokio::spawn(write_lines(Arc::clone(&outbox), output));
    let mut answering = tokio::task::JoinSet::new();
    let mut ready = false;
    let mut lines = BufReader::new(input);
    let mut line = Vec::new();
    let mut stopping = router.stopping.subscribe();
    let mut too_long = false;
    loop {
        line.clear();
        let read = tokio::select! {
            read = read_message(&mut lines, &mut line) => read,
            () = outbox.broken.notified() => break,
            _ = stopping.wait_for(|stop| *stop) => break,
        };
        match read {
            Ok(Read::End) => break,
            Ok(Read::Line) => {}
            Ok(Read::TooLong) => {
                tracing::warn!("koda serve: a client sent a message over the limit; closing it");
                outbox.push(too_long_line());
                too_long = true;
                break;
            }
            Err(e) => {
                tracing::debug!("koda serve: reading a client failed: {e}");
                break;
            }
        }
        let text = line.strip_suffix(b"\n").unwrap_or(&line);
        let text = text.strip_suffix(b"\r").unwrap_or(text);
        if text.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let message = match parse(text) {
            Ok(message) => message,
            Err(error) => {
                outbox.push(error);
                continue;
            }
        };
        match message {
            Message::Request(request) => {
                let method = request.method.clone();
                let request_id = request.id.clone();
                let refuse = |message: &str| {
                    error_line(
                        Some(request_id.clone()),
                        ErrorObject::new(codes::INVALID_REQUEST, message),
                    )
                    .unwrap_or_default()
                };
                if method == methods::Initialize::NAME {
                    if ready {
                        outbox.push(refuse("Already initialized"));
                        continue;
                    }
                    ready = true;
                    outbox.take_capabilities(request.params.as_ref());
                    let answer =
                        serde_json::to_string(&rpc::Response::new(request_id, initialized.clone()))
                            .unwrap_or_default();
                    outbox.push(answer);
                    let mut state = router.lock();
                    for warning in &state.warnings {
                        outbox.push(warning.clone());
                    }
                    state.connections.insert(id, Arc::clone(&outbox));
                    continue;
                }
                if !ready {
                    outbox.push(refuse("Not initialized"));
                    continue;
                }
                if let Some(error) = outbox.refuses(&method) {
                    outbox.push(error_line(Some(request_id), error).unwrap_or_default());
                    continue;
                }
                if method == methods::ServeStop::NAME {
                    let if_idle = request
                        .params
                        .as_ref()
                        .and_then(|p| p["ifIdle"].as_bool())
                        .unwrap_or(false);
                    let busy = if if_idle {
                        router.busy_reason(Some(id))
                    } else {
                        None
                    };
                    let result = koda_protocol::serve::ServeStopResponse {
                        stopped: busy.is_none(),
                        reason: busy.clone(),
                    };
                    outbox.push(
                        serde_json::to_string(&rpc::Response::new(
                            request_id,
                            serde_json::to_value(result).unwrap_or_default(),
                        ))
                        .unwrap_or_default(),
                    );
                    if busy.is_none() {
                        router.stop_requested.notify_one();
                    }
                    continue;
                }
                if method == methods::SessionUnsubscribe::NAME {
                    let session = request
                        .params
                        .as_ref()
                        .and_then(|p| p["sessionId"].as_str())
                        .unwrap_or_default()
                        .to_string();
                    let status = if router.unsubscribe(id, &session) {
                        SessionUnsubscribeStatus::Unsubscribed
                    } else {
                        SessionUnsubscribeStatus::NotLoaded
                    };
                    let result = serde_json::json!({ "status": status });
                    outbox.push(
                        serde_json::to_string(&rpc::Response::new(request_id, result))
                            .unwrap_or_default(),
                    );
                    continue;
                }
                // Resuming a session the host already has open attaches to
                // it, with the requests still waiting in it sent after the
                // answer, as Codex replays them to a client that resumes.
                let resuming = (method == methods::SessionStart::NAME)
                    .then(|| {
                        request.params.as_ref()?["sessionId"]
                            .as_str()
                            .map(str::to_string)
                    })
                    .flatten();
                if let Some(session) = resuming
                    && let Some(Attached { waiting, live }) = router.attach(id, &session)
                {
                    let outbox = Arc::clone(&outbox);
                    let router = Arc::clone(&router);
                    answering.spawn(async move {
                        let read = router
                            .handle
                            .request::<methods::SessionRead>(
                                koda_protocol::session::SessionReadParams {
                                    session_id: session,
                                    include_turns: true,
                                },
                            )
                            .await;
                        let line = match read {
                            Ok(read) => serde_json::to_string(&rpc::Response::new(
                                request_id,
                                serde_json::json!({ "session": with_live(
                                    serde_json::to_value(read.session).unwrap_or_default(),
                                    live.as_ref(),
                                ) }),
                            )),
                            Err(e) => error_line(
                                Some(request_id),
                                ErrorObject::new(codes::INTERNAL_ERROR, e.to_string()),
                            ),
                        };
                        if let Ok(line) = line {
                            outbox.push(line);
                        }
                        for request in waiting {
                            outbox.push(request);
                        }
                    });
                    continue;
                }
                let (reply, answer) = oneshot::channel();
                if router
                    .handle
                    .commands
                    .send(HostCommand::Request { request, reply })
                    .await
                    .is_err()
                {
                    break;
                }
                let outbox = Arc::clone(&outbox);
                let router = Arc::clone(&router);
                answering.spawn(async move {
                    let line = match answer.await {
                        Ok(Ok(result)) => {
                            // A connection subscribes to the sessions it
                            // opens, before it hears the answer.
                            if (method == methods::SessionStart::NAME
                                || method == methods::SessionFork::NAME)
                                && let Some(session) = result["session"]["id"].as_str()
                            {
                                router.subscribe(id, session);
                            }
                            serde_json::to_string(&rpc::Response::new(request_id, result))
                        }
                        Ok(Err(error)) => error_line(Some(request_id), error),
                        Err(_) => error_line(
                            Some(request_id),
                            ErrorObject::new(codes::INTERNAL_ERROR, "Koda's host has stopped"),
                        ),
                    };
                    if let Ok(line) = line {
                        outbox.push(line);
                    }
                });
            }
            Message::Response(response) => {
                let _ = router
                    .handle
                    .commands
                    .send(HostCommand::Response {
                        id: response.id,
                        result: Ok(response.result),
                    })
                    .await;
            }
            Message::Error(ErrorResponse {
                id: Some(id),
                error,
                ..
            }) => {
                let _ = router
                    .handle
                    .commands
                    .send(HostCommand::Response {
                        id,
                        result: Err(error),
                    })
                    .await;
            }
            Message::Error(ErrorResponse { id: None, .. }) | Message::Notification(_) => {}
        }
        while answering.try_join_next().is_some() {}
    }
    router.disconnect(id);
    if outbox.overflowed() {
        // Its socket may be full and never read again: don't wait on it.
        tracing::warn!(
            "koda serve: a client stopped reading ({SLOW_CLIENT_LIMIT} messages waiting); \
             closing it"
        );
        // Its requests still finish: a session one of them opens has to
        // reach the router, or the host would hold it with nobody able to
        // open it again.
        answering.detach_all();
        writer.abort();
        return;
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        while answering.join_next().await.is_some() {}
    })
    .await;
    // As above, for a request that takes longer than the wait.
    answering.detach_all();
    outbox.close();
    let _ = writer.await;
    if too_long {
        // Read what the client is still sending before closing: on Linux,
        // closing a Unix socket with unread input resets the connection, and
        // the client would lose the error.
        let _ = tokio::time::timeout(
            Duration::from_secs(1),
            tokio::io::copy(&mut lines, &mut tokio::io::sink()),
        )
        .await;
    }
}

/// Connect to the shared server at `socket` as a client: the same parts
/// [`crate::in_process::launch`] gives for a host in this process, so a
/// client works either way. Requests go out as JSON-RPC lines, each answered
/// on its own; the server's notifications and requests arrive on `events`.
/// Stopping it ([`RequestHandle::shutdown`]) only hangs up: the sessions
/// stay on the server, as a session outlives its client.
pub async fn connect(socket: &Path) -> Result<crate::in_process::Launched, RequestError> {
    let stream = UnixStream::connect(socket)
        .await
        .map_err(|e| RequestError::Connect(format!("{}: {e}", socket.display())))?;
    let (input, mut output) = stream.into_split();
    let (commands, mut incoming) = tokio::sync::mpsc::channel::<HostCommand>(64);
    let (events, received) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let mut waiting: HashMap<RequestId, oneshot::Sender<crate::host::Answer>> = HashMap::new();
        let mut lines = BufReader::new(input);
        let mut line = Vec::new();
        loop {
            line.clear();
            tokio::select! {
                command = incoming.recv() => {
                    let out = match command {
                        Some(HostCommand::Request { request, reply }) => {
                            waiting.insert(request.id.clone(), reply);
                            serde_json::to_string(&request)
                        }
                        Some(HostCommand::Response { id, result: Ok(result) }) => {
                            serde_json::to_string(&rpc::Response::new(id, result))
                        }
                        Some(HostCommand::Response { id, result: Err(error) }) => {
                            error_line(Some(id), error)
                        }
                        Some(HostCommand::Shutdown { done }) => {
                            let _ = output.shutdown().await;
                            let _ = done.send(());
                            break;
                        }
                        None => break,
                    };
                    let Ok(out) = out else { continue };
                    if output.write_all(format!("{out}\n").as_bytes()).await.is_err() {
                        break;
                    }
                }
                read = read_message(&mut lines, &mut line) => {
                    match read {
                        Ok(Read::End | Read::TooLong) | Err(_) => break,
                        Ok(Read::Line) => {}
                    }
                    let text = line.strip_suffix(b"\n").unwrap_or(&line);
                    let Ok(message) = parse(text) else {
                        tracing::warn!("koda serve sent a line that isn't JSON-RPC");
                        continue;
                    };
                    match message {
                        Message::Response(response) => {
                            if let Some(reply) = waiting.remove(&response.id) {
                                let _ = reply.send(Ok(response.result));
                            }
                        }
                        Message::Error(ErrorResponse { id: Some(id), error, .. }) => {
                            if let Some(reply) = waiting.remove(&id) {
                                let _ = reply.send(Err(error));
                            }
                        }
                        Message::Error(ErrorResponse { id: None, error, .. }) => {
                            tracing::warn!("koda serve refused a line: {error}");
                        }
                        Message::Request(request) => {
                            match koda_protocol::ServerRequest::try_from(request) {
                                Ok(request) => {
                                    let _ = events.send(crate::events::Outgoing::Request(request));
                                }
                                Err(e) => tracing::warn!("koda serve sent a request Koda doesn't know: {e}"),
                            }
                        }
                        Message::Notification(notification) => {
                            match koda_protocol::ServerNotification::try_from(notification) {
                                Ok(notification) => {
                                    let _ = events
                                        .send(crate::events::Outgoing::Notification(notification));
                                }
                                Err(e) => tracing::debug!("koda serve sent a notification Koda doesn't know: {e}"),
                            }
                        }
                    }
                }
            }
        }
        // Requests still waiting see the connection closed.
        drop(waiting);
    });
    Ok(crate::in_process::Launched {
        requests: RequestHandle {
            commands,
            next_id: Arc::new(std::sync::atomic::AtomicI64::new(1)),
        },
        host: task,
        events: received,
    })
}
