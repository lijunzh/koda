//! The host: the task that owns Koda's engine state and answers requests.
//!
//! It takes requests in their wire form and answers with a JSON value or a
//! JSON-RPC error, whatever the transport, so the in-process client and a
//! `koda serve --stdio` ([`crate::stdio`]) share one contract. Codex's in-process app-server
//! does the same (`app-server/src/in_process.rs`).
//!
//! Each request is answered on its own task, so a slow one never holds up
//! the rest; shutting down cancels whatever is still running. Each open
//! session has a task of its own ([`crate::session`]), which runs its turns
//! and answers the requests about them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicI64;
use std::sync::{Arc, OnceLock, PoisonError};
use std::time::Duration;

use koda_core::config::KodaConfig;
use koda_protocol::ClientRequest;
use koda_protocol::ServerNotification;
use koda_protocol::initialize::{ClientInfo, InitializeResponse, ServerInfo};
use koda_protocol::rpc::{self, ErrorObject, RequestId, codes};
use koda_protocol::session::{
    SessionArchiveResponse, SessionArchivedNotification, SessionConfidentialSetParams,
    SessionConfidentialSetResponse, SessionConfidentialUpdatedNotification, SessionDeleteResponse,
    SessionDeletedNotification, SessionListCwd, SessionListParams, SessionListResponse,
    SessionSearchParams, SessionSearchResponse, SessionSearchResult, SessionSortKey,
    SessionSourceKind, SessionStartParams, SessionStartResponse, SessionUnarchiveResponse,
    SessionUnarchivedNotification, SessionUnsubscribeResponse, SessionUnsubscribeStatus,
    SortDirection,
};
use koda_protocol::state::{SandboxState, StateReadResponse};
use koda_state::db::Database;
use serde_json::Value;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

use crate::events::Outgoing;
use crate::session::{
    self, GoalRequest, OpenRequests, SessionCommand, SessionContext, SessionHandles, internal,
    invalid,
};

mod session_archive;

/// Open sessions' in-process handles, by id; shared with the client.
pub(crate) type HandleMap = Arc<std::sync::Mutex<HashMap<String, SessionHandles>>>;

/// How long a session being closed gets to stop its turn; one that hasn't
/// by then is cut off ([`OpenSession::cut_off`]). A turn bounds its own
/// wind-down at five seconds; the client stops the host by force after its
/// own five, so this is under both.
const SESSION_STOP: Duration = Duration::from_secs(4);

/// An open session: where its commands go, and what stops it by force.
#[derive(Clone)]
struct OpenSession {
    commands: mpsc::UnboundedSender<SessionCommand>,
    /// What the session sends its clients by, to say its turn's end for it
    /// if its task has to be stopped.
    sink: Arc<session::HostSink>,
    /// Its task.
    task: tokio::task::AbortHandle,
}

impl OpenSession {
    fn send(&self, command: SessionCommand) -> Result<(), mpsc::error::SendError<SessionCommand>> {
        self.commands.send(command)
    }

    /// Stop the session where it stands: it was told to stop and hasn't
    /// within [`SESSION_STOP`]. The end of the turn it was running is said
    /// first, with its waiting requests withdrawn, and then its task is
    /// stopped, as Codex aborts a task that doesn't stop in its grace and
    /// still sends `TurnAborted`. A client always sees a turn end, and
    /// before it hears the session was archived, deleted or closed.
    async fn cut_off(&self) {
        let secs = SESSION_STOP.as_secs();
        self.sink
            .cut_off(&format!(
                "the turn did not stop within {secs}s of its session closing"
            ))
            .await;
        self.task.abort();
    }
}

/// How many sessions `session/list` gives without a limit.
const DEFAULT_LIST_LIMIT: u32 = 50;

/// Where a page of `session/list` or `session/search` ended: its last
/// session's sort time, in Unix seconds, and its place in the order
/// sessions were made. Opaque to clients.
#[derive(serde::Serialize, serde::Deserialize)]
struct ListCursor {
    at: i64,
    seq: i64,
}

impl ListCursor {
    /// The cursor that goes on after `session`.
    fn after(session: &koda_state::session_store::SessionInfo, by_updated: bool) -> Self {
        Self {
            at: if by_updated {
                session.updated_unix
            } else {
                session.created_unix
            },
            seq: session.sequence,
        }
    }
}

/// How many sessions `session/search` gives without a limit, and at most:
/// Codex's `thread/search`'s.
const DEFAULT_SEARCH_LIMIT: u32 = 25;
const MAX_SEARCH_LIMIT: u32 = 100;

/// How many candidates a search takes from the store at a time. Each page
/// is a pass over the stored text, so they are large and few.
const SEARCH_SCAN_PAGE: i64 = 256;

/// The longest `searchTerm` `session/search` takes, in characters.
const MAX_SEARCH_TERM: usize = 1000;

/// Which sessions `session/list` and `session/search` cover, in what order
/// and from where: the parameters they share.
struct Listing<'a> {
    /// The method, for what a refusal says.
    method: &'static str,
    cwd: Option<&'a SessionListCwd>,
    source_kinds: Option<&'a [SessionSourceKind]>,
    archived: Option<bool>,
    sort_key: Option<SessionSortKey>,
    sort_direction: Option<SortDirection>,
    cursor: Option<&'a str>,
}

/// What a scan of the sessions for a term found ([`Host::scan_sessions`]).
struct Scanned {
    /// The sessions that say it, in the scan's order, each with its first
    /// match.
    found: Vec<(
        koda_state::session_store::SessionInfo,
        crate::session_search::Found,
    )>,
    /// Whether another session says it after these.
    more: bool,
}

/// The longest name `session/name/set` takes, in characters.
const MAX_SESSION_NAME: usize = 100;

/// A request's answer: the method's response as JSON, or an error.
pub(crate) type Answer = Result<Value, ErrorObject>;

/// What a client sends the host.
pub(crate) enum HostCommand {
    /// Answer `request` on `reply`. `client` is the name the connection
    /// it came over gave in `initialize`; without one, the host's own
    /// client sent it.
    Request {
        request: rpc::Request,
        reply: oneshot::Sender<Answer>,
        client: Option<Arc<str>>,
    },
    /// The client's answer to the server request `id`.
    Response { id: RequestId, result: Answer },
    /// Make session `session_id`'s task panic, for testing how that ends.
    #[cfg(test)]
    PanicSession { session_id: String },
    /// A client asked to join the open session `session_id`: have the
    /// session put itself, as it stands, on the message queue under
    /// `token`. An error on `reply` if it isn't open here.
    Join {
        session_id: String,
        token: u64,
        reply: oneshot::Sender<Result<(), ErrorObject>>,
    },
    /// Stop every session and the host; say so on `done`.
    Shutdown { done: oneshot::Sender<()> },
}

/// What a host is started with.
pub(crate) struct HostArgs {
    /// What new sessions start with.
    pub config: KodaConfig,
    /// `~/.config/koda/config.toml`.
    pub config_file: koda_config::ConfigFile,
    /// What the client is told about the config file after `initialize`.
    pub config_warnings: Vec<koda_protocol::config::ConfigWarningNotification>,
    /// The project sessions work in.
    pub project_root: PathBuf,
    /// Where sessions are stored.
    pub db: Database,
    /// This machine's sandbox, which the mode rules and `state/read` use.
    pub sandbox: SandboxState,
    /// The client's queue of notifications and server requests.
    pub events: mpsc::UnboundedSender<Outgoing>,
}

/// Start a host on its own task.
pub(crate) fn spawn(args: HostArgs) -> (mpsc::Sender<HostCommand>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(64);
    let (ended, ended_rx) = mpsc::unbounded_channel();
    let host = Host::new(args, ended);
    (tx, tokio::spawn(run(host, rx, ended_rx)))
}

struct Host {
    /// The host itself, for what it hands a session that outlives the
    /// request that opened it ([`session_archive::HostArchive`]).
    me: std::sync::Weak<Host>,
    /// What every session shares; its config is what new sessions start with.
    context: SessionContext,
    /// Set by `initialize`; until then every other request is refused.
    client: OnceLock<ClientInfo>,
    /// Sent once, right after `initialize` is answered.
    config_warnings: std::sync::Mutex<Vec<koda_protocol::config::ConfigWarningNotification>>,
    /// Open sessions, by id.
    sessions: Mutex<HashMap<String, OpenSession>>,
    /// Open sessions' in-process handles, by id.
    handles: HandleMap,
    /// Their tasks. Dropping the host stops them.
    tasks: Mutex<JoinSet<()>>,
    /// Told when a session's task ends, with the panic's message if it
    /// failed.
    ended: mpsc::UnboundedSender<(String, Option<String>)>,
    /// `fuzzyFileSearch` requests in flight, by cancellation token.
    searches: crate::fuzzy_file_search::PendingSearches,
}

