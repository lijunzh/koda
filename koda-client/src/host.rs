//! The host: the task that owns Koda's engine state and answers requests.
//!
//! It takes requests in their wire form and answers with a JSON value or a
//! JSON-RPC error, whatever the transport, so the in-process client and a
//! later `koda serve` share one contract. Codex's in-process app-server
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
use koda_core::db::Database;
use koda_protocol::ServerNotification;
use koda_protocol::initialize::{ClientInfo, InitializeResponse, ServerInfo};
use koda_protocol::rpc::{self, ErrorObject, RequestId, codes};
use koda_protocol::session::{
    SessionDeleteResponse, SessionDeletedNotification, SessionListParams, SessionListResponse,
    SessionStartParams, SessionStartResponse, SessionUnsubscribeResponse, SessionUnsubscribeStatus,
};
use koda_protocol::state::{SandboxState, StateReadResponse, TrustMode};
use koda_protocol::storage::{StoragePurgeParams, StoragePurgeResponse, StorageReadResponse};
use koda_protocol::{ClientRequest, PROTOCOL_VERSION};
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
    /// In process only: what session `session_id`'s next turns run with.
    Configure {
        session_id: String,
        config: Box<KodaConfig>,
        done: oneshot::Sender<Answer>,
    },
    /// Stop every session and the host; say so on `done`.
    Shutdown { done: oneshot::Sender<()> },
}

/// What a host is started with.
pub(crate) struct HostArgs {
    /// What new sessions start with.
    pub config: KodaConfig,
    /// The project sessions work in.
    pub project_root: PathBuf,
    /// Where sessions are stored.
    pub db: Database,
    /// The client's queue of notifications and server requests.
    pub events: mpsc::UnboundedSender<Outgoing>,
    /// Where engine events go as they are, if the client asked for them.
    pub raw: Option<mpsc::UnboundedSender<crate::RawEvent>>,
    /// In process only: the agent every session uses, if the client built it.
    pub agent: Option<Arc<koda_core::agent::KodaAgent>>,
    /// Where open sessions' handles go, for the client.
    pub handles: HandleMap,
}

/// Start a host on its own task.
pub(crate) fn spawn(args: HostArgs) -> (mpsc::Sender<HostCommand>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(64);
    let host = Arc::new(Host {
        context: SessionContext {
            config: args.config,
            project_root: args.project_root,
            db: args.db,
            events: args.events,
            request_ids: Arc::new(AtomicI64::new(0)),
            open_requests: OpenRequests::default(),
            raw: args.raw,
            agent: args.agent,
        },
        handles: args.handles,
        client: OnceLock::new(),
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
                // In the order it was sent, so it reaches the session before a
                // turn the client starts next.
                Some(HostCommand::Configure { session_id, config, done }) => {
                    let command = SessionCommand::Configure { config, done };
                    host.to_session(&session_id, command).await;
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
                let _ = reply.send(self.initialize(params.client_info));
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
            ClientRequest::ModelProviderList { .. } => {
                let _ = reply.send(to_json(crate::settings::list_providers(
                    &self.context.config,
                )));
                return;
            }
            ClientRequest::SessionList { params, .. } => {
                let _ = reply.send(self.list_sessions(params).await.and_then(to_json));
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
            ClientRequest::StorageRead { .. } => {
                let _ = reply.send(self.read_storage().await.and_then(to_json));
                return;
            }
            ClientRequest::StoragePurge { params, .. } => {
                let _ = reply.send(self.purge_storage(params).await.and_then(to_json));
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

    async fn list_sessions(
        &self,
        params: SessionListParams,
    ) -> Result<SessionListResponse, ErrorObject> {
        use koda_core::persistence::Persistence;
        let root = params
            .cwd
            .map(PathBuf::from)
            .unwrap_or_else(|| self.context.project_root.clone());
        let limit = params.limit.unwrap_or(DEFAULT_LIST_LIMIT);
        let stored = self
            .context
            .db
            .list_sessions(i64::from(limit), &root)
            .await
            .map_err(|e| internal(format!("couldn't list sessions: {e:#}")))?;
        Ok(SessionListResponse {
            data: stored.iter().map(session::describe).collect(),
        })
    }

    /// Delete session `id`, closing it first if it is open, as Codex's
    /// `thread/delete` does, and say so with `session/deleted`.
    async fn delete_session(&self, id: String) -> Result<SessionDeleteResponse, ErrorObject> {
        use koda_core::persistence::Persistence;
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

    async fn read_storage(&self) -> Result<StorageReadResponse, ErrorObject> {
        use koda_core::persistence::Persistence;
        let stats = self
            .context
            .db
            .compacted_stats()
            .await
            .map_err(|e| internal(format!("couldn't read the store: {e:#}")))?;
        let count = |n: i64| u64::try_from(n).unwrap_or(0);
        Ok(StorageReadResponse {
            compacted_messages: count(stats.message_count),
            sessions: count(stats.session_count),
            size_bytes: count(stats.size_bytes),
            oldest_compacted_at: stats.oldest_unix,
        })
    }

    async fn purge_storage(
        &self,
        params: StoragePurgeParams,
    ) -> Result<StoragePurgeResponse, ErrorObject> {
        use koda_core::persistence::Persistence;
        let deleted = self
            .context
            .db
            .purge_compacted(params.min_age_days.unwrap_or(0))
            .await
            .map_err(|e| internal(format!("couldn't purge: {e:#}")))?;
        Ok(StoragePurgeResponse {
            deleted: deleted as u64,
        })
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
        let opened = session::open(self.context.clone(), params.session_id, policy, choice).await?;
        let id = opened.session.id.clone();
        let mut sessions = self.sessions.lock().await;
        // A prefix may have resolved to a session opened meanwhile.
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
        // The sandbox check may start a process, so it runs off the async threads.
        let report = tokio::task::spawn_blocking(koda_core::sandbox::dependency_report)
            .await
            .map_err(|e| internal(format!("sandbox check failed: {e}")))?;
        let config = &self.context.config;
        Ok(StateReadResponse {
            model: config.model.clone(),
            provider: config.provider_type.to_string(),
            base_url: config.base_url.clone(),
            context_window: config.max_context_tokens as u64,
            mode: protocol_mode(config.trust),
            sandbox: SandboxState {
                backend: report.backend.to_string(),
                available: report.available,
                reason: report.reason,
            },
        })
    }
}

pub(crate) fn protocol_mode(mode: koda_core::trust::TrustMode) -> TrustMode {
    match mode {
        koda_core::trust::TrustMode::Plan => TrustMode::Plan,
        koda_core::trust::TrustMode::Safe => TrustMode::Safe,
        koda_core::trust::TrustMode::Auto => TrustMode::Auto,
    }
}

pub(crate) fn to_json<T: serde::Serialize>(response: T) -> Answer {
    serde_json::to_value(response)
        .map_err(|e| ErrorObject::new(codes::INTERNAL_ERROR, format!("response encoding: {e}")))
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
            project_root: dir.path().to_path_buf(),
            db,
            events,
            raw: None,
            agent: None,
            handles: HandleMap::default(),
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
