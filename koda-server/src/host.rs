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
use std::path::PathBuf;
use std::sync::atomic::AtomicI64;
use std::sync::{Arc, OnceLock, PoisonError};
use std::time::Duration;

use koda_core::config::KodaConfig;
use koda_protocol::ServerNotification;
use koda_protocol::initialize::{ClientInfo, InitializeResponse, ServerInfo};
use koda_protocol::rpc::{self, ErrorObject, RequestId, codes};
use koda_protocol::session::{
    SessionDeleteResponse, SessionDeletedNotification, SessionListParams, SessionListResponse,
    SessionStartParams, SessionStartResponse, SessionUnsubscribeResponse, SessionUnsubscribeStatus,
    SortDirection,
};
use koda_protocol::state::{SandboxState, StateReadResponse};
use koda_protocol::{ClientRequest, PROTOCOL_VERSION};
use koda_state::db::Database;
use serde_json::Value;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

use crate::events::Outgoing;
use crate::session::{
    self, OpenRequests, SessionCommand, SessionContext, SessionHandles, internal, invalid,
};

/// Open sessions' in-process handles, by id; shared with the client.
pub(crate) type HandleMap = Arc<std::sync::Mutex<HashMap<String, SessionHandles>>>;

/// How long shutdown waits for sessions to stop their turns. A turn bounds
/// its own wind-down at five seconds; the client stops the host by force
/// after its own five.
const SESSION_STOP: Duration = Duration::from_secs(4);

/// How many sessions `session/list` gives without a limit.
const DEFAULT_LIST_LIMIT: u32 = 50;

/// The longest name `session/name/set` takes, in characters.
const MAX_SESSION_NAME: usize = 100;

/// A request's answer: the method's response as JSON, or an error.
pub(crate) type Answer = Result<Value, ErrorObject>;

/// What a client sends the host.
pub(crate) enum HostCommand {
    /// Answer `request` on `reply`.
    Request {
        request: rpc::Request,
        reply: oneshot::Sender<Answer>,
    },
    /// The client's answer to the server request `id`.
    Response { id: RequestId, result: Answer },
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
    let host = Arc::new(Host {
        context: SessionContext {
            config: args.config,
            config_file: args.config_file,
            project_root: args.project_root,
            db: args.db,
            sandbox: args.sandbox,
            events: args.events,
            request_ids: Arc::new(AtomicI64::new(0)),
            open_requests: OpenRequests::default(),
            run_checked: Arc::default(),
        },
        handles: HandleMap::default(),
        client: OnceLock::new(),
        config_warnings: std::sync::Mutex::new(args.config_warnings),
        sessions: Mutex::new(HashMap::new()),
        tasks: Mutex::new(JoinSet::new()),
    });
    (tx, tokio::spawn(run(host, rx)))
}

struct Host {
    /// What every session shares; its config is what new sessions start with.
    context: SessionContext,
    /// Set by `initialize`; until then every other request is refused.
    client: OnceLock<ClientInfo>,
    /// Sent once, right after `initialize` is answered.
    config_warnings: std::sync::Mutex<Vec<koda_protocol::config::ConfigWarningNotification>>,
    /// Open sessions' command channels, by id.
    sessions: Mutex<HashMap<String, mpsc::UnboundedSender<SessionCommand>>>,
    /// Open sessions' in-process handles, by id.
    handles: HandleMap,
    /// Their tasks. Dropping the host stops them.
    tasks: Mutex<JoinSet<()>>,
}

