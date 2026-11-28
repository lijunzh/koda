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
/// How long the server waits after failing to accept a connection.
const ACCEPT_RETRY: Duration = Duration::from_secs(1);
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
    // Every session's MCP servers, files and clients count against one
    // process; macOS's default of 256 is reached with 15 to 25 sessions
    // running MCP servers. Codex's app-server daemon raises it the same way.
    if let Err(e) = raise_open_file_limit() {
        tracing::warn!("koda serve: couldn't raise the open-file limit: {e}");
    }
    let options = Options {
        idle_exit,
        ..Options::default()
    };
    serve_on(args, None, socket, options, stop).await
}

/// The soft open-file limit `koda serve` asks for, as Codex's app-server
/// daemon does (`app-server/src/managed_daemon.rs`).
const OPEN_FILE_LIMIT: libc::rlim_t = 4096;

/// The soft limit to set: [`OPEN_FILE_LIMIT`], never above the hard limit and
/// never below what the process already has.
fn open_file_target(soft: libc::rlim_t, hard: libc::rlim_t) -> libc::rlim_t {
    soft.max(hard.min(OPEN_FILE_LIMIT))
}

/// Raise this process's soft open-file limit to [`open_file_target`].
fn raise_open_file_limit() -> std::io::Result<()> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit fills in the rlimit it is given.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let target = open_file_target(limit.rlim_cur, limit.rlim_max);
    if target == limit.rlim_cur {
        return Ok(());
    }
    limit.rlim_cur = target;
    // SAFETY: the soft limit asked for is at most the hard limit.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raw const limit) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
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
                // A session's answer to a join, in its place among the
                // session's messages.
                if let crate::events::Outgoing::Joined {
                    token,
                    session,
                    replay,
                } = message
                {
                    let replay = replay
                        .iter()
                        .filter_map(|message| message.to_json().ok())
                        .collect();
                    router.joined(token, *session, replay);
                    continue;
                }
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
                Err(e) => {
                    // Too many open files, say: wait before trying again,
                    // as Codex does, rather than spin.
                    tracing::warn!("koda serve: accepting a connection failed: {e}");
                    tokio::time::sleep(ACCEPT_RETRY).await;
                }
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

/// A client waiting to join an open session: who, and the request to
/// answer.
struct Joining {
    connection: u64,
    request: RequestId,
}

