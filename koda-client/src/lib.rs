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
//! [`stdio::serve`] runs the same host for a client on the other end of a
//! pipe, `koda serve --stdio`, as `codex app-server` serves stdio.
//!
//! The host builds every session's agent; a front end never hands it one.

mod background;
pub mod builtin_skills;
mod config_file;
pub mod events;
/// `feedback/save`: a report ZIP, as Codex writes one.
pub mod feedback;
mod history;
mod host;
mod mcp;
mod memory;
mod session;
mod settings;
mod skills;
pub mod stdio;

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use koda_core::config::KodaConfig;
use koda_protocol::initialize::{ClientInfo, InitializeParams, InitializeResponse};
use koda_protocol::rpc::{ErrorObject, RequestId};
use koda_protocol::{Method, methods};
use koda_state::db::Database;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

pub use koda_config::ConfigFile;
pub use koda_protocol as protocol;

use events::Outgoing;
use host::{HostArgs, HostCommand};

/// How long [`InProcessClient::shutdown`] waits before stopping the host by force.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// What [`InProcessClient::start`] needs.
pub struct StartArgs {
    /// The config file, `~/.config/koda/config.toml` in production
    /// ([`ConfigFile::in_dir`] of Koda's config folder). Tests pass a
    /// scratch one.
    pub config_file: ConfigFile,
    /// The project sessions work in.
    pub project_root: PathBuf,
    /// Koda's folder, `~/.config/koda` in production
    /// ([`koda_config::config_dir`]). The host keeps its database here, as
    /// Codex's server keeps its state under `$CODEX_HOME`; no client opens
    /// it. Tests pass a scratch one.
    pub home: PathBuf,
    /// Who the client is, sent in `initialize`.
    pub client_info: ClientInfo,
}

/// The input history Koda kept in its database before the TUI kept its own
/// file (`history.jsonl`, as Codex's TUI does), oldest first; empty when
/// there is none or it can't be read. A front end reads it once, to bring
/// it over. `home` is Koda's folder, as [`StartArgs::home`].
pub async fn earlier_input_history(home: &std::path::Path) -> Vec<String> {
    match Database::init(home).await {
        Ok(db) => db.history_load().await.unwrap_or_default(),
        Err(e) => {
            tracing::warn!("couldn't read the earlier input history: {e:#}");
            Vec::new()
        }
    }
}

/// A client with Koda's host running in this process.
pub struct InProcessClient {
    requests: RequestHandle,
    host: JoinHandle<()>,
    events: mpsc::UnboundedReceiver<Outgoing>,
    server: InitializeResponse,
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
    /// Start the host and run the `initialize` handshake. A config file
    /// that can't be read or isn't valid stops it here, naming the line;
    /// settings earlier versions kept in the database move into the file
    /// first, once.
    ///
    /// What new sessions start with comes from the config file, with this
    /// run's `-c` values over it, as Codex's server builds its config from
    /// `config.toml` and the overrides it is given: the agent it names
    /// (`agent`), that agent's model and settings, and the file's model
    /// choice and settings over them for each new session.
    pub async fn start(args: StartArgs) -> Result<Self, RequestError> {
        Self::start_on(args, None, None).await
    }

    /// [`Self::start`] with new sessions starting from `config` rather
    /// than the agent the config file names, for tests that need settings
    /// no key gives (a scripted server's address, an iteration cap).
    #[doc(hidden)]
    pub async fn start_with_config(
        args: StartArgs,
        config: KodaConfig,
    ) -> Result<Self, RequestError> {
        Self::start_on(args, Some(config), None).await
    }

    /// [`Self::start_with_config`] on a machine whose sandbox is
    /// `sandbox`, for tests of the mode rules: a host with no sandbox
    /// refuses Auto.
    #[doc(hidden)]
    pub async fn start_with_sandbox(
        args: StartArgs,
        config: KodaConfig,
        sandbox: koda_protocol::state::SandboxState,
    ) -> Result<Self, RequestError> {
        Self::start_on(args, Some(config), Some(sandbox)).await
    }

    async fn start_on(
        args: StartArgs,
        config: Option<KodaConfig>,
        sandbox: Option<koda_protocol::state::SandboxState>,
    ) -> Result<Self, RequestError> {
        let StartArgs {
            config_file,
            project_root,
            home,
            client_info,
        } = args;
        let launched = launch(
            HostStart {
                config_file,
                project_root,
                home,
            },
            config,
            sandbox,
        )
        .await?;
        let mut client = Self {
            requests: launched.requests,
            host: launched.host,
            events: launched.events,
            server: InitializeResponse {
                server_info: koda_protocol::initialize::ServerInfo {
                    name: String::new(),
                    version: String::new(),
                },
                protocol_version: 0,
            },
        };
        client.server = client
            .request::<methods::Initialize>(InitializeParams { client_info })
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
        stop_host(&self.requests.commands, self.host).await;
    }
}

/// Where a host keeps its state and works.
pub(crate) struct HostStart {
    pub config_file: ConfigFile,
    pub project_root: PathBuf,
    pub home: PathBuf,
}

/// A running host, not yet initialized: the way in, its task, and its
/// queue of notifications and server requests.
pub(crate) struct Launched {
    pub requests: RequestHandle,
    pub host: JoinHandle<()>,
    pub events: mpsc::UnboundedReceiver<Outgoing>,
}

