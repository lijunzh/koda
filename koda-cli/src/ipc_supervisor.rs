//! Supervisor IPC server — handles fetch requests from sandboxed workers.
//!
//! ## Lifecycle
//!
//! Call [`IpcSupervisor::bind`] once when the supervisor starts.  It returns
//! an [`IpcSupervisor`] handle and sets `KODA_SUPERVISOR_SOCKET` so that any
//! worker process spawned afterwards automatically finds the socket path.
//!
//! The supervisor task runs until the [`IpcSupervisor`] is dropped (shutdown
//! is signalled via an internal `CancellationToken`).
//!
//! ## Security
//!
//! Every fetch request that arrives over IPC is validated with
//! [`koda_core::tools::web_fetch::is_safe_url`] before any network call is
//! made.  The worker process is never trusted to have already validated the
//! URL — defence in depth requires the supervisor to re-check.
//!
//! ## Example
//!
//! ```rust,ignore
//! let sup = IpcSupervisor::bind().await?;
//! // spawn worker process — it inherits KODA_SUPERVISOR_SOCKET
//! let child = tokio::process::Command::new("koda")
//!     .arg("__worker")
//!     .env(koda_ipc::client::SUPERVISOR_SOCKET_ENV, sup.socket_path())
//!     .spawn()?;
//! // ... the supervisor task runs in the background until `sup` is dropped
//! drop(sup); // shuts down the listener
//! ```

use anyhow::{Context, Result};
use koda_core::providers::LlmProvider;
use koda_ipc::message::{
    FetchResponse, HandshakeAck, HandshakeHello, IpcRequest, IpcRequestBody, IpcResponse,
    IpcResponseBody, PROTOCOL_VERSION,
};
use koda_ipc::transport::{recv, send};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::BufReader;
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Configuration for a sandboxed headless worker subprocess.
///
/// Passed to [`IpcSupervisor::spawn_worker`].
pub struct WorkerConfig<'a> {
    /// The user's prompt.
    pub prompt: &'a str,
    /// Pre-created session ID for conversation history.
    pub session_id: &'a str,
    /// Canonical project root path.
    pub project_root: &'a Path,
    /// Full path to the shared koda.db file.
    pub db_path: &'a Path,
    /// Agent name (matches a JSON file in agents/).
    pub agent: &'a str,
    /// Trust mode: `"safe"` or `"auto"`.
    pub mode: &'a str,
    /// Output format: `"text"` or `"json"`.
    pub output_format: &'a str,
}

/// Maximum response body size the supervisor will return to the worker.
const MAX_BODY_CHARS: usize = 128_000;

/// A running IPC supervisor.
///
/// The background listener task shuts down when this value is dropped.
pub struct IpcSupervisor {
    socket_path: PathBuf,
    _cancel: CancellationToken,
    /// LLM provider used to service `LlmChat` requests from workers.
    /// `None` in pure-test mode where no LLM calls are expected.
    _provider: Option<Arc<dyn LlmProvider>>,
}

impl IpcSupervisor {
    /// Bind a new Unix domain socket without an LLM provider (server/test mode).
    ///
    /// Sets [`koda_ipc::client::SUPERVISOR_SOCKET_ENV`] in the current
    /// process environment so that child processes inherit the path.
    /// For headless worker spawning use [`IpcSupervisor::bind_with_llm`].
    pub async fn bind() -> Result<Self> {
        let socket_path = Self::generate_socket_path();
        let sup = Self::bind_at_with_llm(&socket_path, None).await?;
        // SAFETY: single-threaded at startup; no concurrent env access.
        unsafe {
            std::env::set_var(
                koda_ipc::client::SUPERVISOR_SOCKET_ENV,
                socket_path.as_os_str(),
            );
        }
        Ok(sup)
    }

    /// Bind at a specific path without touching the environment — used by tests
    /// to avoid global env var races.
    #[allow(dead_code)] // called from tests only
    pub async fn bind_at(socket_path: &Path) -> Result<Self> {
        Self::bind_at_with_llm(socket_path, None).await
    }

    /// Bind at a specific path with an LLM provider for `LlmChat` proxying.
    pub async fn bind_with_llm(provider: Box<dyn LlmProvider>) -> Result<Self> {
        let socket_path = Self::generate_socket_path();
        let sup = Self::bind_at_with_llm(&socket_path, Some(Arc::from(provider))).await?;
        // SAFETY: single-threaded at startup; no concurrent env access.
        unsafe {
            std::env::set_var(
                koda_ipc::client::SUPERVISOR_SOCKET_ENV,
                socket_path.as_os_str(),
            );
        }
        Ok(sup)
    }

