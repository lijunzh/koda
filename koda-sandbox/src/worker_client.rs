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

// ── Binary discovery ──────────────────────────────────────────────────────

/// Locate the `koda-fs-worker` binary.
///
/// Resolution order:
/// 1. `KODA_FS_WORKER_BIN` env var (explicit override, used in tests).
/// 2. `CARGO_BIN_EXE_koda-fs-worker` env var (set by Cargo for
///    integration tests — NOT available in `#[cfg(test)]` unit tests).
/// 3. Sibling of the current executable (production install).
fn worker_binary() -> Result<PathBuf> {
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
        let socket_path = unique_socket_path();
        let bin = worker_binary()?;

        let mut child = Command::new(&bin)
            .arg("--socket")
            .arg(&socket_path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
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