/// Open the database, read the config and start a host. Its client sends
/// `initialize`: the in-process client itself, a `koda serve` client over
/// the wire.
pub(crate) async fn launch(
    args: HostStart,
    config: Option<KodaConfig>,
    sandbox: Option<koda_protocol::state::SandboxState>,
) -> Result<Launched, RequestError> {
    let db = Database::init(&args.home)
        .await
        .map_err(|e| RequestError::Database(format!("{e:#}")))?;
    config_file::migrate(&db, &args.config_file).await;
    let loaded = args.config_file.load().map_err(RequestError::Config)?;
    let config = match config {
        Some(config) => config,
        None => {
            config_file::host_config(&args.project_root, &loaded).map_err(RequestError::Agent)?
        }
    };
    let sandbox = match sandbox {
        Some(sandbox) => sandbox,
        // The check may start a process, so it runs off the async
        // threads.
        None => tokio::task::spawn_blocking(settings::probe_sandbox)
            .await
            .unwrap_or_else(|e| koda_protocol::state::SandboxState {
                backend: "none".to_string(),
                available: false,
                reason: Some(format!("the sandbox check failed: {e}")),
                setup_hint: None,
            }),
    };
    let config_warnings = config_file::warnings(&args.config_file, &loaded);
    // Commands any session starts may use local ports if the file says
    // so; each new session takes the file again.
    koda_core::sandbox::set_local_binding_configured(
        loaded.config.sandbox.allow_local_binding.unwrap_or(false),
    );
    // Stored API keys reach the sessions through the environment; one
    // already set there wins.
    if let Err(e) = koda_core::keystore::inject_into_env(&db).await {
        tracing::warn!("couldn't load the stored API keys: {e:#}");
    }
    let (events_tx, events) = mpsc::unbounded_channel();
    let (commands, host) = host::spawn(HostArgs {
        config,
        config_file: args.config_file,
        config_warnings,
        project_root: args.project_root,
        db,
        sandbox,
        events: events_tx,
    });
    Ok(Launched {
        requests: RequestHandle {
            commands,
            next_id: Arc::new(AtomicI64::new(1)),
        },
        host,
        events,
    })
}

/// Stop a host, stopping running turns and cancelling requests still
/// running. Waits up to five seconds, then stops it by force.
pub(crate) async fn stop_host(commands: &mpsc::Sender<HostCommand>, mut host: JoinHandle<()>) {
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
    /// The config file couldn't be read, or isn't valid: the host didn't
    /// start.
    Config(koda_config::ConfigError),
    /// Koda's database couldn't be opened: the host didn't start.
    Database(String),
    /// The agent the config names (`agent`, `--agent`) couldn't be
    /// loaded: the host didn't start.
    Agent(String),
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
            Self::Config(error) => error.fmt(f),
            Self::Database(error) => write!(f, "couldn't open Koda's database: {error}"),
            Self::Agent(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for RequestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Closed { .. } => None,
            Self::Server { error, .. } => Some(error),
            Self::Encode { source, .. } | Self::Decode { source, .. } => Some(source),
            Self::Config(error) => Some(error),
            Self::Database(_) | Self::Agent(_) => None,
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
    async fn args() -> (StartArgs, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let args = StartArgs {
            config_file: ConfigFile::in_dir(dir.path()),
            home: dir.path().to_path_buf(),
            project_root: dir.path().to_path_buf(),
            client_info: ClientInfo {
                name: "koda-client-test".to_string(),
                version: "0".to_string(),
            },
        };
        (args, dir)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_runs_the_handshake() {
        let config = KodaConfig::default_for_testing(ProviderType::LMStudio);
        let (args, _dir) = args().await;
        let client = InProcessClient::start_with_config(args, config)
            .await
            .unwrap();
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

    /// A config file that isn't valid stops the host before it starts,
    /// naming the line, as Codex refuses a bad `config.toml`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_broken_config_file_stops_the_start() {
        let config = KodaConfig::default_for_testing(ProviderType::LMStudio);
        let (args, dir) = args().await;
        std::fs::write(
            dir.path().join("config.toml"),
            "model = \"a\"\nmodel_provider = \n",
        )
        .unwrap();
        let Err(error) = InProcessClient::start_with_config(args, config).await else {
            panic!("started on a broken config file");
        };
        assert!(matches!(error, RequestError::Config(_)), "{error:?}");
        assert!(error.to_string().contains("line 2"), "{error}");
    }

    /// A key Koda doesn't know is loaded past and reported right after
    /// `initialize`, as Codex sends `configWarning`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_config_keys_are_reported_after_initialize() {
        use koda_protocol::ServerNotification;
        let config = KodaConfig::default_for_testing(ProviderType::LMStudio);
        let (args, dir) = args().await;
        std::fs::write(dir.path().join("config.toml"), "modle = \"claude-opus\"\n").unwrap();
        let mut client = InProcessClient::start_with_config(args, config)
            .await
            .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(5), client.next_event())
            .await
            .unwrap();
        let Some(Outgoing::Notification(ServerNotification::ConfigWarning(warning))) = event else {
            panic!("expected a config warning, got {event:?}");
        };
        assert!(warning.summary.contains("`modle`"), "{warning:?}");
        assert_eq!(
            warning.path.as_deref(),
            Some(
                dir.path()
                    .join("config.toml")
                    .display()
                    .to_string()
                    .as_str()
            )
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
        let (args, _dir) = args().await;
        let client = InProcessClient::start_with_config(args, config)
            .await
            .unwrap();

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
        let (args, _dir) = args().await;
        let client = InProcessClient::start_with_config(args, config)
            .await
            .unwrap();
        let (a, b) = tokio::join!(
            client.request::<methods::StateRead>(StateReadParams {}),
            client.request::<methods::StateRead>(StateReadParams {}),
        );
        assert_eq!(a.unwrap(), b.unwrap());
        client.shutdown().await;
    }
}