/// The stored session `stored` (with its turns) as it runs now: what
/// `live` says of its model, provider, window, mode and tools.
fn with_live(mut stored: Value, live: Option<&Value>) -> Value {
    if let Some(live) = live {
        for key in [
            "model",
            "modelProvider",
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
    /// Clients waiting for an open session's answer to join it, by token.
    joining: HashMap<u64, Joining>,
    /// The wait on each request nobody has answered, by its id as JSON.
    timers: HashMap<String, tokio::task::AbortHandle>,
    /// The last join token given.
    next_join: u64,
    /// The newer version this server makes way for once its running turns
    /// end (`serve/stop` with `forUpdate` and `afterTurns`): new turns are
    /// refused until then.
    update: Option<String>,
    /// Sessions deleted, or archived and not brought back: closed in the
    /// host, with no client to come for what their last moments still send.
    closed: HashSet<String>,
}

/// What changes the list of sessions, which goes to every client, as Codex
/// sends a thread's start, rename, delete, archive and status to every
/// connection: a client that lists sessions hears of ones another client
/// makes, and of what each is doing.
const TOLD_TO_EVERYONE: &[&str] = &[
    "session/started",
    "session/name/updated",
    "session/deleted",
    "session/archived",
    "session/unarchived",
    "session/status/changed",
];

impl RouterState {
    /// Session `session` is no longer open in the host: its subscribers,
    /// its waiting requests and what it ran with go.
    fn forget(&mut self, session: &str) {
        self.subscribers.remove(session);
        self.activity.remove(session);
        self.open.remove(session);
        self.running.remove(session);
        self.held.remove(session);
        for (id, _) in self.pending.remove(session).unwrap_or_default() {
            if let Some(timer) = self.timers.remove(&id.to_string()) {
                timer.abort();
            }
        }
        self.live.remove(session);
    }
}

/// Requests that start a turn, which a server making way for a newer Koda
/// refuses, as Codex's draining app-server refuses new turns. A goal set
/// on an idle session starts one, and so does compaction.
const TURN_ADMISSIONS: &[&str] = &["turn/start", "session/compact/start", "session/goal/set"];

/// Sessions with a turn running or a request waiting: the work that stopping
/// the server would cut short.
fn working(state: &RouterState) -> u32 {
    let mut working: HashSet<&String> = state.running.iter().collect();
    working.extend(
        state
            .pending
            .iter()
            .filter(|(_, pending)| !pending.is_empty())
            .map(|(session, _)| session),
    );
    u32::try_from(working.len()).unwrap_or(u32::MAX)
}

/// The error a draining server answers a new turn with, Codex's.
fn draining_error() -> ErrorObject {
    ErrorObject {
        code: codes::INVALID_REQUEST,
        message: koda_protocol::serve::DRAINING_MESSAGE.to_string(),
        data: Some(serde_json::json!({ "reason": koda_protocol::serve::DRAINING_REASON })),
    }
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
        // A session deleted or archived was closed first, with everything
        // it had to say sent before. What still comes of it (a turn that
        // took too long to stop, a background agent's request withdrawn as
        // it goes) has nobody to go to, and isn't kept for a client to come.
        if state.closed.contains(&session) && !TOLD_TO_EVERYONE.contains(&method) {
            return;
        }
        // A request waits until a client answers it, Koda resolves it, or
        // the wait runs out; a client that attaches is sent it again.
        if value.get("id").is_some() && !method.is_empty() {
            state
                .pending
                .entry(session.clone())
                .or_default()
                .push((value["id"].clone(), line.clone()));
            self.answer_no_after_wait(&mut state, &session, value["id"].clone());
        }
        if method == "serverRequest/resolved" {
            let resolved = &value["params"]["requestId"];
            if let Some(pending) = state.pending.get_mut(&session) {
                pending.retain(|(id, _)| id != resolved);
            }
            if let Some(timer) = state.timers.remove(&resolved.to_string()) {
                timer.abort();
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
                    let settings = &value["params"]["sessionSettings"];
                    live["model"] = settings["model"].clone();
                    live["modelProvider"] = settings["modelProvider"].clone();
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
        // Making way for a newer Koda: the last turn has ended, or the last
        // request been answered. This message still goes out first.
        if state.update.is_some() && working(&state) == 0 {
            self.stop_requested.notify_one();
        }
        if TOLD_TO_EVERYONE.contains(&method) {
            match method {
                // Closed in the host: nobody is subscribed to it any more,
                // as Codex drops a thread's subscriptions, and an archived
                // one brought back is opened again by `session/resume`,
                // not joined.
                "session/deleted" | "session/archived" => {
                    state.forget(&session);
                    state.closed.insert(session.clone());
                }
                "session/unarchived" => {
                    state.closed.remove(&session);
                }
                _ => {}
            }
            for outbox in state.connections.values() {
                outbox.push(line.clone());
            }
            return;
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

    /// Start joining `connection` to `session` if the host has it open, as
    /// Codex subscribes a client that resumes a loaded thread: the token
    /// the session's answer will carry ([`Self::joined`]).
    fn begin_join(&self, connection: u64, session: &str, request: RequestId) -> Option<u64> {
        let mut state = self.lock();
        if !state.open.contains(session) {
            return None;
        }
        // Someone wants it: a pending unload is stale.
        *state.activity.entry(session.to_string()).or_default() += 1;
        state.next_join += 1;
        let token = state.next_join;
        state.joining.insert(
            token,
            Joining {
                connection,
                request,
            },
        );
        Some(token)
    }

    /// A join that the session couldn't answer.
    fn end_join(self: &Arc<Self>, token: u64, session: &str) {
        self.lock().joining.remove(&token);
        self.unload_if_idle(session);
    }

    /// The session's answer to the join `token`, reached in the order of
    /// its messages: subscribe the client here and answer it with `session`
    /// as it stands, then what it missed that isn't in the session (its
    /// token counts, plan and turn's diff, in `replay`), then the requests
    /// still waiting in it. What the
    /// session sent before this is in `session`; what it sends after, the
    /// client hears. Codex composes the answer and subscribes in one step
    /// the same way.
    fn joined(
        self: &Arc<Self>,
        token: u64,
        session: koda_protocol::session::Session,
        replay: Vec<String>,
    ) {
        let session_id = session.id.clone();
        {
            let mut state = self.lock();
            let Some(joining) = state.joining.remove(&token) else {
                return;
            };
            if let Some(outbox) = state.connections.get(&joining.connection).cloned() {
                state
                    .subscribers
                    .entry(session_id.clone())
                    .or_default()
                    .insert(joining.connection);
                let described = with_live(
                    serde_json::to_value(&session).unwrap_or_default(),
                    state.live.get(&session_id),
                );
                if let Ok(line) = serde_json::to_string(&rpc::Response::new(
                    joining.request,
                    serde_json::json!({ "session": described }),
                )) {
                    outbox.push(line);
                }
                for line in replay {
                    outbox.push(line);
                }
                for (_, request) in state.pending.get(&session_id).into_iter().flatten() {
                    outbox.push(request.clone());
                }
                return;
            }
        }
        // The client left while it waited.
        self.unload_if_idle(&session_id);
    }

    /// Answer request `id` of `session` no once [`Router::approval_wait`]
    /// has passed, if nobody has: the approval is refused, the question gets
    /// an empty answer, the loop stops.
    fn answer_no_after_wait(self: &Arc<Self>, state: &mut RouterState, session: &str, id: Value) {
        let router = Arc::clone(self);
        let session = session.to_string();
        let key = id.to_string();
        let timer = tokio::spawn(async move {
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
        // Stopped when the request is answered, so a day's wait doesn't
        // outlive it ([`Router::deliver`]). The caller holds the lock.
        state.timers.insert(key, timer.abort_handle());
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
                state.forget(&session);
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

    /// `serve/stop` with `forUpdate`: make way for Koda `version`. Clients
    /// don't hold the server back, since none can go on with a server of
    /// another version; only running turns and waiting requests do. With
    /// those, `if_idle` leaves everything as it is and says how many there
    /// are, `after_turns` waits for them refusing new turns, and neither
    /// stops them. Returns the answer and whether to stop now.
    fn make_way(
        &self,
        version: &str,
        if_idle: bool,
        after_turns: bool,
    ) -> (koda_protocol::serve::ServeStopResponse, bool) {
        use koda_protocol::serve::ServeStopResponse;
        let mut state = self.lock();
        let working = working(&state);
        let waiting = |working: u32| {
            Some(format!(
                "{working} session(s) have a turn running or a request waiting"
            ))
        };
        // Already waiting for them, at another client's word.
        if state.update.is_some() && working > 0 {
            let answer = ServeStopResponse {
                stopped: false,
                reason: waiting(working),
                working,
                draining: true,
            };
            return (answer, false);
        }
        if working > 0 && if_idle {
            let answer = ServeStopResponse {
                stopped: false,
                reason: waiting(working),
                working,
                draining: false,
            };
            return (answer, false);
        }
        let draining = working > 0 && after_turns;
        self.announce(&state, version, if draining { working } else { 0 });
        if draining {
            tracing::info!(
                "koda serve: making way for Koda {version} once {working} session(s) finish"
            );
            state.update = Some(version.to_string());
            let answer = ServeStopResponse {
                stopped: false,
                reason: waiting(working),
                working,
                draining: true,
            };
            return (answer, false);
        }
        tracing::info!("koda serve: making way for Koda {version}");
        let answer = ServeStopResponse {
            stopped: true,
            reason: None,
            working,
            draining: false,
        };
        (answer, true)
    }

    /// Tell every client the server is making way for `version`.
    fn announce(&self, state: &RouterState, version: &str, working: u32) {
        let notice = crate::events::Outgoing::Notification(
            koda_protocol::ServerNotification::ServeUpdating(
                koda_protocol::serve::ServeUpdatingNotification {
                    version: version.to_string(),
                    working,
                },
            ),
        );
        let Ok(line) = notice.to_json() else {
            return;
        };
        for outbox in state.connections.values() {
            outbox.push(line.clone());
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
                    // Each field read on its own, so a field a later Koda
                    // adds can't turn the request into a plain stop.
                    let params = request.params.as_ref();
                    let flag = |name: &str| params.and_then(|p| p[name].as_bool()).unwrap_or(false);
                    let if_idle = flag("ifIdle");
                    let for_update = params
                        .and_then(|p| p["forUpdate"].as_str())
                        .map(str::to_string);
                    let (result, stop) = match for_update {
                        Some(version) => router.make_way(&version, if_idle, flag("afterTurns")),
                        None => {
                            let busy = if if_idle {
                                router.busy_reason(Some(id))
                            } else {
                                None
                            };
                            let stop = busy.is_none();
                            let result = koda_protocol::serve::ServeStopResponse {
                                stopped: stop,
                                reason: busy,
                                ..Default::default()
                            };
                            (result, stop)
                        }
                    };
                    outbox.push(
                        serde_json::to_string(&rpc::Response::new(
                            request_id,
                            serde_json::to_value(result).unwrap_or_default(),
                        ))
                        .unwrap_or_default(),
                    );
                    if stop {
                        router.stop_requested.notify_one();
                    }
                    continue;
                }
                // Making way for a newer Koda: no new turns.
                if TURN_ADMISSIONS.contains(&method.as_str()) && router.lock().update.is_some() {
                    outbox.push(error_line(Some(request_id), draining_error()).unwrap_or_default());
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
                let resuming = (method == methods::SessionResume::NAME)
                    .then(|| {
                        request.params.as_ref()?["sessionId"]
                            .as_str()
                            .map(str::to_string)
                    })
                    .flatten();
                if let Some(session) = resuming
                    && let Some(token) = router.begin_join(id, &session, request_id.clone())
                {
                    // The session answers on its own message queue, at the
                    // point this client joins it ([`Router::joined`]).
                    let (reply, answer) = oneshot::channel();
                    if router
                        .handle
                        .commands
                        .send(HostCommand::Join {
                            session_id: session.clone(),
                            token,
                            reply,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                    let outbox = Arc::clone(&outbox);
                    let router = Arc::clone(&router);
                    answering.spawn(async move {
                        let refused = match answer.await {
                            Ok(Ok(())) => return,
                            Ok(Err(error)) => error,
                            Err(_) => ErrorObject::new(
                                codes::INTERNAL_ERROR,
                                format!("session {session} closed before it answered"),
                            ),
                        };
                        router.end_join(token, &session);
                        if let Ok(line) = error_line(Some(request_id), refused) {
                            outbox.push(line);
                        }
                    });
                    continue;
                }
                let (reply, answer) = oneshot::channel();
                if router
                    .handle
                    .commands
                    .send(HostCommand::Request {
                        request,
                        reply,
                        client: outbox.client(),
                    })
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
                                || method == methods::SessionResume::NAME
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
        // A line from the server as it arrives. Kept while a request is
        // sent: `read_message` leaves what it has read of a line here when
        // the other branch goes first, and the next pass reads the rest.
        let mut line = Vec::new();
        loop {
            tokio::select! {
                command = incoming.recv() => {
                    let out = match command {
                        Some(HostCommand::Request { request, reply, .. }) => {
                            waiting.insert(request.id.clone(), reply);
                            serde_json::to_string(&request)
                        }
                        Some(HostCommand::Response { id, result: Ok(result) }) => {
                            serde_json::to_string(&rpc::Response::new(id, result))
                        }
                        Some(HostCommand::Response { id, result: Err(error) }) => {
                            error_line(Some(id), error)
                        }
                        #[cfg(test)]
                        Some(HostCommand::PanicSession { .. }) => continue,
                        Some(HostCommand::Shutdown { done }) => {
                            let _ = output.shutdown().await;
                            let _ = done.send(());
                            break;
                        }
                        // Only the server's own router joins a session.
                        Some(HostCommand::Join { reply, .. }) => {
                            let _ = reply.send(Err(ErrorObject::new(
                                codes::INVALID_REQUEST,
                                "a client joins a session with session/resume",
                            )));
                            continue;
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
                    let line = std::mem::take(&mut line);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_open_file_target_is_codex_s() {
        // macOS's default soft limit, with room under the hard limit.
        assert_eq!(open_file_target(256, libc::RLIM_INFINITY), 4096);
        // Never above the hard limit.
        assert_eq!(open_file_target(256, 1024), 1024);
        // Never lower than what the process already has.
        assert_eq!(open_file_target(65_536, libc::RLIM_INFINITY), 65_536);
    }

    /// A client that sends while a line from the server is still arriving
    /// keeps what has arrived of it: the line is whole when the rest comes.
    /// Clearing it on every pass of the loop dropped such a line, so a large
    /// answer or notification could be lost while the TUI sent a request.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_line_arriving_while_the_client_sends_is_kept() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        const ROUNDS: usize = 20;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (halfway_tx, mut halfway) = tokio::sync::mpsc::unbounded_channel::<()>();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut read = BufReader::new(read);
            for round in 0..ROUNDS {
                let line = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "warning",
                    "params": {"message": format!("round {round} {}", "x".repeat(4096))},
                })
                .to_string();
                let (first, rest) = line.split_at(line.len() / 2);
                write.write_all(first.as_bytes()).await.unwrap();
                write.flush().await.unwrap();
                halfway_tx.send(()).unwrap();
                // The client's request, sent while half the line is in.
                let mut request = String::new();
                read.read_line(&mut request).await.unwrap();
                write
                    .write_all(format!("{rest}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });

        let mut launched = connect(&path).await.unwrap();
        for round in 0..ROUNDS {
            halfway.recv().await.unwrap();
            let (reply, _answer) = oneshot::channel();
            launched
                .requests
                .commands
                .send(HostCommand::Request {
                    request: rpc::Request::new(
                        RequestId::Integer(round as i64),
                        "state/read",
                        None,
                    ),
                    reply,
                    client: None,
                })
                .await
                .unwrap();
            // Long enough for a busy machine.
            let heard = tokio::time::timeout(Duration::from_secs(30), launched.events.recv())
                .await
                .expect("the line arrived")
                .expect("the connection is open");
            let crate::events::Outgoing::Notification(koda_protocol::ServerNotification::Warning(
                warning,
            )) = heard
            else {
                panic!("not the warning: {heard:?}");
            };
            assert!(
                warning.message.starts_with(&format!("round {round} ")),
                "{}",
                &warning.message[..20]
            );
        }
        server.await.unwrap();
    }

    /// A session archived or deleted is closed: what its last moments
    /// still send (a turn slow to stop, a request withdrawn as a
    /// background agent goes) isn't kept for the next client, as a new
    /// session's first messages are, and a request among it starts no wait
    /// for an answer. Unarchived, it is a session a client may come for
    /// again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn what_a_closed_session_still_sends_is_not_kept() {
        let (commands, _host) = tokio::sync::mpsc::channel(1);
        let router = Arc::new(Router {
            state: Mutex::default(),
            handle: RequestHandle {
                commands,
                next_id: Arc::default(),
            },
            unload_delay: Duration::from_secs(60),
            approval_wait: Duration::from_secs(60),
            stopping: tokio::sync::watch::Sender::new(false),
            stop_requested: tokio::sync::Notify::new(),
        });
        let told = |method: &str| {
            serde_json::json!({"jsonrpc": "2.0", "method": method, "params": {"sessionId": "s"}})
                .to_string()
        };
        let asked = serde_json::json!({
            "jsonrpc": "2.0", "id": 7, "method": "item/toolCall/requestApproval",
            "params": {"sessionId": "s"},
        })
        .to_string();
        let kept = |router: &Router| router.lock().held.get("s").map_or(0, Vec::len);

        // A session with no client yet: what it says waits for one.
        router.deliver(told("turn/started"));
        assert_eq!(kept(&router), 1);

        for closing in ["session/archived", "session/deleted"] {
            router.deliver(told(closing));
            router.deliver(told("turn/completed"));
            router.deliver(asked.clone());
            let state = router.lock();
            assert!(!state.held.contains_key("s"), "{closing}");
            assert!(state.pending.is_empty(), "{closing}");
            assert!(state.timers.is_empty(), "{closing}");
            assert!(!state.running.contains("s"), "{closing}");
            drop(state);
            if closing == "session/archived" {
                router.deliver(told("session/unarchived"));
                router.deliver(told("turn/started"));
                assert_eq!(kept(&router), 1, "unarchived, it is kept again");
            }
        }
    }

    #[test]
    fn the_open_file_limit_is_raised() {
        raise_open_file_limit().unwrap();
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: getrlimit fills in the rlimit it is given.
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) },
            0
        );
        assert!(
            limit.rlim_cur >= limit.rlim_max.min(OPEN_FILE_LIMIT),
            "{}",
            limit.rlim_cur
        );
    }
}
