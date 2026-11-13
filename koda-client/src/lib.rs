//! The client API every Koda front end uses: start Koda, send requests,
//! and read the answers.
//!
//! [`InProcessClient`] runs Koda's host on a task in the same process. A
//! request goes out typed and comes back typed, but passes through its JSON
//! form on the way, exactly as it would over stdio or a socket; one contract
//! by construction. Codex's `app-server-client` is the model.
//!
//! [`events::EventProjector`] turns a session's engine events into the
//! protocol's notifications and server requests; `koda exec --json` prints
//! them. Session requests and turns through the host arrive with the
//! changes that first use them.

pub mod events;
mod host;

use std::fmt;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use koda_core::config::KodaConfig;
use koda_protocol::initialize::{ClientInfo, InitializeParams, InitializeResponse};
use koda_protocol::rpc::{ErrorObject, RequestId};
use koda_protocol::{Method, methods};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

pub use koda_protocol as protocol;

use host::HostCommand;

/// How long [`InProcessClient::shutdown`] waits before stopping the host by force.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// What [`InProcessClient::start`] needs.
pub struct StartArgs {
    /// What new sessions start with.
    pub config: KodaConfig,
    /// Who the client is, sent in `initialize`.
    pub client_info: ClientInfo,
}

/// A client with Koda's host running in this process.
pub struct InProcessClient {
    commands: mpsc::Sender<HostCommand>,
    host: JoinHandle<()>,
    next_id: AtomicI64,
    server: InitializeResponse,
}

impl InProcessClient {
    /// Start the host and run the `initialize` handshake.
    pub async fn start(args: StartArgs) -> Result<Self, RequestError> {
        let (commands, host) = host::spawn(args.config);
        let mut client = Self {
            commands,
            host,
            next_id: AtomicI64::new(1),
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

    /// Stop the host, cancelling requests still running. Waits up to five
    /// seconds, then stops it by force.
    pub async fn shutdown(self) {
        let Self {
            commands, mut host, ..
        } = self;
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

    fn args(config: KodaConfig) -> StartArgs {
        StartArgs {
            config,
            client_info: ClientInfo {
                name: "koda-client-test".to_string(),
                version: "0".to_string(),
            },
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_runs_the_handshake() {
        let config = KodaConfig::default_for_testing(ProviderType::LMStudio);
        let client = InProcessClient::start(args(config)).await.unwrap();
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
        let client = InProcessClient::start(args(config)).await.unwrap();

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
        let client = InProcessClient::start(args(config)).await.unwrap();
        let (a, b) = tokio::join!(
            client.request::<methods::StateRead>(StateReadParams {}),
            client.request::<methods::StateRead>(StateReadParams {}),
        );
        assert_eq!(a.unwrap(), b.unwrap());
        client.shutdown().await;
    }
}