async fn run(
    host: Arc<Host>,
    mut commands: mpsc::Receiver<HostCommand>,
    mut ended: mpsc::UnboundedReceiver<(String, Option<String>)>,
) {
    let mut running = JoinSet::new();
    // Each session's last ordered request, as it will be when done: the next
    // one for the session waits for it.
    let mut last_of: HashMap<String, oneshot::Receiver<()>> = HashMap::new();
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(HostCommand::Request { request, reply, client }) => {
                    let host = Arc::clone(&host);
                    // A session's requests run in the order they came, as
                    // Codex queues a thread's: a settings update then a
                    // turn, or a turn then its interrupt, can't swap. The
                    // rest run at once.
                    let after = match ordered_session(&request) {
                        Some(session) => {
                            let (done, finished) = oneshot::channel::<()>();
                            let before = last_of.insert(session, finished);
                            Some((before, done))
                        }
                        None => None,
                    };
                    running.spawn(async move {
                        let done = match after {
                            Some((before, done)) => {
                                if let Some(before) = before {
                                    let _ = before.await;
                                }
                                Some(done)
                            }
                            None => None,
                        };
                        host.handle(request, reply, client).await;
                        drop(done);
                    });
                    // Sessions whose requests have all finished need no entry.
                    last_of.retain(|_, finished| {
                        !matches!(finished.try_recv(), Err(oneshot::error::TryRecvError::Closed))
                    });
                }
                Some(HostCommand::Response { id, result }) => host.route(id, result).await,
                #[cfg(test)]
                Some(HostCommand::PanicSession { session_id }) => {
                    if let Some(session) = host.sessions.lock().await.get(&session_id) {
                        let _ = session.send(SessionCommand::Panic);
                    }
                }
                Some(HostCommand::Join { session_id, token, reply }) => {
                    let open = host.sessions.lock().await.get(&session_id).cloned();
                    match open {
                        Some(commands) => {
                            if let Err(refused) =
                                commands.send(SessionCommand::Join { token, reply })
                            {
                                refused.0.refuse(invalid(format!(
                                    "session {session_id} has closed"
                                )));
                            }
                        }
                        None => {
                            let _ = reply
                                .send(Err(invalid(format!("session {session_id} isn't open"))));
                        }
                    }
                }
                Some(HostCommand::Shutdown { done }) => {
                    running.shutdown().await;
                    host.stop_sessions().await;
                    let _ = done.send(());
                    return;
                }
                // Every client is gone.
                None => {
                    host.stop_sessions().await;
                    return;
                }
            },
            // A session's task ended: reap it, and forget one that failed,
            // so it can be opened again.
            Some((session, failed)) = ended.recv() => {
                host.reap().await;
                if let Some(message) = failed {
                    host.forget_failed(&session, &message).await;
                }
            }
            Some(finished) = running.join_next(), if !running.is_empty() => {
                if let Err(error) = finished
                    && error.is_panic()
                {
                    tracing::error!("a request handler panicked: {error}");
                }
            }
        }
    }
}

/// What a panic said, as the text it was given.
fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|text| text.to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic".to_string())
}

/// The session a request must wait its turn in, if it is one that changes
/// a session or its turns, as Codex serializes `turn/*` and a thread's
/// updates by thread. Reads run at once.
fn ordered_session(request: &rpc::Request) -> Option<String> {
    const ORDERED: &[&str] = &[
        "session/start",
        "session/resume",
        "session/fork",
        "session/delete",
        "session/archive",
        "session/unarchive",
        "session/name/set",
        "session/confidential/set",
        "session/settings/update",
        "session/compact/start",
        "session/revert",
        "session/unsubscribe",
        "session/goal/set",
        "session/goal/clear",
        "turn/start",
        "turn/steer",
        "turn/interrupt",
    ];
    if !ORDERED.contains(&request.method.as_str()) {
        return None;
    }
    request.params.as_ref()?["sessionId"]
        .as_str()
        .map(str::to_string)
}

impl Host {
    /// A host with no session open, which tells `ended` when a session's
    /// task ends.
    fn new(args: HostArgs, ended: mpsc::UnboundedSender<(String, Option<String>)>) -> Arc<Self> {
        Arc::new_cyclic(|me| Host {
            me: me.clone(),
            context: SessionContext {
                config: args.config,
                config_file: args.config_file,
                project_root: args.project_root,
                db: args.db,
                sandbox: args.sandbox,
                events: args.events,
                request_ids: Arc::new(AtomicI64::new(0)),
                open_requests: OpenRequests::default(),
                statuses: session::Statuses::default(),
                run_checked: Arc::default(),
            },
            handles: HandleMap::default(),
            client: OnceLock::new(),
            config_warnings: std::sync::Mutex::new(args.config_warnings),
            sessions: Mutex::new(HashMap::new()),
            tasks: Mutex::new(JoinSet::new()),
            ended,
            searches: crate::fuzzy_file_search::PendingSearches::default(),
        })
    }

