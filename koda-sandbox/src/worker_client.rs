//! Host-side client for the `koda-fs-worker` process (Phase 2c of #934).
//!
//! [`WorkerClient`] owns a spawned `koda-fs-worker` binary and
//! communicates with it over a Unix domain socket using the
//! length-prefixed JSON protocol defined in [`crate::ipc`].
//!
//! ## Lifecycle
//!
//! ```text
//! WorkerClient::spawn()
//!   │
//!   ├─ generate unique socket path (/tmp/koda-fs-worker-<pid>-<n>.sock)
//!   ├─ spawn koda-fs-worker --socket <path>
//!   ├─ read "ready\n" from child stdout  (≤ 5 s)
//!   ├─ connect UnixStream to path
//!   └─ ready to serve requests
//!
//! client.request(&req) → FsResult<Response>
//!   ├─ write_message (length-prefixed JSON)
//!   └─ read_message  (length-prefixed JSON)
//!
//! Drop(WorkerClient)
//!   ├─ start_kill() child process
//!   └─ remove socket file
//! ```
//!
//! ## Why Unix sockets over stdin/stdout for production?
//!
//! 1. **Bidirectional framing** — no fighting over stdout between the
//!    readiness signal and IPC messages.
//! 2. **Per-slot pool** (Phase 4) — the pool can hold
//!    `WorkerClient`s and hand them to callers without restarting the
//!    binary.
//! 3. **Buffer size** — the kernel socket buffer doesn't add a pipe
//!    capacity constraint.
//!
//! ## Binary discovery
//!
//! Tests find the binary via `CARGO_BIN_EXE_koda-fs-worker` (set by
//! Cargo). In production the binary is expected to live next to the
//! `koda` executable (same installation directory).

use crate::fs::FsError;
use crate::ipc::{Request, Response, read_message, write_message};
use crate::policy::SandboxPolicy;
use crate::proxy::{ProxyHandle, ca_bundle_for_policy, proxy_env_vars};
use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, BufReader, ReadHalf, WriteHalf};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tracing::debug;

// ── Socket-path generation ────────────────────────────────────────────────

static SLOT_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_socket_path() -> PathBuf {
    let n = SLOT_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("koda-fs-worker-{pid}-{n}.sock"))
}

// ── Binary discovery ────────────────────────────────────────────────

/// Test-only override for the worker binary path. Set via
/// [`set_worker_binary_for_tests`] from a test setup helper; takes
/// precedence over env-var lookup in `worker_binary` (private).
///
/// **#1109 F1**: replaces `unsafe { std::env::set_var }` in test
/// helpers (UB in Rust 2024 if any other thread reads env
/// concurrently). [`OnceLock`](std::sync::OnceLock) gives us "set
/// once at process startup" semantics without locks or unsafe.
static WORKER_BIN_OVERRIDE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Set the worker binary path used internally by `worker_binary`.
/// Idempotent: after the first successful call, subsequent calls are
/// silently ignored (matches the "set once per process" semantics
/// tests need).
///
/// Intended for test setup helpers. Production code uses env-var or
/// sibling discovery.
pub fn set_worker_binary_for_tests(path: PathBuf) {
    let _ = WORKER_BIN_OVERRIDE.set(path);
}

/// Locate the `koda-fs-worker` binary.
///
/// Resolution order:
/// 1. [`set_worker_binary_for_tests`] override (test-only).
/// 2. `KODA_FS_WORKER_BIN` env var (explicit override; legacy entry
///    point, preserved for backward compatibility).
/// 3. `CARGO_BIN_EXE_koda-fs-worker` env var (set by Cargo for
///    integration tests — NOT available in `#[cfg(test)]` unit tests).
/// 4. Sibling of the current executable (production install).
fn worker_binary() -> Result<PathBuf> {
    if let Some(p) = WORKER_BIN_OVERRIDE.get() {
        return Ok(p.clone());
    }
    if let Ok(p) = std::env::var("KODA_FS_WORKER_BIN") {
        return Ok(PathBuf::from(p));
    }
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_koda-fs-worker") {
        return Ok(PathBuf::from(p));
    }
    let mut p = std::env::current_exe().context("can't locate koda executable")?;
    p.set_file_name("koda-fs-worker");
    if p.exists() {
        return Ok(p);
    }
    bail!(
        "koda-fs-worker not found next to {}; set KODA_FS_WORKER_BIN to override",
        p.display()
    )
}

// ── WorkerClient ─────────────────────────────────────────────────────────

/// Owns a live `koda-fs-worker` subprocess and the Unix-socket
/// connection to it.
///
/// Only one request can be in flight at a time — callers must serialize
/// access. [`crate::fs::SandboxedFileSystem`] does this via an
/// `Arc<tokio::sync::Mutex<WorkerClient>>`.
pub struct WorkerClient {
    child: Child,
    socket_path: PathBuf,
    reader: BufReader<ReadHalf<UnixStream>>,
    writer: WriteHalf<UnixStream>,
}

impl WorkerClient {
    /// Spawn a fresh worker and wait for it to signal readiness.
    ///
    /// Blocks the current async task for up to 5 seconds waiting for
    /// the worker to bind its socket and write "ready\n" to stdout.
    pub async fn spawn() -> Result<Self> {
        Self::spawn_inner(None, None).await
    }

