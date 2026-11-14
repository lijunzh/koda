//! The client API every Koda front end uses: start Koda, send requests,
//! and read the answers.
//!
//! [`InProcessClient`] runs Koda's host on a task in the same process. A
//! request goes out typed and comes back typed, but passes through its JSON
//! form on the way, exactly as it would over stdio or a socket; one contract
//! by construction. Codex's `app-server-client` is the model.
//!
//! A session opens with `session/start` and closes with
//! `session/unsubscribe`; `turn/start`, `turn/interrupt` and `turn/steer`
//! drive its turns. What a turn does arrives on one ordered
//! queue, [`InProcessClient::next_event`]: notifications, and the server
//! requests (approvals, questions, the loop cap) the client answers with
//! [`InProcessClient::resolve_server_request`] or
//! [`InProcessClient::reject_server_request`]. The queue is unbounded, as
//! Codex's client queue is, so unread events never hold up a response.
//!
//! [`events::EventProjector`] turns a session's engine events into those
//! messages; `koda exec --json` prints them.
//!
//! In process only, and only for now, a front end whose renderer still
//! reads engine events can ask for them as they are
//! ([`StartArgs::raw_events`]), as Codex's TUI read its old events while it
//! moved onto its app-server. They go when the TUI renders items. For the same
//! move, a front end can hand the host its own agent ([`StartArgs::agent`]),
//! reach a session's sub-agents ([`InProcessClient::session_handles`]) and set
//! what its next turns run with ([`InProcessClient::configure_session`]).

pub mod builtin_skills;
pub mod events;
mod host;
mod session;
mod settings;

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use koda_core::config::KodaConfig;
use koda_core::db::Database;
use koda_protocol::initialize::{ClientInfo, InitializeParams, InitializeResponse};
use koda_protocol::rpc::{ErrorObject, RequestId};
use koda_protocol::{Method, methods};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

pub use koda_protocol as protocol;

use events::Outgoing;
use host::{HostArgs, HostCommand};
pub use session::SessionHandles;

/// How long [`InProcessClient::shutdown`] waits before stopping the host by force.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// What [`InProcessClient::start`] needs.
pub struct StartArgs {
    /// What new sessions start with.
    pub config: KodaConfig,
    /// The project sessions work in.
    pub project_root: PathBuf,
    /// Where sessions are stored.
    pub db: Database,
    /// Also hand out every engine event as it is
    /// ([`InProcessClient::take_raw_events`]). In process only.
    pub raw_events: bool,
    /// Who the client is, sent in `initialize`.
    pub client_info: ClientInfo,
    /// In process only: the agent every session uses, for a front end that
    /// builds its own (its tools, MCP servers and system prompt). Without
    /// one, each session builds an agent from `config`.
    pub agent: Option<Arc<koda_core::agent::KodaAgent>>,
}

/// A client with Koda's host running in this process.
pub struct InProcessClient {
    requests: RequestHandle,
    host: JoinHandle<()>,
    events: mpsc::UnboundedReceiver<Outgoing>,
    raw_events: Option<mpsc::UnboundedReceiver<RawEvent>>,
    handles: host::HandleMap,
    server: InitializeResponse,
}

/// An engine event as the engine sent it, for a front end whose renderer
/// still reads them ([`StartArgs::raw_events`]).
#[derive(Debug, Clone)]
pub struct RawEvent {
    /// The session it came from.
    pub session_id: String,
    /// The tool call that started the sub-agent it came from, if one did.
    pub parent: Option<String>,
    /// The event.
    pub event: koda_core::engine::EngineEvent,
}

/// Sends requests to the host. Cheap to clone, so a task can send one
/// while another reads events.
#[derive(Clone)]
pub struct RequestHandle {
    commands: mpsc::Sender<HostCommand>,
    next_id: Arc<AtomicI64>,
}

impl RequestHandle {
    /// Call method `M` and wait for its response.
    pub async fn request<M: Method>(&self, params: M::Params) -> Result<M::Response, RequestError> {
        let id = RequestId::Integer(self.next_id.fetch_add(1, Ordering::Relaxed));
        let request =
            koda_protocol::encode::<M>(id, &params).map_err(|source| RequestError::Encode {
                method: M::NAME,
                source,
            })?;
        let (reply, answer) = oneshot::channel();
        let closed = || RequestError::Closed { method: M::NAME };
        self.commands
            .send(HostCommand::Request { request, reply })
            .await
            .map_err(|_| closed())?;
        let result = answer
            .await
            .map_err(|_| closed())?
            .map_err(|error| RequestError::Server {
                method: M::NAME,
                error,
            })?;
        serde_json::from_value(result).map_err(|source| RequestError::Decode {
            method: M::NAME,
            source,
        })
    }
}