    async fn handle(
        &self,
        request: rpc::Request,
        reply: oneshot::Sender<Answer>,
        client: Option<Arc<str>>,
    ) {
        let request = match ClientRequest::try_from(request) {
            Ok(request) => request,
            Err(e) => {
                let _ = reply.send(Err(e.to_error_object()));
                return;
            }
        };
        let (session_id, command) = match request {
            ClientRequest::Initialize { params, .. } => {
                let answer = self.initialize(params.client_info);
                let initialized = answer.is_ok();
                let _ = reply.send(answer);
                // After the answer, as Codex sends its config warnings.
                if initialized {
                    let warnings = std::mem::take(
                        &mut *self
                            .config_warnings
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner),
                    );
                    for warning in warnings {
                        let _ = self.context.events.send(Outgoing::Notification(
                            koda_protocol::ServerNotification::ConfigWarning(warning),
                        ));
                    }
                    // A Claude model Anthropic has released that this Koda
                    // doesn't support yet, said once, when the Models API
                    // answers (it needs a key; without one, nothing).
                    let events = self.context.events.clone();
                    tokio::spawn(async move {
                        if let Some(ids) = koda_core::model_alias::ensure_live_claude_ids().await {
                            for message in koda_core::model_alias::new_model_notices(ids) {
                                let _ = events.send(Outgoing::Notification(
                                    koda_protocol::ServerNotification::Warning(
                                        koda_protocol::server::WarningNotification {
                                            session_id: None,
                                            message,
                                        },
                                    ),
                                ));
                            }
                        }
                    });
                }
                return;
            }
            _ if self.client.get().is_none() => {
                let _ = reply.send(Err(invalid("Not initialized")));
                return;
            }
            ClientRequest::StateRead { .. } => {
                let _ = reply.send(self.state().await.and_then(to_json));
                return;
            }
            ClientRequest::SessionStart { params, .. } => {
                let origin = self.origin(client.as_deref(), params.ephemeral.unwrap_or(false));
                let _ = reply.send(
                    self.start_session(None, params, origin)
                        .await
                        .and_then(to_json),
                );
                return;
            }
            // Codex's `thread/resume`: the stored session, with its turns.
            // One the server has open is joined before it gets here.
            ClientRequest::SessionResume { params, .. } => {
                let origin = self.origin(client.as_deref(), false);
                let koda_protocol::session::SessionResumeParams {
                    session_id,
                    approval_policy,
                    model_provider,
                    base_url,
                    model,
                    cwd,
                    config,
                } = params;
                if let Err(archived) = self.refuse_archived(&session_id).await {
                    let _ = reply.send(Err(archived));
                    return;
                }
                // Without a folder, the session's own, as Codex's
                // `thread/resume` falls back to the thread's stored cwd: a
                // client that resumes by id (the desktop app) gets the
                // session where it was, not the server's folder.
                let cwd = match cwd {
                    Some(cwd) => Some(cwd),
                    None => match self.stored_folder(&session_id).await {
                        Ok(folder) => folder,
                        Err(e) => {
                            let _ = reply.send(Err(e));
                            return;
                        }
                    },
                };
                let start = SessionStartParams {
                    approval_policy,
                    model_provider,
                    base_url,
                    model,
                    cwd,
                    config,
                    ephemeral: None,
                };
                let resumed = self
                    .start_session(Some(session_id), start, origin)
                    .await
                    .map(|started| koda_protocol::session::SessionResumeResponse {
                        session: started.session,
                    });
                let _ = reply.send(resumed.and_then(to_json));
                return;
            }
            ClientRequest::ModelList { params, .. } => {
                let answer = crate::settings::list_models(&self.context.config, params).await;
                let _ = reply.send(answer.and_then(to_json));
                return;
            }
            ClientRequest::ConfigRead { params, .. } => {
                let read = crate::config_file::read(&self.context.config_file, &params);
                let _ = reply.send(read.and_then(to_json));
                return;
            }
            ClientRequest::ConfigBatchWrite { params, .. } => {
                let written = crate::config_file::write(&self.context.config_file, params);
                let _ = reply.send(written.and_then(to_json));
                return;
            }
            ClientRequest::ModelProviderApiKeySet { params, .. } => {
                let set = crate::settings::set_api_key(
                    &self.context.db,
                    &params.provider_id,
                    &params.api_key,
                )
                .await;
                let _ =
                    reply.send(set.and_then(|()| {
                        to_json(koda_protocol::config::ModelProviderApiKeyResponse {})
                    }));
                return;
            }
            ClientRequest::ModelProviderApiKeyRemove { params, .. } => {
                let removed =
                    crate::settings::remove_api_key(&self.context.db, &params.provider_id).await;
                let _ =
                    reply.send(removed.and_then(|()| {
                        to_json(koda_protocol::config::ModelProviderApiKeyResponse {})
                    }));
                return;
            }
            ClientRequest::WebSearchApiKeyRead { .. } => {
                let _ = reply.send(to_json(crate::settings::web_search_key()));
                return;
            }
            ClientRequest::WebSearchApiKeySet { params, .. } => {
                let set =
                    crate::settings::set_web_search_key(&self.context.db, &params.api_key).await;
                let _ = reply.send(
                    set.and_then(|()| to_json(koda_protocol::config::WebSearchApiKeyResponse {})),
                );
                return;
            }
            ClientRequest::WebSearchApiKeyRemove { .. } => {
                let removed = crate::settings::remove_web_search_key(&self.context.db).await;
                let _ = reply.send(
                    removed
                        .and_then(|()| to_json(koda_protocol::config::WebSearchApiKeyResponse {})),
                );
                return;
            }
            ClientRequest::McpServerStatusList { params, .. } => {
                let _ = reply.send(self.mcp_status(params).await.and_then(to_json));
                return;
            }
            ClientRequest::McpServerRefresh { .. } => {
                // Each open session takes the stored servers now when idle,
                // or once its turn has ended. Answered once the idle ones
                // have, so a list that follows shows the change.
                let mut taken = Vec::new();
                for session in self.sessions.lock().await.values() {
                    let (done, rx) = oneshot::channel();
                    if session.send(SessionCommand::ReloadMcp { done }).is_ok() {
                        taken.push(rx);
                    }
                }
                for rx in taken {
                    let _ = tokio::time::timeout(SESSION_STOP, rx).await;
                }
                let _ = reply.send(to_json(koda_protocol::mcp::McpServerRefreshResponse {}));
                return;
            }
            ClientRequest::McpServerOauthLogin { params, .. } => {
                let login =
                    crate::mcp::oauth_login(&self.context.config_file, &params.name, params.scopes)
                        .await;
                let login = match login {
                    Ok(login) => login,
                    Err(e) => {
                        let _ = reply.send(Err(e));
                        return;
                    }
                };
                let _ = reply.send(to_json(koda_protocol::mcp::McpServerOauthLoginResponse {
                    authorization_url: login.authorization_url.clone(),
                }));
                // The browser comes back later: then the tokens are stored,
                // the client is told, and open sessions reconnect.
                let events = self.context.events.clone();
                let sessions: Vec<_> = self.sessions.lock().await.values().cloned().collect();
                let name = params.name;
                tokio::spawn(async move {
                    let result = login.wait().await;
                    let success = result.is_ok();
                    let _ = events.send(Outgoing::Notification(
                        koda_protocol::ServerNotification::McpServerOauthLoginCompleted(
                            koda_protocol::mcp::McpServerOauthLoginCompletedNotification {
                                name,
                                success,
                                error: result.err().map(|e| format!("{e:#}")),
                            },
                        ),
                    ));
                    if success {
                        for session in sessions {
                            let (done, _rx) = oneshot::channel();
                            let _ = session.send(SessionCommand::ReloadMcp { done });
                        }
                    }
                });
                return;
            }
            ClientRequest::McpServerOauthLogout { params, .. } => {
                let removed = koda_core::mcp::oauth::logout(&params.name)
                    .map_err(|e| internal(format!("{e:#}")));
                let _ = reply.send(removed.and_then(|removed| {
                    to_json(koda_protocol::mcp::McpServerOauthLogoutResponse { removed })
                }));
                return;
            }
            ClientRequest::ModelProviderList { .. } => {
                let _ = reply.send(to_json(crate::settings::list_providers(
                    &self.context.config,
                )));
                return;
            }
            ClientRequest::SessionMemoryRead { params, .. } => {
                let notes = self
                    .project_root(&params.session_id)
                    .and_then(|root| crate::memory::read(&root));
                let _ = reply.send(notes.and_then(to_json));
                return;
            }
            ClientRequest::SessionMemoryAdd { params, .. } => {
                let added = self
                    .project_root(&params.session_id)
                    .and_then(|root| crate::memory::add(&root, params.scope, &params.text));
                let _ = reply.send(added.and_then(to_json));
                return;
            }
            ClientRequest::SessionBackgroundTerminalsList { params, .. } => {
                let listed = self
                    .agent(&params.session_id)
                    .map(|agent| crate::background::list(&agent));
                let _ = reply.send(listed.and_then(to_json));
                return;
            }
            ClientRequest::SessionBackgroundTerminalsTerminate { params, .. } => {
                let stopped = self
                    .agent(&params.session_id)
                    .and_then(|agent| crate::background::terminate(&agent, &params.process_id));
                let _ = reply.send(stopped.and_then(to_json));
                return;
            }
            ClientRequest::SessionBackgroundTerminalsClean { params, .. } => {
                let cleaned = self
                    .agent(&params.session_id)
                    .map(|agent| crate::background::clean(&agent));
                let _ = reply.send(cleaned.and_then(to_json));
                return;
            }
            ClientRequest::AgentList { params, .. } => {
                let listed = self
                    .agent(&params.session_id)
                    .map(|agent| crate::background::list_agents(&agent));
                let _ = reply.send(listed.and_then(to_json));
                return;
            }
            ClientRequest::SkillsList { params, .. } => {
                let listed = crate::skills::list(params, &self.context.project_root);
                let _ = reply.send(to_json(listed));
                return;
            }
            ClientRequest::FuzzyFileSearch { params, .. } => {
                let found =
                    crate::fuzzy_file_search::fuzzy_file_search(&self.searches, params).await;
                let _ = reply.send(found.and_then(to_json));
                return;
            }
            ClientRequest::SessionList { params, .. } => {
                let _ = reply.send(self.list_sessions(params).await.and_then(to_json));
                return;
            }
            ClientRequest::SessionSearch { params, .. } => {
                let _ = reply.send(self.search_sessions(params).await.and_then(to_json));
                return;
            }
            // The shared server answers this itself; a host in the
            // client's process has no server to stop.
            ClientRequest::ServeStop { .. } => {
                let _ = reply.send(Err(invalid(
                    "serve/stop is for the shared server (koda serve); this host runs in the \
                     client's process"
                        .to_string(),
                )));
                return;
            }
            ClientRequest::FeedbackSave { params, .. } => {
                let saved = crate::feedback::save(&self.context.db, params).await;
                let _ = reply.send(saved.and_then(to_json));
                return;
            }
            ClientRequest::SessionRead { params, .. } => {
                let _ = reply.send(self.read_session(params).await.and_then(to_json));
                return;
            }
            ClientRequest::SessionTurnsList { params, .. } => {
                let _ = reply.send(self.list_turns(params).await.and_then(to_json));
                return;
            }
            ClientRequest::SessionDelete { params, .. } => {
                let _ = reply.send(
                    self.delete_session(params.session_id)
                        .await
                        .and_then(to_json),
                );
                return;
            }
            ClientRequest::SessionArchive { params, .. } => {
                let archived = self.archive_session(params.session_id).await;
                self.answer_then_tell(reply, archived, |session_id| {
                    ServerNotification::SessionArchived(SessionArchivedNotification { session_id })
                });
                return;
            }
            ClientRequest::SessionUnarchive { params, .. } => {
                let restored = self.unarchive_session(params.session_id).await;
                self.answer_then_tell(reply, restored, |session_id| {
                    ServerNotification::SessionUnarchived(SessionUnarchivedNotification {
                        session_id,
                    })
                });
                return;
            }
            ClientRequest::SessionNameSet { params, .. } => {
                let _ = reply.send(self.set_name(params).await.and_then(to_json));
                return;
            }
            ClientRequest::SessionConfidentialSet { params, .. } => {
                let confidential = params.confidential;
                let set = self.set_confidential(params).await;
                self.answer_then_tell(reply, set, |session_id| {
                    confidential_updated(session_id, confidential)
                });
                return;
            }
            ClientRequest::SessionFork { params, .. } => {
                let origin = self.origin(client.as_deref(), params.ephemeral);
                let _ = reply.send(self.fork_session(params, origin).await.and_then(to_json));
                return;
            }
            ClientRequest::SessionUnsubscribe { params, .. } => {
                let status = self.unsubscribe(&params.session_id).await;
                let _ = reply.send(to_json(SessionUnsubscribeResponse { status }));
                return;
            }
            // The session's task answers these, a turn's start before its
            // first notification and an interrupt once the turn has stopped.
            ClientRequest::TurnStart { params, .. } => (
                params.session_id.clone(),
                SessionCommand::StartTurn { params, reply },
            ),
            ClientRequest::TurnInterrupt { params, .. } => (
                params.session_id.clone(),
                SessionCommand::Interrupt { params, reply },
            ),
            ClientRequest::TurnSteer { params, .. } => (
                params.session_id.clone(),
                SessionCommand::Steer { params, reply },
            ),
            ClientRequest::SessionSettingsUpdate { params, .. } => (
                params.session_id.clone(),
                SessionCommand::UpdateSettings { params, reply },
            ),
            // Answered at once; the compaction runs as a turn.
            ClientRequest::SessionCompactStart { params, .. } => {
                (params.session_id, SessionCommand::Compact { reply })
            }
            ClientRequest::SessionRevert { params, .. } => (
                params.session_id,
                SessionCommand::Rewind {
                    before_turn_id: params.before_turn_id,
                    reply,
                },
            ),
            // The session's task answers these, in order with its turns.
            ClientRequest::SessionGoalSet { params, .. } => (
                params.session_id.clone(),
                SessionCommand::Goal {
                    request: GoalRequest::Set(params),
                    reply,
                },
            ),
            ClientRequest::SessionGoalGet { params, .. } => (
                params.session_id,
                SessionCommand::Goal {
                    request: GoalRequest::Get,
                    reply,
                },
            ),
            ClientRequest::SessionGoalClear { params, .. } => (
                params.session_id,
                SessionCommand::Goal {
                    request: GoalRequest::Clear,
                    reply,
                },
            ),
        };
        self.to_session(&session_id, command).await;
    }

    /// Hand `command` to session `session_id`, or refuse it.
    async fn to_session(&self, session_id: &str, command: SessionCommand) {
        let sessions = self.sessions.lock().await;
        let Some(session) = sessions.get(session_id) else {
            command.refuse(invalid(format!("no open session {session_id:?}")));
            return;
        };
        if let Err(mpsc::error::SendError(command)) = session.send(command) {
            command.refuse(internal(format!("session {session_id} has stopped")));
        }
    }

    /// `mcpServerStatus/list`: a session's servers, or without one the
    /// configured servers, none started.
    /// The project an open session works in.
    fn project_root(&self, id: &str) -> Result<PathBuf, ErrorObject> {
        self.open_handles(id).map(|handles| handles.project_root)
    }

    /// The agent an open session runs.
    fn agent(&self, id: &str) -> Result<Arc<koda_core::agent::KodaAgent>, ErrorObject> {
        self.open_handles(id).map(|handles| handles.agent)
    }

    fn open_handles(&self, id: &str) -> Result<crate::session::SessionHandles, ErrorObject> {
        self.handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .cloned()
            .ok_or_else(|| invalid(format!("no open session {id:?}")))
    }

    async fn mcp_status(
        &self,
        params: koda_protocol::mcp::ListMcpServerStatusParams,
    ) -> Result<koda_protocol::mcp::ListMcpServerStatusResponse, ErrorObject> {
        let Some(id) = &params.session_id else {
            let configured = crate::mcp::configured(&self.context.config_file)?;
            return Ok(crate::mcp::configured_status(configured, &params));
        };
        let mcp = self
            .handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .map(|handles| handles.mcp.clone())
            .ok_or_else(|| invalid(format!("no open session {id:?}")))?;
        Ok(match mcp {
            Some(mcp) => crate::mcp::session_status(&mcp, &params).await,
            None => koda_protocol::mcp::ListMcpServerStatusResponse::default(),
        })
    }

    /// The store's query for `listing`, without a limit.
    fn listing_query(
        &self,
        listing: Listing<'_>,
    ) -> Result<koda_state::session_store::SessionListQuery, ErrorObject> {
        use koda_state::session_store::{SessionListQuery, source};
        let home = koda_config::runtime_env::get("HOME").map(PathBuf::from);
        let folder = |cwd: &str| {
            listed_folder(cwd, &self.context.project_root, home.as_deref())
                .to_string_lossy()
                .into_owned()
        };
        let project_roots = listing.cwd.map(|cwd| match cwd {
            SessionListCwd::One(cwd) => vec![folder(cwd)],
            SessionListCwd::Many(cwds) => cwds.iter().map(|cwd| folder(cwd)).collect(),
        });
        // Without any, the sessions a person started, as Codex's
        // `thread/list` lists its interactive sources.
        let kinds = match listing.source_kinds {
            None | Some([]) => &[
                SessionSourceKind::Cli,
                SessionSourceKind::AppServer,
                SessionSourceKind::Unknown,
            ][..],
            Some(kinds) => kinds,
        };
        let sources = kinds
            .iter()
            .filter_map(|kind| match kind {
                SessionSourceKind::Cli => Some(source::CLI),
                SessionSourceKind::Exec => Some(source::EXEC),
                SessionSourceKind::AppServer => Some(source::APP_SERVER),
                SessionSourceKind::SubAgent => Some(source::SUB_AGENT),
                SessionSourceKind::Unknown => None,
            })
            .map(str::to_string)
            .collect();
        let after = match listing.cursor {
            None => None,
            Some(cursor) => Some(
                serde_json::from_str::<ListCursor>(cursor)
                    .map(|cursor| (cursor.at, cursor.seq))
                    .map_err(|_| {
                        invalid(format!("not a cursor from {}: {cursor}", listing.method))
                    })?,
            ),
        };
        Ok(SessionListQuery {
            project_roots,
            sources: Some(sources),
            unknown_source: kinds.contains(&SessionSourceKind::Unknown),
            providers: None,
            archived: listing.archived.unwrap_or(false),
            search_term: None,
            by_updated: listing.sort_key == Some(SessionSortKey::UpdatedAt),
            ascending: listing.sort_direction == Some(SortDirection::Asc),
            after,
            limit: 0,
        })
    }

    async fn list_sessions(
        &self,
        params: SessionListParams,
    ) -> Result<SessionListResponse, ErrorObject> {
        use koda_state::session_store::{SessionListQuery, SessionStore};
        let limit = i64::from(params.limit.unwrap_or(DEFAULT_LIST_LIMIT));
        let query = SessionListQuery {
            providers: params
                .model_providers
                .filter(|providers| !providers.is_empty()),
            search_term: params
                .search_term
                .as_deref()
                .map(str::trim)
                .filter(|term| !term.is_empty())
                .map(str::to_string),
            // One more than asked says whether there is another page.
            limit: limit + 1,
            ..self.listing_query(Listing {
                method: "session/list",
                cwd: params.cwd.as_ref(),
                source_kinds: params.source_kinds.as_deref(),
                archived: params.archived,
                sort_key: params.sort_key,
                sort_direction: params.sort_direction,
                cursor: params.cursor.as_deref(),
            })?
        };
        let mut stored = self
            .context
            .db
            .list_sessions_page(&query)
            .await
            .map_err(|e| internal(format!("couldn't list sessions: {e:#}")))?;
        let more = stored.len() as i64 > limit;
        stored.truncate(limit.max(0) as usize);
        // The next page goes on from this one's last session.
        let next_cursor = stored.last().filter(|_| more).and_then(|last| {
            serde_json::to_string(&ListCursor::after(last, query.by_updated)).ok()
        });
        Ok(SessionListResponse {
            data: stored
                .iter()
                .map(|stored| self.with_status(session::describe(stored)))
                .collect(),
            next_cursor,
        })
    }

    /// `session/search`: the sessions that say `searchTerm`, in
    /// `session/list`'s order, each with its first match, as Codex's
    /// `thread/search`. A scan, as Codex's is: the store gives the sessions
    /// that may hold the term, in order, and each is read until the page is
    /// full.
    async fn search_sessions(
        &self,
        params: SessionSearchParams,
    ) -> Result<SessionSearchResponse, ErrorObject> {
        use koda_state::session_store::SessionListQuery;
        let term = params.search_term.trim();
        if term.is_empty() {
            return Err(invalid("session/search requires a non-empty searchTerm"));
        }
        if term.chars().count() > MAX_SEARCH_TERM {
            return Err(invalid(format!(
                "session/search takes a searchTerm of at most {MAX_SEARCH_TERM} characters"
            )));
        }
        let limit = params
            .limit
            .unwrap_or(DEFAULT_SEARCH_LIMIT)
            .clamp(1, MAX_SEARCH_LIMIT) as usize;
        let query = SessionListQuery {
            limit: SEARCH_SCAN_PAGE,
            ..self.listing_query(Listing {
                method: "session/search",
                cwd: params.cwd.as_ref(),
                source_kinds: params.source_kinds.as_deref(),
                archived: params.archived,
                sort_key: params.sort_key,
                sort_direction: params.sort_direction,
                cursor: params.cursor.as_deref(),
            })?
        };
        // What open sessions have said so far is searched too.
        self.store_open_sessions(None).await;

        let by_updated = query.by_updated;
        let scanned = self
            .scan_sessions(query, term, params.include_tool_output, limit, None)
            .await?;
        // The next page goes on from this one's last session.
        let next_cursor = scanned
            .found
            .last()
            .filter(|_| scanned.more)
            .and_then(|(last, _)| serde_json::to_string(&ListCursor::after(last, by_updated)).ok());
        Ok(SessionSearchResponse {
            data: scanned
                .found
                .into_iter()
                .map(|(stored, found)| SessionSearchResult {
                    session: self.with_status(session::describe(&stored)),
                    snippet: found.snippet,
                    turn_id: Some(found.turn_id),
                    item_id: Some(found.item_id),
                    matched: found.matched,
                })
                .collect(),
            next_cursor,
        })
    }

    /// The sessions among `query`'s that say `term`, in its order and
    /// `limit` at most, each with its first match: the scan under
    /// `session/search` and under the model's `SearchSessions`. The store
    /// gives the sessions that may hold the term, a page at a time, and
    /// each is read until `limit` are found.
    ///
    /// With a `viewer`, the search is a session's model's: only the
    /// sessions it may see, and a fork's first match is looked for after
    /// what it copied ([`Self::copied_rows`]).
    async fn scan_sessions(
        &self,
        mut query: koda_state::session_store::SessionListQuery,
        term: &str,
        tool_output: bool,
        limit: usize,
        viewer: Option<&session_archive::Viewer<'_>>,
    ) -> Result<Scanned, ErrorObject> {
        use koda_state::session_store::SessionStore;
        let db = &self.context.db;
        let matcher = crate::session_search::Term::new(term);
        let mut scanned = Scanned {
            found: Vec::with_capacity(limit),
            more: false,
        };
        'scan: loop {
            let candidates = db
                .search_candidates_page(&query, term)
                .await
                .map_err(|e| internal(format!("couldn't search sessions: {e:#}")))?;
            let full = candidates.len() as i64 >= query.limit;
            // A short page was the store's last.
            let after = candidates
                .last()
                .filter(|_| full)
                .map(|end| ListCursor::after(end, query.by_updated));
            for stored in candidates {
                if viewer.is_some_and(|viewer| !viewer.covers(&stored)) {
                    continue;
                }
                let turns = crate::history::turns(db, &stored.id, false)
                    .await
                    .map_err(history_error)?;
                let rows = crate::session_text::rows(&turns);
                let copied = match viewer {
                    Some(viewer) => self.copied_rows(viewer, &stored, &rows).await?,
                    None => 0,
                };
                let Some(found) =
                    crate::session_search::first_match(&rows[copied..], &matcher, tool_output)
                else {
                    continue;
                };
                if scanned.found.len() == limit {
                    scanned.more = true;
                    break 'scan;
                }
                scanned.found.push((stored, found));
            }
            match after {
                Some(end) => query.after = Some((end.at, end.seq)),
                None => break,
            }
        }
        Ok(scanned)
    }

    /// Have every open session store the turns it has sent so far, so a
    /// read of the store that follows sees them; all but session `except`,
    /// when the read is that session's own and leaves it out.
    async fn store_open_sessions(&self, except: Option<&str>) {
        let open: Vec<OpenSession> = self
            .sessions
            .lock()
            .await
            .iter()
            .filter(|(id, _)| Some(id.as_str()) != except)
            .map(|(_, session)| session.clone())
            .collect();
        let mut stored = Vec::new();
        for session in open {
            let (done, flushed) = oneshot::channel();
            if session.send(SessionCommand::FlushHistory { done }).is_ok() {
                stored.push(flushed);
            }
        }
        for flushed in stored {
            let _ = flushed.await;
        }
    }

    /// Whether session `id` is open here, having stored what it has sent so
    /// far, and its latest turn as it stands when it is.
    async fn settle(&self, id: &str) -> (bool, Option<crate::live_turn::LiveTurn>) {
        let Some(commands) = self.sessions.lock().await.get(id).cloned() else {
            return (false, None);
        };
        let (done, stored) = oneshot::channel();
        if commands
            .send(crate::session::SessionCommand::FlushHistory { done })
            .is_err()
        {
            return (true, None);
        }
        (true, stored.await.ok().flatten())
    }

    /// Session `id`'s turns as its clients were told of them. An open
    /// session's last turn may still be running, with items not stored yet
    /// or still open: laid over the stored turns as a join gets them, as
    /// Codex's `thread/read` merges a thread's active turn.
    async fn session_turns(&self, id: &str) -> Result<Vec<koda_protocol::turn::Turn>, ErrorObject> {
        let (running, live) = self.settle(id).await;
        let mut turns = crate::history::turns(&self.context.db, id, running)
            .await
            .map_err(history_error)?;
        if let Some(live) = live {
            live.over(&mut turns);
        }
        Ok(turns)
    }

    /// `session/read`: a stored session as `session/list` gives it, with its
    /// turns when asked, as Codex's `thread/read`.
    async fn read_session(
        &self,
        params: koda_protocol::session::SessionReadParams,
    ) -> Result<koda_protocol::session::SessionReadResponse, ErrorObject> {
        use koda_state::session_store::SessionStore;
        let stored = self
            .context
            .db
            .get_session(&params.session_id)
            .await
            .map_err(|e| internal(format!("couldn't read the session: {e:#}")))?
            .ok_or_else(|| {
                ErrorObject::new(
                    koda_protocol::rpc::codes::INVALID_PARAMS,
                    format!("no session {:?}", params.session_id),
                )
            })?;
        let mut session = self.with_status(session::describe(&stored));
        if params.include_turns {
            session.turns = self.session_turns(&session.id).await?;
        }
        Ok(koda_protocol::session::SessionReadResponse { session })
    }

    /// `session/turns/list`: a page of a session's turns, as Codex's
    /// `thread/turns/list`. It reads them all and pages in memory, as Codex
    /// does from its rollout file.
    async fn list_turns(
        &self,
        params: koda_protocol::session::SessionTurnsListParams,
    ) -> Result<koda_protocol::session::SessionTurnsListResponse, ErrorObject> {
        use koda_state::session_store::SessionStore;
        let exists = self
            .context
            .db
            .get_session(&params.session_id)
            .await
            .map_err(|e| internal(format!("couldn't read the session: {e:#}")))?
            .is_some();
        if !exists {
            return Err(ErrorObject::new(
                koda_protocol::rpc::codes::INVALID_PARAMS,
                format!("no session {:?}", params.session_id),
            ));
        }
        let turns = self.session_turns(&params.session_id).await?;
        crate::history::page(
            turns,
            params.cursor.as_deref(),
            params.limit,
            params.sort_direction.unwrap_or(SortDirection::Desc),
        )
    }

    /// `session/fork`: copy the stored session into a new one named "Fork
    /// of" its name, then open the copy as `session/resume` opens one, as
    /// Codex's `thread/fork` does. An open source first stores what it has
    /// sent, so the copy has everything its client has been told. The copy
    /// belongs to this host's project, where it opens. A confidential
    /// session's copy is confidential, and opens only where the session
    /// itself may ([`crate::settings::check_confidential`]).
    async fn fork_session(
        &self,
        params: koda_protocol::session::SessionForkParams,
        origin: koda_state::session_store::SessionOrigin,
    ) -> Result<koda_protocol::session::SessionForkResponse, ErrorObject> {
        use koda_state::session_store::SessionStore;
        let source = self
            .context
            .db
            .get_session(&params.session_id)
            .await
            .map_err(|e| internal(format!("couldn't read the session: {e:#}")))?
            .ok_or_else(|| invalid(format!("no session {:?}", params.session_id)))?;
        self.settle(&source.id).await;
        // The copy belongs to the folder it opens in: the one asked for, or
        // the host's.
        let root = match params.cwd.as_deref() {
            Some(cwd) => session_folder(cwd)?,
            None => self.context.project_root.clone(),
        };
        let name = fork_name(source.title.as_deref().or(source.preview.as_deref()));
        let id = self
            .context
            .db
            .fork_session(&source.id, &name, &root, &origin)
            .await
            .map_err(|e| internal(format!("couldn't fork the session: {e:#}")))?;
        let started = self
            .start_session(
                Some(id.clone()),
                SessionStartParams {
                    approval_policy: params.approval_policy,
                    model_provider: params.model_provider,
                    base_url: params.base_url,
                    model: params.model,
                    cwd: Some(root.display().to_string()),
                    config: params.config,
                    ephemeral: None,
                },
                origin,
            )
            .await;
        match started {
            Ok(started) => Ok(koda_protocol::session::SessionForkResponse {
                session: started.session,
            }),
            Err(e) => {
                // A copy that can't open is no use to anyone.
                let _ = self.context.db.delete_session(&id).await;
                Err(e)
            }
        }
    }

    /// `session/name/set`: store the name, and tell an open session its
    /// first prompt no longer titles it.
    async fn set_name(
        &self,
        params: koda_protocol::session::SessionNameSetParams,
    ) -> Result<koda_protocol::session::SessionNameSetResponse, ErrorObject> {
        use koda_state::session_store::SessionStore;
        let name = params.name.trim().to_string();
        if name.is_empty() {
            return Err(invalid("a session's name can't be empty".to_string()));
        }
        if name.chars().count() > MAX_SESSION_NAME {
            return Err(invalid(format!(
                "a session's name is at most {MAX_SESSION_NAME} characters"
            )));
        }
        let id = params.session_id;
        let stored = self
            .context
            .db
            .get_session(&id)
            .await
            .map_err(|e| internal(format!("couldn't read the session: {e:#}")))?;
        if stored.is_none() {
            return Err(invalid(format!("no session {id:?}")));
        }
        // An open session stores it on its own task, after what is queued.
        let open = {
            let sessions = self.sessions.lock().await;
            sessions.get(&id).map(|session| {
                let (done, stored) = oneshot::channel();
                let sent = session.send(SessionCommand::Named {
                    name: name.clone(),
                    done,
                });
                (sent.is_ok(), stored)
            })
        };
        let stored = match open {
            Some((true, stored)) => stored
                .await
                .unwrap_or_else(|_| Err("the session stopped".to_string())),
            _ => {
                // Not while another process writes it, as Codex takes the
                // writer lock to change a thread's stored name.
                let _writer = session::take_writer(&self.context.db, &id).await?;
                self.context
                    .db
                    .set_session_title(&id, &name)
                    .await
                    .map_err(|e| format!("{e:#}"))
            }
        };
        stored.map_err(|e| internal(format!("couldn't name the session: {e}")))?;
        let _ = self.context.events.send(Outgoing::Notification(
            ServerNotification::SessionNameUpdated(
                koda_protocol::session::SessionNameUpdatedNotification {
                    session_id: id,
                    session_name: name,
                },
            ),
        ));
        Ok(koda_protocol::session::SessionNameSetResponse {})
    }

    /// `session/confidential/set`: mark stored session `id` confidential,
    /// or unmark it, with the sessions of the sub-agents it ran. Marking
    /// reaches the sessions forked from it too, which hold copies of its
    /// text, and the answer says how many; unmarking leaves forks as they
    /// are. Open or not, archived or not, and whoever has it open: the
    /// mark is one value on the session's row that nothing a session
    /// writes for itself touches, so it takes no writer lock, and a user
    /// can mark a session another Koda process is running. A session
    /// already as asked is answered the same, with nothing to tell.
    /// Returns the sessions changed, for their
    /// `session/confidential/updated`.
    async fn set_confidential(
        &self,
        params: SessionConfidentialSetParams,
    ) -> Result<(SessionConfidentialSetResponse, Vec<String>), ErrorObject> {
        use koda_state::session_store::SessionStore;
        let id = params.session_id;
        self.stored(&id).await?;
        let failed = |e: anyhow::Error| internal(format!("couldn't mark the session: {e:#}"));
        let db = &self.context.db;
        if !params.confidential {
            let changed = db.set_session_confidential(&id, false).await;
            return Ok((
                SessionConfidentialSetResponse::default(),
                changed.map_err(failed)?,
            ));
        }
        let marked = db.mark_confidential_with_forks(&id).await.map_err(failed)?;
        let forks_marked = u32::try_from(marked.forks).unwrap_or(u32::MAX);
        Ok((
            SessionConfidentialSetResponse { forks_marked },
            marked.sessions,
        ))
    }

    /// Tell every client that `sessions` are confidential now, or no
    /// longer: `session/confidential/updated`, one each.
    fn tell_confidential(&self, sessions: Vec<String>, confidential: bool) {
        for session_id in sessions {
            let _ = self
                .context
                .events
                .send(Outgoing::Notification(confidential_updated(
                    session_id,
                    confidential,
                )));
        }
    }

    /// Close session `id` if it is open here, before it is deleted or
    /// archived, as Codex shuts a loaded thread down first: its running
    /// turn stops, which withdraws the requests still waiting in it
    /// (`serverRequest/resolved`), and its task ends.
    async fn close_for_removal(&self, id: &str) {
        let open = self.sessions.lock().await.remove(id);
        if let Some(session) = open {
            self.handles
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(id);
            let (done, stopped) = oneshot::channel();
            let _ = session.send(SessionCommand::Shutdown { done: Some(done) });
            // Dropped once the session's turn has stopped and said so. One
            // that takes too long is cut off, so its turn's end is still
            // said before the session is said to be closed.
            if tokio::time::timeout(SESSION_STOP, stopped).await.is_err() {
                tracing::warn!("session {id} didn't stop in time; cutting its turn off");
                session.cut_off().await;
            }
            self.closed(id);
        }
    }

    /// Answer `reply` with what `done` gives, then tell every client of
    /// each session it names with `notice`, as Codex answers
    /// `thread/archive` before its `thread/archived`s.
    fn answer_then_tell<T: serde::Serialize>(
        &self,
        reply: oneshot::Sender<Answer>,
        done: Result<(T, Vec<String>), ErrorObject>,
        notice: impl Fn(String) -> ServerNotification,
    ) {
        let (answer, sessions) = match done {
            Ok((answer, sessions)) => (to_json(answer), sessions),
            Err(e) => (Err(e), Vec::new()),
        };
        let _ = reply.send(answer);
        for session_id in sessions {
            let _ = self
                .context
                .events
                .send(Outgoing::Notification(notice(session_id)));
        }
    }

    /// Stored session `id`, for the requests that change one by its id.
    async fn stored(
        &self,
        id: &str,
    ) -> Result<koda_state::session_store::SessionInfo, ErrorObject> {
        use koda_state::session_store::SessionStore;
        self.context
            .db
            .get_session(id)
            .await
            .map_err(|e| internal(format!("couldn't read the session: {e:#}")))?
            .ok_or_else(|| invalid(format!("no session {id:?}")))
    }

    /// `session/resume` of an archived session is refused, in Codex's
    /// words for `thread/resume`. A session that isn't stored is left to
    /// the open, which says so.
    async fn refuse_archived(&self, id: &str) -> Result<(), ErrorObject> {
        use koda_state::session_store::SessionStore;
        let stored = self
            .context
            .db
            .get_session(id)
            .await
            .map_err(|e| internal(format!("couldn't read the session: {e:#}")))?;
        if stored.is_some_and(|stored| stored.archived_unix.is_some()) {
            return Err(invalid(format!(
                "session {id} is archived. Run `koda unarchive {id}` to unarchive it first."
            )));
        }
        Ok(())
    }

    /// `session/archive`: archive session `id` and the sessions of the
    /// sub-agents it ran, as Codex's `thread/archive` does. One that is
    /// open is closed first, whatever it is doing: its running turn stops
    /// and its waiting requests are withdrawn. Returns the sessions
    /// archived, for their `session/archived`.
    async fn archive_session(
        &self,
        id: String,
    ) -> Result<(SessionArchiveResponse, Vec<String>), ErrorObject> {
        use koda_state::session_store::SessionStore;
        let already = || invalid(format!("session {id} is archived"));
        if self.stored(&id).await?.archived_unix.is_some() {
            return Err(already());
        }
        self.close_for_removal(&id).await;
        // Not while another process writes it, as Codex's `thread/archive`
        // takes the thread's writer lock.
        let _writer = session::take_writer(&self.context.db, &id).await?;
        let archived = self
            .context
            .db
            .archive_session(&id)
            .await
            .map_err(|e| internal(format!("couldn't archive the session: {e:#}")))?;
        // Another client archived it meanwhile.
        if archived.is_empty() {
            return Err(already());
        }
        // A sub-agent's session a client had opened by its id.
        for descendant in archived.iter().skip(1) {
            self.close_for_removal(descendant).await;
        }
        Ok((SessionArchiveResponse {}, archived))
    }

    /// `session/unarchive`: bring session `id` back, with the sessions of
    /// the sub-agents it ran, as Codex's `thread/unarchive` does, and
    /// answer with it as `session/read` gives it. It isn't opened. Returns
    /// the sessions restored, for their `session/unarchived`.
    async fn unarchive_session(
        &self,
        id: String,
    ) -> Result<(SessionUnarchiveResponse, Vec<String>), ErrorObject> {
        use koda_state::session_store::SessionStore;
        let not_archived = || invalid(format!("session {id} is not archived"));
        if self.stored(&id).await?.archived_unix.is_none() {
            return Err(not_archived());
        }
        let restored = self
            .context
            .db
            .unarchive_session(&id)
            .await
            .map_err(|e| internal(format!("couldn't unarchive the session: {e:#}")))?;
        if restored.is_empty() {
            return Err(not_archived());
        }
        let session = self.with_status(session::describe(&self.stored(&id).await?));
        Ok((SessionUnarchiveResponse { session }, restored))
    }

    /// Delete session `id`, closing it first if it is open, as Codex's
    /// `thread/delete` does, and say so with `session/deleted`.
    async fn delete_session(&self, id: String) -> Result<SessionDeleteResponse, ErrorObject> {
        use koda_state::session_store::SessionStore;
        self.close_for_removal(&id).await;
        // Not while another process writes it, as Codex's `thread/delete`
        // takes the thread's writer lock. An id that names no session is
        // left to the lookup below.
        let _writer = match session::take_writer(&self.context.db, &id).await {
            Err(e) if e.code == codes::INVALID_REQUEST => return Err(e),
            held => held.ok(),
        };
        let deleted = self
            .context
            .db
            .delete_session(&id)
            .await
            .map_err(|e| internal(format!("couldn't delete the session: {e:#}")))?;
        if !deleted {
            return Err(invalid(format!("no session {id:?}")));
        }
        let _ =
            self.context
                .events
                .send(Outgoing::Notification(ServerNotification::SessionDeleted(
                    SessionDeletedNotification { session_id: id },
                )));
        Ok(SessionDeleteResponse {})
    }

    /// Drop the results of session tasks that have ended.
    async fn reap(&self) {
        let mut tasks = self.tasks.lock().await;
        while tasks.try_join_next().is_some() {}
    }

    /// Session `id`'s task failed: it is no longer open, and every client
    /// is told why.
    async fn forget_failed(&self, id: &str, message: &str) {
        self.sessions.lock().await.remove(id);
        self.handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id);
        let _ = self
            .context
            .events
            .send(Outgoing::Notification(ServerNotification::Error(
                koda_protocol::server::ErrorNotification {
                    session_id: id.to_string(),
                    turn_id: None,
                    error: koda_protocol::turn::TurnError {
                        message: format!("Koda's session stopped on an internal error: {message}"),
                        code: None,
                    },
                    will_retry: false,
                    retry: None,
                },
            )));
        self.closed(id);
    }

    /// `session` with what it is doing now: an open session's own account,
    /// or not loaded.
    fn with_status(
        &self,
        mut session: koda_protocol::session::Session,
    ) -> koda_protocol::session::Session {
        use koda_protocol::session::SessionStatus;
        session.status = Some(
            self.context
                .statuses
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&session.id)
                .cloned()
                .unwrap_or(SessionStatus::NotLoaded),
        );
        session
    }

    /// Session `id` is no longer open: say so, to every client, as Codex
    /// says a thread was unloaded.
    fn closed(&self, id: &str) {
        let was_open = self
            .context
            .statuses
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id)
            .is_some();
        if was_open {
            let _ = self.context.events.send(Outgoing::Notification(
                ServerNotification::SessionStatusChanged(
                    koda_protocol::session::SessionStatusChangedNotification {
                        session_id: id.to_string(),
                        status: koda_protocol::session::SessionStatus::NotLoaded,
                    },
                ),
            ));
        }
    }

    /// Close session `id`: its task stops its turn, if one runs, and ends.
    async fn unsubscribe(&self, id: &str) -> SessionUnsubscribeStatus {
        let Some(session) = self.sessions.lock().await.remove(id) else {
            return SessionUnsubscribeStatus::NotLoaded;
        };
        self.closed(id);
        self.handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id);
        let _ = session.send(SessionCommand::Shutdown { done: None });
        SessionUnsubscribeStatus::Unsubscribed
    }

    /// Where a session `client` starts comes from: the client by the name
    /// it gave (the host's own client when the connection gave none), and
    /// what that makes the session, as Codex tells its TUI's threads from
    /// `codex exec`'s and an app's.
    fn origin(
        &self,
        client: Option<&str>,
        ephemeral: bool,
    ) -> koda_state::session_store::SessionOrigin {
        use koda_state::session_store::source;
        let originator = client
            .map(str::to_string)
            .or_else(|| self.client.get().map(|info| info.name.clone()));
        let source = originator.as_deref().map(|name| match name {
            "koda-tui" => source::CLI,
            "koda-exec" => source::EXEC,
            _ => source::APP_SERVER,
        });
        koda_state::session_store::SessionOrigin {
            source: source.map(str::to_string),
            originator,
            parent_session_id: None,
            ephemeral,
        }
    }

    fn initialize(&self, client_info: ClientInfo) -> Answer {
        if self.client.set(client_info).is_err() {
            return Err(invalid("Already initialized"));
        }
        to_json(InitializeResponse {
            server_info: ServerInfo {
                name: "koda".to_string(),
                // Every Koda crate shares one version.
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        })
    }

    /// The mode a new session starts in: the config file's, with `--mode`
    /// over it, else the host's default.
    fn new_session_mode(&self) -> koda_core::trust::TrustMode {
        let config = &self.context.config;
        crate::config_file::load(&self.context.config_file)
            .ok()
            .and_then(|loaded| crate::config_file::mode(&loaded))
            .unwrap_or(config.trust)
    }

    /// The context a session opens with: the host's, in `cwd` when one is
    /// given, with `config`'s values over the run's, as Codex's
    /// `thread/start` takes a `cwd` and a `config` map.
    fn session_context(
        &self,
        cwd: Option<&str>,
        config: Option<&std::collections::BTreeMap<String, serde_json::Value>>,
    ) -> Result<session::SessionContext, ErrorObject> {
        let mut context = self.context.clone();
        if let Some(cwd) = cwd {
            context.project_root = session_folder(cwd)?;
        }
        if let Some(config) = config.filter(|config| !config.is_empty()) {
            let overrides = config
                .iter()
                .map(|(key, value)| koda_config::Override::from_json(key, value))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| invalid(format!("config: {e}")))?;
            context.config_file = context.config_file.with_more_overrides(overrides);
        }
        Ok(context)
    }

    /// The folder stored session `id` was started in, to resume it there;
    /// `None` for a session too old to record one, or one not stored (the
    /// open itself says so). A recorded folder that no longer exists is
    /// refused rather than swapped for the server's.
    async fn stored_folder(&self, id: &str) -> Result<Option<String>, ErrorObject> {
        use koda_state::session_store::SessionStore;
        let stored =
            self.context.db.get_session(id).await.map_err(|e| {
                ErrorObject::new(codes::INTERNAL_ERROR, format!("session {id}: {e:#}"))
            })?;
        let Some(folder) = stored.and_then(|stored| stored.project_root) else {
            return Ok(None);
        };
        if !Path::new(&folder).is_dir() {
            return Err(invalid(format!(
                "session {id}'s folder {folder} no longer exists; give a cwd to open it elsewhere"
            )));
        }
        Ok(Some(folder))
    }

    /// Open a session: a new one, or the stored session `resume` names
    /// (`session/resume`, and a fork's copy).
    async fn start_session(
        &self,
        resume: Option<String>,
        params: SessionStartParams,
        origin: koda_state::session_store::SessionOrigin,
    ) -> Result<SessionStartResponse, ErrorObject> {
        if let Some(id) = &resume
            && self.sessions.lock().await.contains_key(id)
        {
            return Err(invalid(format!("session {id} is already open")));
        }
        let policy = params.approval_policy.unwrap_or_default();
        let choice = crate::settings::ModelChoice {
            provider: params.model_provider,
            base_url: params.base_url,
            model: params.model,
        };
        let resuming = resume.is_some();
        let context = self.session_context(params.cwd.as_deref(), params.config.as_ref())?;
        let mut opened = session::open(context, resume, policy, choice, origin).await?;
        let id = opened.session.id.clone();
        // Its model reaches the other sessions through the host, as this
        // session may see them.
        opened.handles.agent.tools.set_session_archive(Arc::new(
            session_archive::HostArchive::new(self.me.clone(), id.clone()),
        ));
        // A resumed session comes back with its turns, as Codex's
        // `thread/resume` does.
        if resuming {
            opened.session.turns = crate::history::turns_for_resume(&self.context.db, &id)
                .await
                .map_err(history_error)?;
            // Its stored token counts, on the turn they were last counted
            // on, as Codex replays a thread's when a client attaches.
            if let (Some(token_usage), Some(last)) =
                (opened.restored_usage.take(), opened.session.turns.last())
            {
                let _ = self.context.events.send(Outgoing::Notification(
                    ServerNotification::SessionTokenUsageUpdated(
                        koda_protocol::session::SessionTokenUsageUpdatedNotification {
                            session_id: id.clone(),
                            turn_id: last.id.clone(),
                            token_usage,
                        },
                    ),
                ));
            }
        }
        let mut sessions = self.sessions.lock().await;
        // It may have been opened meanwhile.
        if sessions.contains_key(&id) {
            return Err(invalid(format!("session {id} is already open")));
        }
        self.handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id.clone(), opened.handles);
        // A panic in the session (a tool, a provider) ends this session
        // alone: its turn fails, its clients are told, and the host forgets
        // it, as Codex's unwinding build keeps the server up.
        let sink = opened.task.sink();
        let ended = self.ended.clone();
        let session_id = id.clone();
        let commands = opened.commands;
        let cut_off_by = Arc::clone(&sink);
        let task = self.tasks.lock().await.spawn(async move {
            use futures_util::FutureExt;
            let ran = std::panic::AssertUnwindSafe(opened.task.run())
                .catch_unwind()
                .await;
            let failed = ran.err().map(|panic| panic_message(panic.as_ref()));
            if let Some(message) = &failed {
                tracing::error!("koda serve: session {session_id} failed: {message}");
                sink.crashed(message);
            }
            let _ = ended.send((session_id, failed));
        });
        sessions.insert(
            id.clone(),
            OpenSession {
                commands,
                sink: cut_off_by,
                task,
            },
        );
        Ok(SessionStartResponse {
            session: opened.session,
        })
    }

    /// Hand the client's answer to the session whose request it is.
    async fn route(&self, id: RequestId, result: Answer) {
        let owner = self
            .context
            .open_requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&id)
            .cloned();
        let Some(session_id) = owner else {
            tracing::debug!("an answer to request {id}, which isn't open");
            return;
        };
        if let Some(session) = self.sessions.lock().await.get(&session_id) {
            let _ = session.send(SessionCommand::Respond { id, result });
        }
    }

    /// Tell every session to stop, wait a while for their turns to wind
    /// down, then stop what's left.
    async fn stop_sessions(&self) {
        self.handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        let open: Vec<OpenSession> = self.sessions.lock().await.drain().map(|(_, s)| s).collect();
        for session in &open {
            let _ = session.send(SessionCommand::Shutdown { done: None });
        }
        let mut tasks = self.tasks.lock().await;
        let _ = tokio::time::timeout(SESSION_STOP, async {
            while tasks.join_next().await.is_some() {}
        })
        .await;
        // One still running is cut off: its turn's end is said before its
        // task is stopped, so the client sees every turn it saw start end.
        for session in open.iter().filter(|session| !session.task.is_finished()) {
            tracing::warn!("a session didn't stop in time; cutting its turn off");
            session.cut_off().await;
        }
        tasks.shutdown().await;
    }

    async fn state(&self) -> Result<StateReadResponse, ErrorObject> {
        let config = &self.context.config;
        Ok(StateReadResponse {
            model: config.model.clone(),
            model_provider: config.provider_type.to_string(),
            base_url: config.base_url.clone(),
            context_window: config.max_context_tokens as u64,
            mode: self.new_session_mode(),
            sandbox: self.context.sandbox.clone(),
        })
    }
}