    /// Spawn a worker with write-policy enforcement.
    ///
    /// Passes `--root <writable_root>` and the serialized `policy`
    /// (via `KODA_FS_WORKER_POLICY`) to the binary. Every `Write` and
    /// `Edit` request will be validated against the root and policy
    /// before touching the filesystem.
    ///
    /// Use this for production slots; use [`spawn`](Self::spawn) only
    /// for tests and the `--no-sandbox` escape hatch.
    pub async fn spawn_with_policy(writable_root: PathBuf, policy: &SandboxPolicy) -> Result<Self> {
        Self::spawn_inner(Some((writable_root, policy)), None).await
    }

    /// Spawn a worker with write-policy enforcement *and* proxy env injection.
    ///
    /// Same as [`spawn_with_policy`](Self::spawn_with_policy), plus pipes the
    /// proxy env-var bouquet (see [`crate::proxy::proxy_env_vars`]) into the
    /// worker subprocess so anything *it* execs (e.g. via `Bash` tool) inherits
    /// `HTTPS_PROXY` and friends.
    ///
    /// `proxy` may be `None` — then this is exactly equivalent to
    /// [`spawn_with_policy`](Self::spawn_with_policy). The CA-bundle path is sourced from
    /// `policy.net.mitm.ca_bundle` (if any).
    ///
    /// ## Fail-open caller pattern
    ///
    /// If `proxy` is `None` because the user's external proxy failed to
    /// start, callers should `warn!` and proceed — the worker still works,
    /// just without egress restrictions. See [`crate::proxy::ExternalProxy::spawn`]
    /// docs for the full contract.
    pub async fn spawn_with_policy_and_proxy(
        writable_root: PathBuf,
        policy: &SandboxPolicy,
        proxy: Option<&ProxyHandle>,
    ) -> Result<Self> {
        let env = proxy.map(|p| {
            // CA bundle preference: explicit `ProxyHandle::ca_bundle()` (3b
            // built-in proxy will set this) wins over the policy.
            let ca = p.ca_bundle().or_else(|| ca_bundle_for_policy(&policy.net));
            proxy_env_vars(p.port, ca)
        });
        Self::spawn_inner(Some((writable_root, policy)), env).await
    }

    /// Shared spawn logic. `policy_args` being `None` → no `--root`/policy env.
    /// `extra_env`, when `Some`, is appended to the worker's process env
    /// (used by Phase 3a to inject the proxy bouquet).
    async fn spawn_inner(
        policy_args: Option<(PathBuf, &SandboxPolicy)>,
        extra_env: Option<Vec<(String, String)>>,
    ) -> Result<Self> {
        let socket_path = unique_socket_path();
        let bin = worker_binary()?;

        let mut cmd = Command::new(&bin);
        cmd.arg("--socket")
            .arg(&socket_path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit());

        if let Some((ref root, policy)) = policy_args {
            cmd.arg("--root").arg(root);
            let policy_json =
                serde_json::to_string(policy).context("serialize SandboxPolicy for worker env")?;
            cmd.env("KODA_FS_WORKER_POLICY", policy_json);
        }

        if let Some(env) = extra_env {
            cmd.envs(env);
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawn {}", bin.display()))?;

        // Wait for the worker to bind and signal "ready\n".
        let stdout = child.stdout.take().expect("stdout piped");
        let mut lines = BufReader::new(stdout).lines();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(line) = lines.next_line().await? {
                if line.trim() == "ready" {
                    return Ok::<_, std::io::Error>(());
                }
            }
            Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "worker exited before signalling ready",
            ))
        })
        .await
        .context("worker readiness timeout (5 s)")?
        .context("reading worker stdout")?;
        drop(lines); // release stdout handle

        // Connect to the Unix socket the worker just bound.
        let stream = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            UnixStream::connect(&socket_path),
        )
        .await
        .context("Unix socket connect timeout (2 s)")?
        .context("UnixStream::connect")?;

        let (r, writer) = tokio::io::split(stream);
        let reader = BufReader::new(r);

        debug!("worker_client: connected to {}", socket_path.display());

        Ok(Self {
            child,
            socket_path,
            reader,
            writer,
        })
    }

    /// Path of the Unix socket this client is connected to.
    ///
    /// Exposed for tests that need to verify socket cleanup on drop.
    pub fn socket_path(&self) -> &std::path::Path {
        &self.socket_path
    }

    /// Send one request and receive one response.
    ///
    /// Callers must not pipeline — wait for the response before
    /// sending the next request. `SandboxedFileSystem` enforces
    /// this via the mutex.
    pub async fn request(&mut self, req: &Request) -> Result<Response, FsError> {
        write_message(&mut self.writer, req)
            .await
            .map_err(|e| FsError::Transport {
                message: format!("write: {e}"),
            })?;

        let resp: Response = read_message(&mut self.reader)
            .await
            .map_err(|e| FsError::Transport {
                message: format!("read: {e}"),
            })?
            .ok_or_else(|| FsError::Transport {
                message: "worker closed connection unexpectedly".into(),
            })?;

        Ok(resp)
    }
}

impl Drop for WorkerClient {
    fn drop(&mut self) {
        // Best-effort kill — if it fails the OS will reap it anyway
        // when the child handle is dropped.
        let _ = self.child.start_kill();
        let _ = std::fs::remove_file(&self.socket_path);
    }
}