    async fn bind_at_with_llm(
        socket_path: &Path,
        provider: Option<Arc<dyn LlmProvider>>,
    ) -> Result<Self> {
        // Remove a leftover socket file from a previous crashed run.
        if socket_path.exists() {
            std::fs::remove_file(socket_path).ok();
        }

        let listener = UnixListener::bind(socket_path)
            .with_context(|| format!("bind IPC socket {}", socket_path.display()))?;

        info!(path = %socket_path.display(), "IPC supervisor listening");

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let path_clone = socket_path.to_path_buf();
        let provider_clone = provider.clone();

        tokio::spawn(async move {
            run_supervisor_loop(listener, cancel_clone, path_clone, provider_clone).await;
        });

        Ok(Self {
            socket_path: socket_path.to_path_buf(),
            _cancel: cancel,
            _provider: provider,
        })
    }

    /// The path of the bound Unix socket.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    fn generate_socket_path() -> PathBuf {
        let id = Uuid::new_v4().simple().to_string();
        std::env::temp_dir().join(format!("koda-sup-{id}.sock"))
    }

    /// Spawn a network-isolated `koda __worker` subprocess.
    ///
    /// The worker reads its configuration from env vars; the supervisor passes
    /// all required vars plus `KODA_SUPERVISOR_SOCKET` so the worker's
    /// `create_provider` returns an `IpcLlmProvider` automatically.
    ///
    /// ## Network isolation
    ///
    /// - **macOS**: `sandbox-exec` profile denying all network except unix sockets
    /// - **Linux**: `unshare --net` (new network namespace, loopback only)
    ///
    /// Unix sockets are filesystem-based and survive both sandboxes, so IPC
    /// back to the supervisor works normally.
    pub async fn spawn_worker(&self, cfg: WorkerConfig<'_>) -> Result<tokio::process::Child> {
        let exe = std::env::current_exe().context("resolve current exe")?;

        let mut cmd = build_isolated_command(&exe);
        cmd.arg("__worker");

        cmd.env("KODA_WORKER_PROMPT", cfg.prompt)
            .env("KODA_WORKER_SESSION_ID", cfg.session_id)
            .env("KODA_WORKER_PROJECT_ROOT", cfg.project_root)
            .env("KODA_WORKER_DB_PATH", cfg.db_path)
            .env("KODA_WORKER_AGENT", cfg.agent)
            .env("KODA_WORKER_MODE", cfg.mode)
            .env("KODA_WORKER_OUTPUT_FORMAT", cfg.output_format)
            .env(koda_ipc::client::SUPERVISOR_SOCKET_ENV, &self.socket_path);

        let child = cmd.spawn().context("spawn koda __worker")?;
        Ok(child)
    }
}

impl Drop for IpcSupervisor {
    fn drop(&mut self) {
        // The CancellationToken is cancelled when `_cancel` is dropped,
        // which wakes the accept loop and causes it to return.
        debug!(path = %self.socket_path.display(), "IPC supervisor shutting down");
        // Best-effort cleanup of the socket file.
        std::fs::remove_file(&self.socket_path).ok();
        // Unset the env var so subsequent non-worker processes don't try
        // to use a dead socket.
        // SAFETY: called on drop; no concurrent env access expected.
        unsafe {
            std::env::remove_var(koda_ipc::client::SUPERVISOR_SOCKET_ENV);
        }
    }
}

// ── Supervisor accept loop ─────────────────────────────────────────────────

async fn run_supervisor_loop(
    listener: UnixListener,
    cancel: CancellationToken,
    socket_path: PathBuf,
    provider: Option<Arc<dyn LlmProvider>>,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                debug!(path = %socket_path.display(), "IPC supervisor: cancel received");
                break;
            }
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _addr)) => {
                        let p = provider.clone();
                        tokio::spawn(handle_connection(stream, p));
                    }
                    Err(e) => {
                        warn!(error = %e, "IPC supervisor: accept error");
                    }
                }
            }
        }
    }
}

// ── Per-connection handler ─────────────────────────────────────────────────

async fn handle_connection(stream: UnixStream, provider: Option<Arc<dyn LlmProvider>>) {
    if let Err(e) = serve_connection(stream, provider).await {
        warn!(error = %e, "IPC supervisor: connection error");
    }
}