/// `session/confidential/updated` for session `session_id`.
fn confidential_updated(session_id: String, confidential: bool) -> ServerNotification {
    ServerNotification::SessionConfidentialUpdated(SessionConfidentialUpdatedNotification {
        session_id,
        confidential,
    })
}

/// A fork's name: "Fork of" what the source is called, within a name's
/// length, or "Fork" for a source with neither name nor prompt.
/// A session's folder as a client names it: absolute, an existing folder,
/// without symlinks, as the host's own is.
fn session_folder(cwd: &str) -> Result<PathBuf, ErrorObject> {
    let path = std::path::Path::new(cwd);
    if !path.is_absolute() {
        return Err(invalid(format!("cwd {cwd:?} must be an absolute path")));
    }
    let folder = path
        .canonicalize()
        .map_err(|e| invalid(format!("cwd {cwd:?}: {e}")))?;
    if !folder.is_dir() {
        return Err(invalid(format!("cwd {cwd:?} isn't a folder")));
    }
    Ok(folder)
}

/// The folder `session/list` filters on, as Codex resolves `thread/list`'s
/// `cwd`: `~` for the home folder, a relative path against the host's folder,
/// then `.` and `..` removed by the path's text, without touching the disk.
/// So `/a/../a/b` lists the sessions of `/a/b`, and a folder that no longer
/// exists still lists its sessions.
fn listed_folder(cwd: &str, host_folder: &Path, home: Option<&Path>) -> PathBuf {
    use std::path::Component;
    let written = match (cwd.strip_prefix('~'), home) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
            home.join(rest.trim_start_matches('/'))
        }
        _ => PathBuf::from(cwd),
    };
    let mut folder = PathBuf::new();
    for part in host_folder.join(written).components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                folder.pop();
            }
            other => folder.push(other),
        }
    }
    folder
}