async fn run(host: Arc<Host>, mut commands: mpsc::Receiver<HostCommand>) {
    let mut running = JoinSet::new();
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(HostCommand::Request { request, reply }) => {
                    let host = Arc::clone(&host);
                    running.spawn(async move { host.handle(request, reply).await });
                }
                Some(HostCommand::Response { id, result }) => host.route(id, result).await,
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

impl Host {
    async fn handle(&self, request: rpc::Request, reply: oneshot::Sender<Answer>) {
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
                let _ = reply.send(self.start_session(params).await.and_then(to_json));
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
            ClientRequest::SessionList { params, .. } => {
                let _ = reply.send(self.list_sessions(params).await.and_then(to_json));
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
            ClientRequest::SessionNameSet { params, .. } => {
                let _ = reply.send(self.set_name(params).await.and_then(to_json));
                return;
            }
            ClientRequest::SessionFork { params, .. } => {
                let _ = reply.send(self.fork_session(params).await.and_then(to_json));
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

    async fn list_sessions(
        &self,
        params: SessionListParams,
    ) -> Result<SessionListResponse, ErrorObject> {
        use koda_state::session_store::SessionStore;
        let root = params
            .cwd
            .map(PathBuf::from)
            .unwrap_or_else(|| self.context.project_root.clone());
        let limit = i64::from(params.limit.unwrap_or(DEFAULT_LIST_LIMIT));
        // The cursor is how many came before; opaque to clients.
        let offset = match params.cursor.as_deref() {
            None => 0,
            Some(cursor) => cursor
                .parse::<i64>()
                .ok()
                .filter(|n| *n >= 0)
                .ok_or_else(|| invalid(format!("not a cursor from session/list: {cursor}")))?,
        };
        // One more than asked says whether there is another page.
        let mut stored = self
            .context
            .db
            .list_sessions_from(limit + 1, offset, &root)
            .await
            .map_err(|e| internal(format!("couldn't list sessions: {e:#}")))?;
        let more = stored.len() as i64 > limit;
        stored.truncate(limit.max(0) as usize);
        Ok(SessionListResponse {
            data: stored.iter().map(session::describe).collect(),
            next_cursor: more.then(|| (offset + limit).to_string()),
        })
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
        let mut session = session::describe(&stored);
        if params.include_turns {
            // An open session's last turn may still be running.
            let running = self.sessions.lock().await.contains_key(&session.id);
            session.turns = crate::history::turns(&self.context.db, &session.id, running)
                .await
                .map_err(history_error)?;
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
        // An open session's last turn may still be running, and what it has
        // sent may not be stored yet: wait for that, so the list has
        // everything a client has already been told.
        let open = self.sessions.lock().await.get(&params.session_id).cloned();
        let running = open.is_some();
        if let Some(commands) = open {
            let (done, stored) = oneshot::channel();
            if commands
                .send(crate::session::SessionCommand::FlushHistory { done })
                .is_ok()
            {
                let _ = stored.await;
            }
        }
        let turns = crate::history::turns(&self.context.db, &params.session_id, running)
            .await
            .map_err(history_error)?;
        crate::history::page(
            turns,
            params.cursor.as_deref(),
            params.limit,
            params.sort_direction.unwrap_or(SortDirection::Desc),
        )
    }

    /// `session/fork`: copy the stored session into a new one named "Fork
    /// of" its name, then open the copy as `session/start` resumes one, as
    /// Codex's `thread/fork` does. An open source first stores what it has
    /// sent, so the copy has everything its client has been told.
    async fn fork_session(
        &self,
        params: koda_protocol::session::SessionForkParams,
    ) -> Result<koda_protocol::session::SessionForkResponse, ErrorObject> {
        use koda_state::session_store::SessionStore;
        let source = self
            .context
            .db
            .get_session(&params.session_id)
            .await
            .map_err(|e| internal(format!("couldn't read the session: {e:#}")))?
            .ok_or_else(|| invalid(format!("no session {:?}", params.session_id)))?;
        let open = self.sessions.lock().await.get(&source.id).cloned();
        if let Some(commands) = open {
            let (done, stored) = oneshot::channel();
            if commands
                .send(crate::session::SessionCommand::FlushHistory { done })
                .is_ok()
            {
                let _ = stored.await;
            }
        }
        let name = fork_name(source.title.as_deref().or(source.preview.as_deref()));
        let id = self
            .context
            .db
            .fork_session(&source.id, &name)
            .await
            .map_err(|e| internal(format!("couldn't fork the session: {e:#}")))?;
        let started = self
            .start_session(SessionStartParams {
                session_id: Some(id.clone()),
                approval_policy: params.approval_policy,
                model_provider: params.model_provider,
                base_url: params.base_url,
                model: params.model,
            })
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

    /// Delete session `id`, closing it first if it is open, as Codex's
    /// `thread/delete` does, and say so with `session/deleted`.
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
            _ => self
                .context
                .db
                .set_session_title(&id, &name)
                .await
                .map_err(|e| format!("{e:#}")),
        };
        stored.map_err(|e| internal(format!("couldn't name the session: {e}")))?;
        let _ = self.context.events.send(Outgoing::Notification(
            ServerNotification::SessionNameUpdated(
                koda_protocol::session::SessionNameUpdatedNotification {
                    session_id: id,
                    name,
                },
            ),
        ));
        Ok(koda_protocol::session::SessionNameSetResponse {})
    }

    async fn delete_session(&self, id: String) -> Result<SessionDeleteResponse, ErrorObject> {
        use koda_state::session_store::SessionStore;
        let open = self.sessions.lock().await.remove(&id);
        if let Some(session) = open {
            self.handles
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
            let (done, stopped) = oneshot::channel();
            let _ = session.send(SessionCommand::Shutdown { done: Some(done) });
            // Dropped once the session's task has ended, its turn stopped.
            let _ = tokio::time::timeout(SESSION_STOP, stopped).await;
        }
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

    /// Close session `id`: its task stops its turn, if one runs, and ends.
    async fn unsubscribe(&self, id: &str) -> SessionUnsubscribeStatus {
        let Some(session) = self.sessions.lock().await.remove(id) else {
            return SessionUnsubscribeStatus::NotLoaded;
        };
        self.handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id);
        let _ = session.send(SessionCommand::Shutdown { done: None });
        SessionUnsubscribeStatus::Unsubscribed
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
            protocol_version: PROTOCOL_VERSION,
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

    async fn start_session(
        &self,
        params: SessionStartParams,
    ) -> Result<SessionStartResponse, ErrorObject> {
        if let Some(id) = &params.session_id
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
        let resuming = params.session_id.is_some();
        let mut opened =
            session::open(self.context.clone(), params.session_id, policy, choice).await?;
        let id = opened.session.id.clone();
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
        sessions.insert(id, opened.commands);
        self.tasks.lock().await.spawn(opened.task.run());
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
        for session in self.sessions.lock().await.drain().map(|(_, s)| s) {
            let _ = session.send(SessionCommand::Shutdown { done: None });
        }
        let mut tasks = self.tasks.lock().await;
        let _ = tokio::time::timeout(SESSION_STOP, async {
            while tasks.join_next().await.is_some() {}
        })
        .await;
        tasks.shutdown().await;
    }

    async fn state(&self) -> Result<StateReadResponse, ErrorObject> {
        let config = &self.context.config;
        Ok(StateReadResponse {
            model: config.model.clone(),
            provider: config.provider_type.to_string(),
            base_url: config.base_url.clone(),
            context_window: config.max_context_tokens as u64,
            mode: self.new_session_mode(),
            sandbox: self.context.sandbox.clone(),
        })
    }
}

/// A fork's name: "Fork of" what the source is called, within a name's
/// length, or "Fork" for a source with neither name nor prompt.
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

    /// A host on a scratch database. Keep the directory while it runs.
    async fn host() -> (mpsc::Sender<HostCommand>, JoinHandle<()>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::init(dir.path()).await.unwrap();
        let (events, _) = mpsc::unbounded_channel();
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
        (tx, handle, dir)
    }
    use serde_json::json;

    async fn ask(tx: &mpsc::Sender<HostCommand>, method: &str, params: Value) -> Answer {
        let (reply, answer) = oneshot::channel();
        let request = rpc::Request::new(RequestId::Integer(1), method, Some(params));
        tx.send(HostCommand::Request { request, reply })
            .await
            .unwrap();
        answer.await.unwrap()
    }

    fn initialize() -> Value {
        json!({"clientInfo": {"name": "test", "version": "1"}})
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn requests_before_initialize_are_refused_and_so_is_a_second_initialize() {
        let (tx, _host, _dir) = host().await;

        let early = ask(&tx, "state/read", json!({})).await.unwrap_err();
        assert_eq!(early.code, codes::INVALID_REQUEST);
        assert_eq!(early.message, "Not initialized");

        let first = ask(&tx, "initialize", initialize()).await.unwrap();
        assert_eq!(first["serverInfo"]["name"], "koda");
        assert_eq!(first["protocolVersion"], PROTOCOL_VERSION);

        let again = ask(&tx, "initialize", initialize()).await.unwrap_err();
        assert_eq!(again.code, codes::INVALID_REQUEST);
        assert_eq!(again.message, "Already initialized");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unknown_method_and_bad_params_are_errors_not_crashes() {
        let (tx, _host, _dir) = host().await;
        ask(&tx, "initialize", initialize()).await.unwrap();

        let unknown = ask(&tx, "thread/start", json!({})).await.unwrap_err();
        assert_eq!(unknown.code, codes::METHOD_NOT_FOUND);
        let bad = ask(&tx, "state/read", json!({"sessionId": "x"}))
            .await
            .unwrap_err();
        assert_eq!(bad.code, codes::INVALID_PARAMS);
        // The host still answers after both.
        assert!(ask(&tx, "state/read", json!({})).await.is_ok());
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