async fn serve_connection(
    stream: UnixStream,
    provider: Option<Arc<dyn LlmProvider>>,
) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // ── Handshake ──────────────────────────────────────────────────────────
    let hello: HandshakeHello = recv(&mut reader).await.context("recv handshake")?;
    let accepted = hello.protocol_version == PROTOCOL_VERSION;
    let ack = HandshakeAck {
        protocol_version: PROTOCOL_VERSION,
        accepted,
        message: if accepted {
            "ok".into()
        } else {
            format!(
                "protocol version mismatch: supervisor={PROTOCOL_VERSION}, \
                 worker={}. Upgrade koda.",
                hello.protocol_version
            )
        },
    };
    send(&mut write_half, &ack)
        .await
        .context("send handshake ack")?;

    if !accepted {
        return Ok(());
    }

    debug!(
        koda_version = hello.koda_version,
        "IPC supervisor: worker connected"
    );

    // ── Request loop ───────────────────────────────────────────────────────
    loop {
        let req: IpcRequest = match recv(&mut reader).await {
            Ok(r) => r,
            Err(e) => {
                // Connection closed normally — not an error worth logging.
                debug!(error = %e, "IPC supervisor: worker disconnected");
                break;
            }
        };

        let resp_body = handle_request(&req.body, provider.as_deref()).await;
        let resp = IpcResponse {
            req_id: req.req_id.clone(),
            body: resp_body,
        };
        send(&mut write_half, &resp)
            .await
            .context("send response")?;

        // Shutdown request ends the loop cleanly.
        if matches!(req.body, IpcRequestBody::Shutdown) {
            debug!("IPC supervisor: worker requested shutdown");
            break;
        }
    }

    Ok(())
}

async fn handle_request(
    body: &IpcRequestBody,
    provider: Option<&dyn LlmProvider>,
) -> IpcResponseBody {
    match body {
        IpcRequestBody::Fetch(req) => fetch_via_supervisor(req).await,
        IpcRequestBody::LlmChat(req) => llm_chat_via_supervisor(req, provider).await,
        IpcRequestBody::Shutdown => IpcResponseBody::ShutdownAck,
    }
}

// ── Supervisor-side LLM chat ───────────────────────────────────────────────

async fn llm_chat_via_supervisor(
    req: &koda_ipc::llm::IpcLlmRequest,
    provider: Option<&dyn LlmProvider>,
) -> IpcResponseBody {
    let Some(prov) = provider else {
        return IpcResponseBody::Error {
            message: "supervisor has no LLM provider configured".into(),
        };
    };

    use koda_core::config::ModelSettings;
    use koda_core::providers::{ChatMessage, ImageData, ToolDefinition};
    use koda_ipc::llm::{IpcImageData, IpcToolCall};

    // Convert IPC types → koda-core types.
    let messages: Vec<ChatMessage> = req
        .messages
        .iter()
        .map(|m| ChatMessage {
            role: m.role.clone(),
            content: m.content.clone(),
            tool_calls: m
                .tool_calls
                .as_ref()
                .map(|tc| tc.iter().map(from_ipc_tool_call).collect()),
            tool_call_id: m.tool_call_id.clone(),
            images: m.images.as_ref().map(|imgs| {
                imgs.iter()
                    .map(|i: &IpcImageData| ImageData {
                        media_type: i.media_type.clone(),
                        base64: i.base64.clone(),
                    })
                    .collect()
            }),
        })
        .collect();

    let tools: Vec<ToolDefinition> = req
        .tools
        .iter()
        .map(|t| ToolDefinition {
            name: t.name.clone(),
            description: t.description.clone(),
            parameters: t.parameters.clone(),
        })
        .collect();

    let settings = ModelSettings {
        model: req.settings.model.clone(),
        max_tokens: req.settings.max_tokens,
        temperature: req.settings.temperature,
        thinking_budget: req.settings.thinking_budget,
        reasoning_effort: req.settings.reasoning_effort.clone(),
        max_context_tokens: req.settings.max_context_tokens,
    };

    match prov.chat(&messages, &tools, &settings).await {
        Ok(resp) => IpcResponseBody::LlmChatOk(koda_ipc::llm::IpcLlmResponse {
            content: resp.content,
            tool_calls: resp
                .tool_calls
                .into_iter()
                .map(|tc| IpcToolCall {
                    id: tc.id,
                    function_name: tc.function_name,
                    arguments: tc.arguments,
                    thought_signature: tc.thought_signature,
                })
                .collect(),
            usage: koda_ipc::llm::IpcTokenUsage {
                prompt_tokens: resp.usage.prompt_tokens,
                completion_tokens: resp.usage.completion_tokens,
                cache_read_tokens: resp.usage.cache_read_tokens,
                cache_creation_tokens: resp.usage.cache_creation_tokens,
                thinking_tokens: resp.usage.thinking_tokens,
                stop_reason: resp.usage.stop_reason,
            },
        }),
        Err(e) => IpcResponseBody::Error {
            message: format!("LLM chat failed: {e}"),
        },
    }
}

