//! Koda's host in the caller's process: [`launch`] starts it, a
//! [`RequestHandle`] sends it requests, and its notifications and server
//! requests arrive on [`Launched::events`]. `koda_client::InProcessClient`
//! wraps it for a front end, as Codex's `app-server-client` wraps
//! `codex_app_server::in_process`; [`crate::stdio::serve`] puts it on a
//! pipe.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use koda_config::ConfigFile;
use koda_core::config::KodaConfig;
use koda_protocol::Method;
use koda_protocol::rpc::{ErrorObject, RequestId};
use koda_state::db::Database;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::events::Outgoing;
use crate::host::{self, HostArgs, HostCommand};
use crate::{config_file, settings};

/// How long [`stop_host`] waits before stopping the host by force.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Sends requests to the host. Cheap to clone, so a task can send one
/// while another reads events.
#[derive(Clone)]
pub struct RequestHandle {
    pub(crate) commands: mpsc::Sender<HostCommand>,
    pub(crate) next_id: Arc<AtomicI64>,
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

    /// Answer the server request `id`: its response as JSON, or an error,
    /// which Koda reads as no.
    pub async fn respond(
        &self,
        id: RequestId,
        result: Result<serde_json::Value, ErrorObject>,
    ) -> Result<(), RequestError> {
        self.commands
            .send(HostCommand::Response { id, result })
            .await
            .map_err(|_| RequestError::Closed { method: "response" })
    }

    /// Stop the host `host` this handle sends to, stopping running turns
    /// and cancelling requests still running. Waits up to five seconds,
    /// then stops it by force.
    pub async fn shutdown(&self, host: JoinHandle<()>) {
        stop_host(&self.commands, host).await;
    }
}

/// Where a host keeps its state and works.
pub struct HostStart {
    pub config_file: ConfigFile,
    pub project_root: PathBuf,
    pub home: PathBuf,
}

/// A running host, not yet initialized: the way in, its task, and its
/// queue of notifications and server requests.
pub struct Launched {
    pub requests: RequestHandle,
    pub host: JoinHandle<()>,
    pub events: mpsc::UnboundedReceiver<Outgoing>,
}

/// Open the database, read the config and start a host. Its client sends
/// `initialize`: the in-process client itself, a `koda serve` client over
/// the wire.
pub async fn launch(
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
    /// The shared server couldn't be reached (`koda serve`'s socket).
    Connect(String),
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
            Self::Connect(error) => write!(f, "couldn't reach koda serve: {error}"),
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
            Self::Database(_) | Self::Agent(_) | Self::Connect(_) => None,
        }
    }
}
