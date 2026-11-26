//! The client API every Koda front end uses: start Koda, send requests,
//! and read the answers.
//!
//! [`InProcessClient`] runs Koda's host, [`koda_server`], on a task in the
//! same process. A
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
//! pipe, `koda serve --stdio`, as `codex app-server` serves stdio. It and
//! the other host modules re-exported here live in `koda-server`, as Codex
//! keeps `app-server` apart from `app-server-client`.
//!
//! The host builds every session's agent; a front end never hands it one.
//!
//! **Versioning:** published so that `cargo install koda-cli` builds. It
//! follows koda-cli's version, and its Rust API makes no semver promise of
//! its own: a 0.7.x release may change it. The Koda crates pin each other
//! exactly, so every koda-cli builds against the crates it shipped with.

use std::path::PathBuf;

use koda_core::config::KodaConfig;
use koda_protocol::initialize::{ClientInfo, InitializeParams, InitializeResponse};
use koda_protocol::rpc::{ErrorObject, RequestId};
use koda_protocol::{Method, methods};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub use koda_config::ConfigFile;
pub use koda_protocol as protocol;
pub use koda_server::in_process::{RequestError, RequestHandle};
#[cfg(unix)]
pub use koda_server::socket;
pub use koda_server::{builtin_skills, earlier_input_history, events, feedback, stdio};

/// Getting back to the shared server after losing it, as Codex's TUI does.
pub mod reconnect;

/// Whether a server's version is this client's: Koda runs one version.
pub mod version;

use events::Outgoing;
use koda_server::in_process::{HostStart, launch};

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

/// A client with Koda's host running in this process.
pub struct InProcessClient {
    requests: RequestHandle,
    host: JoinHandle<()>,
    events: mpsc::UnboundedReceiver<Outgoing>,
    server: InitializeResponse,
    /// The folder this client's sessions work in, which it names in
    /// `session/start` and lists sessions and skills for: one shared server
    /// serves every project.
    project_root: PathBuf,
}

/// The `initialize` Koda's own clients send: everything, experimental
/// methods and notifications included, as Codex's TUI and `codex exec`
/// declare `experimentalApi`. A client ships with the Koda it talks to.
fn first_party(client_info: ClientInfo) -> InitializeParams {
    InitializeParams {
        client_info,
        capabilities: Some(koda_protocol::initialize::InitializeCapabilities {
            experimental_api: true,
            opt_out_notification_methods: None,
        }),
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
        let project_root_for_client = project_root.clone();
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
            project_root: project_root_for_client,
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
            .request::<methods::Initialize>(first_party(client_info))
            .await?;
        Ok(client)
    }

    /// Connect to the shared server (`koda serve`) at `socket` and
    /// initialize, for sessions in `project_root`, instead of starting a host
    /// in this process: the same
    /// client, its requests and events going over the socket. [`Self::shutdown`]
    /// only hangs up; the sessions stay on the server.
    #[cfg(unix)]
    pub async fn connect(
        socket: &std::path::Path,
        project_root: PathBuf,
        client_info: ClientInfo,
    ) -> Result<Self, RequestError> {
        let connected = koda_server::socket::connect(socket).await?;
        let mut client = Self {
            project_root,
            requests: connected.requests,
            host: connected.host,
            events: connected.events,
            server: InitializeResponse {
                server_info: koda_protocol::initialize::ServerInfo {
                    name: String::new(),
                    version: String::new(),
                },
                protocol_version: 0,
            },
        };
        client.server = client
            .request::<methods::Initialize>(first_party(client_info))
            .await?;
        Ok(client)
    }

    /// The folder this client's sessions work in.
    pub fn project_root(&self) -> &std::path::Path {
        &self.project_root
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
        self.requests.respond(id, Ok(result)).await
    }

    /// Refuse the server request `id`. Koda reads it as no: the approval is
    /// refused, the question gets an empty answer, the loop stops.
    pub async fn reject_server_request(
        &self,
        id: RequestId,
        error: ErrorObject,
    ) -> Result<(), RequestError> {
        self.requests.respond(id, Err(error)).await
    }

    /// Stop the host, stopping running turns and cancelling requests still
    /// running. Waits up to five
    /// seconds, then stops it by force.
    pub async fn shutdown(self) {
        self.requests.shutdown(self.host).await;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

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
                title: None,
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