fn from_ipc_tool_call(tc: &koda_ipc::llm::IpcToolCall) -> koda_core::providers::ToolCall {
    koda_core::providers::ToolCall {
        id: tc.id.clone(),
        function_name: tc.function_name.clone(),
        arguments: tc.arguments.clone(),
        thought_signature: tc.thought_signature.clone(),
    }
}

// ── Supervisor-side fetch ──────────────────────────────────────────────────

async fn fetch_via_supervisor(req: &koda_ipc::message::FetchRequest) -> IpcResponseBody {
    // Re-validate the URL — never trust the worker's own validation.
    if !req.url.starts_with("http://") && !req.url.starts_with("https://") {
        return IpcResponseBody::Error {
            message: "URL must start with http:// or https://".into(),
        };
    }

    if !koda_core::tools::web_fetch::is_safe_url(&req.url) {
        return IpcResponseBody::Error {
            message: format!(
                "URL blocked by supervisor: requests to internal/private \
                 networks are not allowed ({})",
                req.url
            ),
        };
    }

    let cap = req
        .max_body_chars
        .unwrap_or(MAX_BODY_CHARS)
        .min(MAX_BODY_CHARS);

    match do_fetch(&req.url, cap).await {
        Ok((body, status)) => IpcResponseBody::FetchOk(FetchResponse { body, status }),
        Err(e) => IpcResponseBody::Error {
            message: format!("fetch failed: {e}"),
        },
    }
}

async fn do_fetch(url: &str, max_body_chars: usize) -> Result<(String, u16)> {
    // Use the JSON args format that web_fetch::web_fetch() already understands
    // so we piggyback all its HTML-stripping, content-type detection, and
    // truncation logic for free.
    let args = serde_json::json!({ "url": url, "raw": false });
    // Temporarily unset the supervisor socket env var so we don't recurse!
    let _guard = SocketEnvGuard::take();
    let body = koda_core::tools::web_fetch::web_fetch(&args, max_body_chars).await?;
    let status = 200u16; // web_fetch raises on non-2xx; success here means 200-ish
    Ok((body, status))
}

