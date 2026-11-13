//! The host: the task that owns Koda's engine state and answers requests.
//!
//! It takes requests in their wire form and answers with a JSON value or a
//! JSON-RPC error, whatever the transport, so the in-process client and a
//! later `koda serve` share one contract. Codex's in-process app-server
//! does the same (`app-server/src/in_process.rs`).
//!
//! Each request is answered on its own task, so a slow one never holds up
//! the rest; shutting down cancels whatever is still running.

use std::sync::{Arc, OnceLock};

use koda_core::config::KodaConfig;
use koda_protocol::initialize::{ClientInfo, InitializeResponse, ServerInfo};
use koda_protocol::rpc::{self, ErrorObject, codes};
use koda_protocol::state::{SandboxState, StateReadResponse, TrustMode};
use koda_protocol::{ClientRequest, PROTOCOL_VERSION};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

/// A request's answer: the method's response as JSON, or an error.
pub(crate) type Answer = Result<Value, ErrorObject>;

/// What a client sends the host.
pub(crate) enum HostCommand {
    /// Answer `request` on `reply`.
    Request {
        request: rpc::Request,
        reply: oneshot::Sender<Answer>,
    },
    /// Cancel what's running and stop; say so on `done`.
    Shutdown { done: oneshot::Sender<()> },
}

/// Start a host for `config` on its own task.
pub(crate) fn spawn(config: KodaConfig) -> (mpsc::Sender<HostCommand>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(64);
    let host = Arc::new(Host {
        config,
        client: OnceLock::new(),
    });
    (tx, tokio::spawn(run(host, rx)))
}

struct Host {
    /// What new sessions start with.
    config: KodaConfig,
    /// Set by `initialize`; until then every other request is refused.
    client: OnceLock<ClientInfo>,
}

async fn run(host: Arc<Host>, mut commands: mpsc::Receiver<HostCommand>) {
    let mut running = JoinSet::new();
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(HostCommand::Request { request, reply }) => {
                    let host = Arc::clone(&host);
                    running.spawn(async move {
                        // A client that stopped waiting just drops the answer.
                        let _ = reply.send(host.answer(request).await);
                    });
                }
                Some(HostCommand::Shutdown { done }) => {
                    running.shutdown().await;
                    let _ = done.send(());
                    return;
                }
                // Every client is gone.
                None => return,
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
    async fn answer(&self, request: rpc::Request) -> Answer {
        let request = ClientRequest::try_from(request).map_err(|e| e.to_error_object())?;
        match request {
            ClientRequest::Initialize { params, .. } => {
                if self.client.set(params.client_info).is_err() {
                    return Err(ErrorObject::new(
                        codes::INVALID_REQUEST,
                        "Already initialized",
                    ));
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
            _ if self.client.get().is_none() => {
                Err(ErrorObject::new(codes::INVALID_REQUEST, "Not initialized"))
            }
            ClientRequest::StateRead { .. } => to_json(self.state().await?),
        }
    }

    async fn state(&self) -> Result<StateReadResponse, ErrorObject> {
        // The sandbox check may start a process, so it runs off the async threads.
        let report = tokio::task::spawn_blocking(koda_core::sandbox::dependency_report)
            .await
            .map_err(|e| {
                ErrorObject::new(codes::INTERNAL_ERROR, format!("sandbox check failed: {e}"))
            })?;
        let config = &self.config;
        Ok(StateReadResponse {
            model: config.model.clone(),
            provider: config.provider_type.to_string(),
            base_url: config.base_url.clone(),
            context_window: config.max_context_tokens as u64,
            mode: trust_mode(config.trust),
            sandbox: SandboxState {
                backend: report.backend.to_string(),
                available: report.available,
                reason: report.reason,
            },
        })
    }
}

fn trust_mode(mode: koda_core::trust::TrustMode) -> TrustMode {
    match mode {
        koda_core::trust::TrustMode::Plan => TrustMode::Plan,
        koda_core::trust::TrustMode::Safe => TrustMode::Safe,
        koda_core::trust::TrustMode::Auto => TrustMode::Auto,
    }
}

fn to_json<T: serde::Serialize>(response: T) -> Answer {
    serde_json::to_value(response)
        .map_err(|e| ErrorObject::new(codes::INTERNAL_ERROR, format!("response encoding: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use koda_core::config::ProviderType;
    use koda_protocol::rpc::RequestId;
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
        let (tx, _host) = spawn(KodaConfig::default_for_testing(ProviderType::LMStudio));

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
        let (tx, _host) = spawn(KodaConfig::default_for_testing(ProviderType::LMStudio));
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
        let (tx, host) = spawn(KodaConfig::default_for_testing(ProviderType::LMStudio));
        let (done, stopped) = oneshot::channel();
        tx.send(HostCommand::Shutdown { done }).await.unwrap();
        stopped.await.unwrap();
        host.await.unwrap();
        assert!(tx.is_closed());
    }
}