fn fork_name(source: Option<&str>) -> String {
    let Some(source) = source.map(str::trim).filter(|s| !s.is_empty()) else {
        return "Fork".to_string();
    };
    let name = format!("Fork of {source}");
    if name.chars().count() <= MAX_SESSION_NAME {
        return name;
    }
    let mut name: String = name.chars().take(MAX_SESSION_NAME - 1).collect();
    name.push('\u{2026}');
    name
}

fn history_error(e: String) -> ErrorObject {
    internal(format!("couldn't read the session's history: {e}"))
}

pub(crate) fn to_json<T: serde::Serialize>(response: T) -> Answer {
    serde_json::to_value(response)
        .map_err(|e| ErrorObject::new(codes::INTERNAL_ERROR, format!("response encoding: {e}")))
}

#[cfg(test)]
mod fork_name_tests {
    use super::*;

    #[test]
    fn a_fork_is_named_after_its_source_within_a_name_s_length() {
        assert_eq!(fork_name(Some(" Parser work ")), "Fork of Parser work");
        assert_eq!(fork_name(Some("")), "Fork");
        assert_eq!(fork_name(None), "Fork");
        let long = fork_name(Some(&"x".repeat(200)));
        assert_eq!(long.chars().count(), MAX_SESSION_NAME);
        assert!(long.ends_with('\u{2026}'));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use koda_core::config::ProviderType;

    #[test]
    fn a_listed_folder_is_resolved_by_its_text() {
        let host = Path::new("/work/app");
        let home = Path::new("/home/me");
        let folder = |cwd: &str| listed_folder(cwd, host, Some(home));
        assert_eq!(folder("/work/app"), PathBuf::from("/work/app"));
        assert_eq!(folder("/work/app/"), PathBuf::from("/work/app"));
        assert_eq!(folder("/work/../work/app"), PathBuf::from("/work/app"));
        assert_eq!(folder("/work/./app/."), PathBuf::from("/work/app"));
        assert_eq!(folder("."), PathBuf::from("/work/app"));
        assert_eq!(folder("../lib"), PathBuf::from("/work/lib"));
        assert_eq!(folder("/../../x"), PathBuf::from("/x"));
        assert_eq!(folder("~"), PathBuf::from("/home/me"));
        assert_eq!(folder("~/src/../repo"), PathBuf::from("/home/me/repo"));
        // `~name` isn't this user's home; it stays a folder name.
        assert_eq!(folder("~other"), PathBuf::from("/work/app/~other"));
        assert_eq!(
            listed_folder("~/repo", host, None),
            PathBuf::from("/work/app/~/repo")
        );
    }

    /// A host on a scratch database. Keep the directory while it runs.
    async fn host() -> (mpsc::Sender<HostCommand>, JoinHandle<()>, tempfile::TempDir) {
        let (tx, handle, dir, _) = host_with_events().await;
        (tx, handle, dir)
    }

    /// [`host`], with what it sends its client.
    async fn host_with_events() -> (
        mpsc::Sender<HostCommand>,
        JoinHandle<()>,
        tempfile::TempDir,
        mpsc::UnboundedReceiver<Outgoing>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::init(dir.path()).await.unwrap();
        let (events, received) = mpsc::unbounded_channel();
        let (tx, handle) = spawn(HostArgs {
            config: KodaConfig::default_for_testing(ProviderType::LMStudio),
            config_file: koda_config::ConfigFile::in_dir(dir.path()),
            config_warnings: Vec::new(),
            project_root: dir.path().to_path_buf(),
            db,
            sandbox: SandboxState {
                backend: "seatbelt".to_string(),
                available: true,
                reason: None,
                setup_hint: None,
            },
            events,
        });
        (tx, handle, dir, received)
    }
    use serde_json::json;

    /// A session whose task panics ends alone: every client is told, the
    /// host forgets it, and it opens again from the store. The host and
    /// its other sessions go on, as in Codex's unwinding build.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_session_that_panics_ends_alone_and_opens_again() {
        let (tx, host, _dir, mut events) = host_with_events().await;
        ask(&tx, "initialize", initialize()).await.unwrap();
        let start = |_: ()| ask(&tx, "session/start", json!({}));
        let failing = start(()).await.unwrap()["session"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let other = start(()).await.unwrap()["session"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        tx.send(HostCommand::PanicSession {
            session_id: failing.clone(),
        })
        .await
        .unwrap();
        // Told why, then that it isn't open.
        let mut told = false;
        loop {
            // Long enough for a busy machine.
            let message = tokio::time::timeout(std::time::Duration::from_secs(30), events.recv())
                .await
                .expect("the failure is said")
                .unwrap();
            match message {
                Outgoing::Notification(ServerNotification::Error(error)) => {
                    assert_eq!(error.session_id, failing);
                    assert!(error.error.message.contains("a test's panic"), "{error:?}");
                    told = true;
                }
                Outgoing::Notification(ServerNotification::SessionStatusChanged(changed))
                    if changed.session_id == failing =>
                {
                    assert_eq!(
                        changed.status,
                        koda_protocol::session::SessionStatus::NotLoaded
                    );
                    break;
                }
                _ => {}
            }
        }
        assert!(told, "the error comes first");

        // The host goes on: the other session still answers, and the failed
        // one opens again from the store.
        let read = ask(&tx, "session/read", json!({"sessionId": other}))
            .await
            .unwrap();
        assert_eq!(read["session"]["status"]["type"], "idle", "{read}");
        let reopened = ask(&tx, "session/resume", json!({"sessionId": failing}))
            .await
            .unwrap();
        assert_eq!(reopened["session"]["id"], failing.as_str());
        assert!(!host.is_finished());
    }

    /// The store gives a search its candidates a page at a time, and a
    /// candidate only seems to match when the term is in text that isn't
    /// searched. A search reads on through such pages to the sessions that
    /// do match, and its cursor goes on from the last one it gave.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_search_reads_past_a_page_of_sessions_that_only_seemed_to_match() {
        use koda_state::session_store::{ITEM_KIND, SessionStore, TurnRecord};
        let (tx, _host, dir) = host().await;
        ask(&tx, "initialize", initialize()).await.unwrap();
        let db = Database::init(dir.path()).await.unwrap();
        let session_with = |item: Value| {
            let db = db.clone();
            let root = dir.path().to_path_buf();
            async move {
                let session = db.create_session("default", &root).await.unwrap();
                let record = TurnRecord {
                    turn_id: "t1".into(),
                    kind: ITEM_KIND.into(),
                    payload: item.to_string(),
                };
                db.append_turn_records(&session, &[record]).await.unwrap();
                session
            }
        };
        let says = json!({"type": "agentMessage", "id": "m1", "text": "An okapi, at last."});
        // The model's reasoning is stored and not searched.
        let thinks = json!({"type": "reasoning", "id": "r1", "text": "Is it an okapi?"});
        let oldest = session_with(says.clone()).await;
        for _ in 0..SEARCH_SCAN_PAGE + 3 {
            session_with(thinks.clone()).await;
        }
        let newest = session_with(says).await;

        let ids = |found: &Value| -> Vec<String> {
            found["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|result| result["session"]["id"].as_str().unwrap().to_string())
                .collect()
        };
        let all = ask(&tx, "session/search", json!({"searchTerm": "okapi"}))
            .await
            .unwrap();
        assert_eq!(ids(&all), [newest.clone(), oldest.clone()], "{all}");
        assert!(all["nextCursor"].is_null(), "{all}");

        let first = ask(
            &tx,
            "session/search",
            json!({"searchTerm": "okapi", "limit": 1}),
        )
        .await
        .unwrap();
        assert_eq!(ids(&first), [newest], "{first}");
        let second = ask(
            &tx,
            "session/search",
            json!({"searchTerm": "okapi", "limit": 1, "cursor": first["nextCursor"]}),
        )
        .await
        .unwrap();
        assert_eq!(ids(&second), [oldest], "{second}");
        assert!(second["nextCursor"].is_null(), "{second}");
    }

    async fn ask(tx: &mpsc::Sender<HostCommand>, method: &str, params: Value) -> Answer {
        let (reply, answer) = oneshot::channel();
        let request = rpc::Request::new(RequestId::Integer(1), method, Some(params));
        tx.send(HostCommand::Request {
            request,
            reply,
            client: None,
        })
        .await
        .unwrap();
        answer.await.unwrap()
    }

    fn initialize() -> Value {
        json!({"clientInfo": {"name": "test", "version": "1"}})
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_stops_the_host() {
        let (tx, host, _dir) = host().await;
        let (done, stopped) = oneshot::channel();
        tx.send(HostCommand::Shutdown { done }).await.unwrap();
        stopped.await.unwrap();
        host.await.unwrap();
        assert!(tx.is_closed());
    }
}