/// Build a [`tokio::process::Command`] that runs `exe` inside a
/// network-isolation wrapper appropriate for the current OS.
///
/// - **macOS**: `sandbox-exec -p <profile> <exe>` — denies all network
///   syscalls except outbound Unix domain socket connections.
/// - **Linux**: `unshare --net <exe>` if `unshare` is available; falls back
///   to running `<exe>` unsandboxed with a logged warning.
///
/// In both cases Unix sockets remain functional because they are
/// filesystem objects and bypass the network stack entirely.
fn build_isolated_command(exe: &std::path::Path) -> tokio::process::Command {
    #[cfg(target_os = "macos")]
    {
        // Deny all network, allow outbound Unix domain sockets.
        // The `(allow network-outbound (local unix-socket))` rule lets the
        // worker reach the supervisor's socket.
        let profile = "(version 1)\
            (deny default)\
            (allow file*)\
            (allow process*)\
            (allow signal)\
            (allow mach*)\
            (allow sysctl*)\
            (allow network-outbound (local unix-socket))";
        let mut cmd = tokio::process::Command::new("sandbox-exec");
        cmd.arg("-p").arg(profile).arg(exe);
        cmd
    }
    #[cfg(target_os = "linux")]
    {
        // `unshare --net` creates a new network namespace with no interfaces.
        // TCP/UDP connections fail; Unix sockets work (they're FS-based).
        if is_command_available("unshare") {
            let mut cmd = tokio::process::Command::new("unshare");
            cmd.arg("--net").arg(exe);
            cmd
        } else {
            tracing::warn!(
                "`unshare` not found — spawning worker without network isolation. \
                 Install `util-linux` to enable isolation."
            );
            tokio::process::Command::new(exe)
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        // Fallback for other Unix (e.g. FreeBSD) — no network isolation.
        tracing::warn!("Network isolation not implemented for this platform.");
        tokio::process::Command::new(exe)
    }
}

#[cfg(target_os = "linux")]
fn is_command_available(cmd: &str) -> bool {
    std::process::Command::new("which")
        .arg(cmd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// RAII guard: temporarily removes `KODA_SUPERVISOR_SOCKET` from the
/// environment so the supervisor's own `web_fetch` call doesn't recurse into
/// IPC, then restores it on drop.
struct SocketEnvGuard {
    saved: Option<String>,
}

impl SocketEnvGuard {
    fn take() -> Self {
        let saved = std::env::var(koda_ipc::client::SUPERVISOR_SOCKET_ENV).ok();
        // SAFETY: single-threaded fetch handler; no concurrent env access.
        unsafe {
            std::env::remove_var(koda_ipc::client::SUPERVISOR_SOCKET_ENV);
        }
        Self { saved }
    }
}

impl Drop for SocketEnvGuard {
    fn drop(&mut self) {
        if let Some(ref path) = self.saved {
            // SAFETY: restoring on drop; same single-threaded context.
            unsafe {
                std::env::set_var(koda_ipc::client::SUPERVISOR_SOCKET_ENV, path);
            }
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// `bind_at` creates a socket file at the given path.
    #[tokio::test]
    async fn bind_at_creates_socket() {
        let path = std::env::temp_dir().join("koda-ipc-test-a.sock");
        let sup = IpcSupervisor::bind_at(&path).await.unwrap();
        assert!(
            sup.socket_path().exists(),
            "socket file must exist after bind"
        );
    }

    /// Dropping the supervisor removes the socket file.
    #[tokio::test]
    async fn drop_removes_socket_file() {
        let path = std::env::temp_dir().join("koda-ipc-test-b.sock");
        let sup = IpcSupervisor::bind_at(&path).await.unwrap();
        let recorded = sup.socket_path().to_path_buf();
        drop(sup);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!recorded.exists(), "socket file must be removed on drop");
    }

    /// Two `bind_at` calls at different paths are independent.
    #[tokio::test]
    async fn bind_at_two_different_paths_independent() {
        let p1 = std::env::temp_dir().join("koda-ipc-test-c1.sock");
        let p2 = std::env::temp_dir().join("koda-ipc-test-c2.sock");
        let s1 = IpcSupervisor::bind_at(&p1).await.unwrap();
        let s2 = IpcSupervisor::bind_at(&p2).await.unwrap();
        assert_ne!(s1.socket_path(), s2.socket_path());
        assert!(s1.socket_path().exists());
        assert!(s2.socket_path().exists());
    }

    /// `generate_socket_path` produces a path in the temp dir.
    #[test]
    fn generate_path_in_tmp() {
        let path = IpcSupervisor::generate_socket_path();
        assert!(path.starts_with(std::env::temp_dir()));
        assert!(
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("koda-sup-")
        );
    }

    /// `SocketEnvGuard` restores the original value on drop.
    /// Uses the SUPERVISOR_SOCKET_ENV key, so this test must be run in
    /// isolation from other tests that also touch that var.  Because Rust's
    /// test harness runs unit tests in the same process with multiple threads
    /// there is an inherent TOCTOU risk with any global env var test.  Accept
    /// the risk here; the guard logic is a trivial save/restore one-liner.
    #[test]
    fn socket_env_guard_saves_and_restores() {
        use koda_ipc::client::SUPERVISOR_SOCKET_ENV;
        let sentinel = "/tmp/guard-test-sentinel.sock";
        // SAFETY: test-only; accept env race risk (documented above).
        unsafe {
            std::env::set_var(SUPERVISOR_SOCKET_ENV, sentinel);
        }
        {
            let _guard = SocketEnvGuard::take();
            assert!(
                std::env::var(SUPERVISOR_SOCKET_ENV).is_err(),
                "guard must remove the var while live"
            );
        } // guard dropped here
        assert_eq!(
            std::env::var(SUPERVISOR_SOCKET_ENV).unwrap(),
            sentinel,
            "guard must restore original value on drop"
        );
        unsafe {
            std::env::remove_var(SUPERVISOR_SOCKET_ENV);
        }
    }
}
