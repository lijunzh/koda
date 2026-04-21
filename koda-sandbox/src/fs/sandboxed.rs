//! [`SandboxedFileSystem`] — IPC-backed [`FileSystem`] impl
//! (Phase 2c of #934).
//!
//! Forwards each [`FileSystem`] method call to a live [`WorkerClient`]
//! as a length-prefixed JSON [`Request`], receives the [`Response`],
//! and maps it back to a typed `FsResult<T>`. The worker process
//! performs the actual filesystem operation — the host process never
//! touches the disk for sandboxed tools.
//!
//! ## Usage
//!
//! ```rust,ignore
//! use koda_sandbox::fs::{FileSystem, SandboxedFileSystem};
//!
//! // Spawn a worker, wrap it in a shareable FS handle.
//! let fs = SandboxedFileSystem::spawn().await?;
//!
//! // Use it just like LocalFileSystem — same trait, different backend.
//! let bytes = fs.read(Path::new("/work/src/lib.rs"), Some(65536)).await?;
//! ```
//!
//! ## Thread safety
//!
//! `WorkerClient` is serial (one outstanding request at a time — that's
//! the IPC protocol). `SandboxedFileSystem` wraps it in a
//! `tokio::sync::Mutex` so multiple tool tasks can share one worker
//! without data races. If contention becomes a bottleneck under the
//! Phase 4 pool, each pool slot gets its own `SandboxedFileSystem` and
//! the mutex becomes uncontested.

use crate::fs::{FileSystem, FsError, FsResult, Metadata};
use crate::ipc::{ErrorCode, GrepMatch, Request, Response};
use crate::worker_client::WorkerClient;
use anyhow::Result;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

/// IPC-backed [`FileSystem`] implementation.
///
/// Cheap to clone — the clone shares the same worker connection.
#[derive(Clone)]
pub struct SandboxedFileSystem {
    client: Arc<Mutex<WorkerClient>>,
}

impl SandboxedFileSystem {
    /// Spawn a fresh worker and wrap it in a [`SandboxedFileSystem`].
    pub async fn spawn() -> Result<Self> {
        let client = WorkerClient::spawn().await?;
        Ok(Self {
            client: Arc::new(Mutex::new(client)),
        })
    }
}

// ── FileSystem impl ───────────────────────────────────────────────────────

#[async_trait]
impl FileSystem for SandboxedFileSystem {
    async fn read(&self, path: &Path, max_bytes: Option<usize>) -> FsResult<Vec<u8>> {
        let req = Request::Read {
            path: path.to_path_buf(),
            max_bytes,
        };
        match self.rpc(req).await? {
            Response::Read { content } => Ok(content),
            other => Err(unexpected_response("Read", other)),
        }
    }

    async fn write(&self, path: &Path, content: &[u8]) -> FsResult<usize> {
        let req = Request::Write {
            path: path.to_path_buf(),
            content: content.to_vec(),
        };
        match self.rpc(req).await? {
            Response::Write { bytes_written } => Ok(bytes_written),
            other => Err(unexpected_response("Write", other)),
        }
    }

    async fn edit(
        &self,
        path: &Path,
        old_string: &str,
        new_string: &str,
        _all: bool,
    ) -> FsResult<usize> {
        // `all` is not yet wired into the IPC protocol — the wire
        // always does a single-occurrence edit (Phase 2a §IPC). The
        // `all` flag is a tool-layer concern; when Phase 2d migrates
        // MultiEdit it will call `edit()` in a loop until count drops
        // to zero, which is equivalent.
        let req = Request::Edit {
            path: path.to_path_buf(),
            old_string: old_string.to_string(),
            new_string: new_string.to_string(),
        };
        match self.rpc(req).await? {
            Response::Edit { replacements } => Ok(replacements),
            other => Err(unexpected_response("Edit", other)),
        }
    }

    async fn glob(&self, pattern: &str, root: &Path) -> FsResult<Vec<PathBuf>> {
        let req = Request::Glob {
            pattern: pattern.to_string(),
            root: root.to_path_buf(),
        };
        match self.rpc(req).await? {
            Response::Glob { paths } => Ok(paths),
            other => Err(unexpected_response("Glob", other)),
        }
    }

    async fn grep(
        &self,
        pattern: &str,
        root: &Path,
        include: Option<&str>,
    ) -> FsResult<Vec<GrepMatch>> {
        let req = Request::Grep {
            pattern: pattern.to_string(),
            root: root.to_path_buf(),
            include: include.map(str::to_string),
        };
        match self.rpc(req).await? {
            Response::Grep { matches } => Ok(matches),
            other => Err(unexpected_response("Grep", other)),
        }
    }

    async fn stat(&self, path: &Path) -> FsResult<Metadata> {
        let req = Request::Stat {
            path: path.to_path_buf(),
        };
        match self.rpc(req).await? {
            Response::Stat {
                size,
                is_dir,
                is_symlink,
            } => Ok(Metadata {
                size,
                is_dir,
                is_symlink,
            }),
            other => Err(unexpected_response("Stat", other)),
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────

impl SandboxedFileSystem {
    /// Send a request to the worker and return the response.
    /// Maps `Response::Error` → `FsError`.
    async fn rpc(&self, req: Request) -> FsResult<Response> {
        let resp = self.client.lock().await.request(&req).await?;

        if let Response::Error { code, message } = resp {
            return Err(match code {
                ErrorCode::PolicyDenied => FsError::PolicyDenied { message },
                ErrorCode::Io => {
                    FsError::Io(std::io::Error::new(std::io::ErrorKind::Other, message))
                }
                ErrorCode::Protocol | ErrorCode::Internal | ErrorCode::Unimplemented => {
                    FsError::Transport { message }
                }
            });
        }

        Ok(resp)
    }
}

fn unexpected_response(op: &str, resp: Response) -> FsError {
    FsError::Transport {
        message: format!("{op}: unexpected response variant {resp:?}"),
    }
}