impl InProcessClient {
    /// Start the host and run the `initialize` handshake.
    pub async fn start(args: StartArgs) -> Result<Self, RequestError> {
        // Stored API keys reach the sessions through the environment; one
        // already set there wins.
        if let Err(e) = koda_core::keystore::inject_into_env(&args.db).await {
            tracing::warn!("couldn't load the stored API keys: {e:#}");
        }
        let (events_tx, events) = mpsc::unbounded_channel();
        let (raw_tx, raw_events) = if args.raw_events {
            let (tx, rx) = mpsc::unbounded_channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let handles = host::HandleMap::default();
        let (commands, host) = host::spawn(HostArgs {
            config: args.config,
            project_root: args.project_root,
            db: args.db,
            events: events_tx,
            raw: raw_tx,
            agent: args.agent,
            handles: Arc::clone(&handles),
        });
        let mut client = Self {
            requests: RequestHandle {
                commands,
                next_id: Arc::new(AtomicI64::new(1)),
            },
            host,
            events,
            raw_events,
            handles,
            server: InitializeResponse {
                server_info: koda_protocol::initialize::ServerInfo {
                    name: String::new(),
                    version: String::new(),
                },
                protocol_version: 0,
            },
        };
        client.server = client
            .request::<methods::Initialize>(InitializeParams {
                client_info: args.client_info,
            })
            .await?;
        Ok(client)
    }

    /// What the server said in `initialize`.
    pub fn server(&self) -> &InitializeResponse {
        &self.server
    }

    /// Call method `M` and wait for its response.
    pub async fn request<M: Method>(&self, params: M::Params) -> Result<M::Response, RequestError> {
        self.requests.request::<M>(params).await
    }

    /// A handle that sends requests, for another task.
    pub fn request_handle(&self) -> RequestHandle {
        self.requests.clone()
    }

    /// The engine events, as they are, when [`StartArgs::raw_events`] asked
    /// for them. Only the first call gets them.
    pub fn take_raw_events(&mut self) -> Option<mpsc::UnboundedReceiver<RawEvent>> {
        self.raw_events.take()
    }

    /// In process only: open session `session_id`'s handles, for a front end
    /// that still reaches into a session while it moves onto the protocol.
    pub fn session_handles(&self, session_id: &str) -> Option<SessionHandles> {
        self.handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
            .cloned()
    }

    /// In process only: what session `session_id`'s next turns run with
    /// (model, provider, address and the rest), until the protocol has
    /// requests for them. A running turn keeps what it started with; the
    /// answer comes once the change has taken effect.
    pub async fn configure_session(
        &self,
        session_id: &str,
        config: KodaConfig,
    ) -> Result<(), RequestError> {
        const METHOD: &str = "configure";
        let (done, answer) = oneshot::channel();
        self.requests
            .commands
            .send(HostCommand::Configure {
                session_id: session_id.to_string(),
                config: Box::new(config),
                done,
            })
            .await
            .map_err(|_| RequestError::Closed { method: METHOD })?;
        answer
            .await
            .map_err(|_| RequestError::Closed { method: METHOD })?
            .map(|_| ())
            .map_err(|error| RequestError::Server {
                method: METHOD,
                error,
            })
    }

    /// The next notification or server request, in the order Koda produced
    /// them. `None` once the host has stopped.
    pub async fn next_event(&mut self) -> Option<Outgoing> {
        self.events.recv().await
    }

    /// The next notification or server request if one is already queued.
    pub fn try_next_event(&mut self) -> Option<Outgoing> {
        self.events.try_recv().ok()
    }

    /// Answer the server request `id` with `result`, the request's response
    /// type as JSON.
    pub async fn resolve_server_request(
        &self,
        id: RequestId,
        result: serde_json::Value,
    ) -> Result<(), RequestError> {
        self.respond(id, Ok(result)).await
    }

    /// Refuse the server request `id`. Koda reads it as no: the approval is
    /// refused, the question gets an empty answer, the loop stops.
    pub async fn reject_server_request(
        &self,
        id: RequestId,
        error: ErrorObject,
    ) -> Result<(), RequestError> {
        self.respond(id, Err(error)).await
    }

    async fn respond(
        &self,
        id: RequestId,
        result: Result<serde_json::Value, ErrorObject>,
    ) -> Result<(), RequestError> {
        self.requests
            .commands
            .send(HostCommand::Response { id, result })
            .await
            .map_err(|_| RequestError::Closed { method: "response" })
    }

    /// Stop the host, stopping running turns and cancelling requests still
    /// running. Waits up to five
    /// seconds, then stops it by force.
    pub async fn shutdown(self) {
        let Self {
            requests, mut host, ..
        } = self;
        let commands = requests.commands;
        let (done, stopped) = oneshot::channel();
        if commands.send(HostCommand::Shutdown { done }).await.is_ok() {
            let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, stopped).await;
        }
        if tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut host)
            .await
            .is_err()
        {
            tracing::warn!("Koda's host didn't stop in time; stopping it by force");
            host.abort();
        }
    }
}

/// Why a request failed.
#[derive(Debug)]
pub enum RequestError {
    /// The host has stopped.
    Closed {
        /// The method.
        method: &'static str,
    },
    /// Koda answered with an error.
    Server {
        /// The method.
        method: &'static str,
        /// Koda's error.
        error: ErrorObject,
    },
    /// The parameters wouldn't encode: a bug in the caller's types.
    Encode {
        /// The method.
        method: &'static str,
        /// What serde said.
        source: serde_json::Error,
    },
    /// The response didn't fit the method's type: client and server disagree.
    Decode {
        /// The method.
        method: &'static str,
        /// What serde said.
        source: serde_json::Error,
    },
}

impl fmt::Display for RequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed { method } => write!(f, "{method}: Koda's host has stopped"),
            Self::Server { method, error } => write!(f, "{method} failed: {error}"),
            Self::Encode { method, source } => {
                write!(f, "{method}: params didn't encode: {source}")
            }
            Self::Decode { method, source } => {
                write!(f, "{method}: response didn't decode: {source}")
            }
        }
    }
}

impl std::error::Error for RequestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Closed { .. } => None,
            Self::Server { error, .. } => Some(error),
            Self::Encode { source, .. } | Self::Decode { source, .. } => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use koda_core::config::ProviderType;
    use koda_core::trust::TrustMode;
    use koda_protocol::state::{self, StateReadParams};

    /// Start arguments on a scratch database. Keep the directory while the
    /// client runs.
    async fn args(config: KodaConfig) -> (StartArgs, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::init(dir.path()).await.unwrap();
        let args = StartArgs {
            config,
            project_root: dir.path().to_path_buf(),
            db,
            raw_events: false,
            client_info: ClientInfo {
                name: "koda-client-test".to_string(),
                version: "0".to_string(),
            },
            agent: None,
        };
        (args, dir)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_runs_the_handshake() {
        let config = KodaConfig::default_for_testing(ProviderType::LMStudio);
        let (args, _dir) = args(config).await;
        let client = InProcessClient::start(args).await.unwrap();
        assert_eq!(client.server().server_info.name, "koda");
        assert_eq!(
            client.server().server_info.version,
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(
            client.server().protocol_version,
            koda_protocol::PROTOCOL_VERSION
        );
        client.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn state_read_reports_what_a_new_session_starts_with() {
        let mut config = KodaConfig::default_for_testing(ProviderType::LMStudio);
        config.model = "stub-model".to_string();
        // Never contacted: reading state makes no model call.
        config.base_url = "http://127.0.0.1:9/v1".to_string();
        config.max_context_tokens = 32_768;
        config.trust = TrustMode::Safe;
        let (args, _dir) = args(config).await;
        let client = InProcessClient::start(args).await.unwrap();

        let state = client
            .request::<methods::StateRead>(StateReadParams {})
            .await
            .unwrap();
        assert_eq!(state.model, "stub-model");
        assert_eq!(state.provider, "lm-studio");
        assert_eq!(state.base_url, "http://127.0.0.1:9/v1");
        assert_eq!(state.context_window, 32_768);
        assert_eq!(state.mode, state::TrustMode::Safe);
        let report = koda_core::sandbox::dependency_report();
        assert_eq!(state.sandbox.backend, report.backend);
        assert_eq!(state.sandbox.available, report.available);
        client.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn requests_run_side_by_side() {
        let config = KodaConfig::default_for_testing(ProviderType::LMStudio);
        let (args, _dir) = args(config).await;
        let client = InProcessClient::start(args).await.unwrap();
        let (a, b) = tokio::join!(
            client.request::<methods::StateRead>(StateReadParams {}),
            client.request::<methods::StateRead>(StateReadParams {}),
        );
        assert_eq!(a.unwrap(), b.unwrap());
        client.shutdown().await;
    }
}
