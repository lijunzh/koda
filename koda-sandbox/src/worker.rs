//! FS worker dispatch loop (Phase 2c of #934).
//!
//! The worker is a tiny event loop:
//!
//! ```text
//! loop {
//!     read_message(transport) -> Request
//!     handle(ctx, request)   -> Response
//!     write_message(transport, response)
//! }
//! ```
//!
//! Phase 2c implements all FS request kinds by delegating to
//! [`LocalFileSystem`]. Policy enforcement (checking requests against
//! [`crate::policy::SandboxPolicy`]) is wired in Phase 2f once the
//! CC defense patterns land. For now the worker enforces nothing —
//! the kernel-level sandbox does that job.
//!
//! The transport is either:
//! - **Unix domain socket** (production, Phase 2c+): the host spawns
//!   the binary with `--socket <path>`, the worker binds and signals
//!   "ready\n" on stdout, the host connects. See [`run_unix_socket`].
//! - **stdin/stdout** (testing / legacy): [`run_stdio`] — the
//!   `worker_binary` integration tests still use this path.

use crate::fs::{FileSystem, FsError, LocalFileSystem};
use crate::ipc::{ErrorCode, Request, Response, read_message, write_message};
use anyhow::Result;
use std::path::Path;
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{debug, warn};

// ── Context ──────────────────────────────────────────────────────────────

/// Per-connection state threaded through every handler.
///
/// Today this is just a [`LocalFileSystem`] — a zero-cost newtype.
/// Phase 2f adds a `policy: SandboxPolicy` field so the handlers can
/// enforce rules on each path before touching the filesystem.
struct Context {
    fs: LocalFileSystem,
}

impl Default for Context {
    fn default() -> Self {
        Self {
            fs: LocalFileSystem::new(),
        }
    }
}

// ── Dispatch loop ─────────────────────────────────────────────────────────

/// Run the worker dispatch loop against an arbitrary duplex transport.
///
/// Creates a default [`Context`] (bare `LocalFileSystem`, no policy
/// enforcement yet). All existing tests call this entry point.
///
/// Returns `Ok(())` on clean shutdown (peer EOF or `Request::Shutdown`).
/// Returns `Err(...)` only for genuine transport errors.
pub async fn run<R, W>(reader: &mut R, writer: &mut W) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let ctx = Context::default();
    run_with_ctx(&ctx, reader, writer).await
}

/// Inner loop — separated so Phase 2f can inject a policy-bearing context.
async fn run_with_ctx<R, W>(ctx: &Context, reader: &mut R, writer: &mut W) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    loop {
        let req = match read_message::<R, Request>(reader).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                debug!("worker: peer closed transport, exiting cleanly");
                return Ok(());
            }
            Err(e) => {
                warn!("worker: transport read error: {e}");
                let _ = write_message(
                    writer,
                    &Response::Error {
                        code: ErrorCode::Protocol,
                        message: format!("read error: {e}"),
                    },
                )
                .await;
                return Err(e.into());
            }
        };

        let is_shutdown = matches!(req, Request::Shutdown);
        let resp = handle(ctx, req).await;
        write_message(writer, &resp).await?;

        if is_shutdown {
            debug!("worker: shutdown requested, exiting cleanly");
            return Ok(());
        }
    }
}

// ── Request handlers ──────────────────────────────────────────────────────

async fn handle(ctx: &Context, req: Request) -> Response {
    match req {
        Request::Ping | Request::Shutdown => Response::Pong,
        Request::Read { path, max_bytes } => match ctx.fs.read(&path, max_bytes).await {
            Ok(content) => Response::Read { content },
            Err(e) => fs_err_to_resp(e),
        },
        Request::Write { path, content } => match ctx.fs.write(&path, &content).await {
            Ok(bytes_written) => Response::Write { bytes_written },
            Err(e) => fs_err_to_resp(e),
        },
        Request::Edit {
            path,
            old_string,
            new_string,
        } => {
            // Workers always do single-occurrence edits — the
            // `all` flag is a tool-layer choice, not an IPC primitive.
            match ctx.fs.edit(&path, &old_string, &new_string, false).await {
                Ok(replacements) => Response::Edit { replacements },
                Err(e) => fs_err_to_resp(e),
            }
        }
        Request::Glob { pattern, root } => match ctx.fs.glob(&pattern, &root).await {
            Ok(paths) => Response::Glob { paths },
            Err(e) => fs_err_to_resp(e),
        },
        Request::Grep {
            pattern,
            root,
            include,
        } => match ctx.fs.grep(&pattern, &root, include.as_deref()).await {
            Ok(matches) => Response::Grep { matches },
            Err(e) => fs_err_to_resp(e),
        },
        Request::Stat { path } => match ctx.fs.stat(&path).await {
            Ok(m) => Response::Stat {
                size: m.size,
                is_dir: m.is_dir,
                is_symlink: m.is_symlink,
            },
            Err(e) => fs_err_to_resp(e),
        },
    }
}

fn fs_err_to_resp(e: FsError) -> Response {
    let (code, message) = match e {
        FsError::Io(e) => (ErrorCode::Io, e.to_string()),
        FsError::PolicyDenied { message } => (ErrorCode::PolicyDenied, message),
        FsError::EditNotFound { path } => (
            ErrorCode::Io,
            format!("old_string not found in {}", path.display()),
        ),
        FsError::InvalidPattern { message } => (ErrorCode::Protocol, message),
        FsError::Transport { message } => (ErrorCode::Internal, message),
    };
    Response::Error { code, message }
}

// ── Transport entry points ────────────────────────────────────────────────

/// Run the dispatch loop against process stdin/stdout.
///
/// Used by the binary when invoked without `--socket` (integration
/// tests and legacy callers).
pub async fn run_stdio() -> Result<()> {
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    run(&mut stdin, &mut stdout).await
}

/// Bind a Unix domain socket at `path`, signal readiness to the host
/// by writing "ready\n" to stdout, accept exactly one connection, and
/// run the dispatch loop over it.
///
/// This is the production transport used by [`crate::worker_client`].
/// One connection, one slot, one worker — per-slot policy comes in 2f.
#[cfg(unix)]
pub async fn run_unix_socket(path: &Path) -> Result<()> {
    use std::io::Write as _;
    use tokio::net::UnixListener;

    let listener = UnixListener::bind(path)?;

    // Signal host — must flush before the host tries to connect.
    println!("ready");
    std::io::stdout().flush()?;

    let (stream, _addr) = listener.accept().await?;
    let (mut reader, mut writer) = tokio::io::split(stream);
    run(&mut reader, &mut writer).await
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{Request, Response};
    use tempfile::TempDir;
    use tokio::io::duplex;

    fn spawn_worker() -> (tokio::io::DuplexStream, tokio::task::JoinHandle<Result<()>>) {
        let (host, worker) = duplex(65536);
        let (mut wr, mut ww) = tokio::io::split(worker);
        let join = tokio::spawn(async move { run(&mut wr, &mut ww).await });
        (host, join)
    }

    // ── Protocol mechanics (unchanged from 2a) ───────────────────────

    #[tokio::test]
    async fn ping_returns_pong() {
        let (mut host, _join) = spawn_worker();
        write_message(&mut host, &Request::Ping).await.unwrap();
        let resp: Response = read_message(&mut host).await.unwrap().unwrap();
        assert_eq!(resp, Response::Pong);
    }

    #[tokio::test]
    async fn shutdown_acks_then_worker_exits() {
        let (mut host, join) = spawn_worker();
        write_message(&mut host, &Request::Shutdown).await.unwrap();
        let resp: Response = read_message(&mut host).await.unwrap().unwrap();
        assert_eq!(resp, Response::Pong);
        drop(host);
        tokio::time::timeout(std::time::Duration::from_secs(2), join)
            .await
            .expect("worker must exit within 2s of Shutdown")
            .expect("join error")
            .expect("worker returned Err");
    }

    #[tokio::test]
    async fn peer_eof_exits_loop_cleanly() {
        let (host, join) = spawn_worker();
        drop(host);
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), join)
            .await
            .expect("worker must exit within 2s of EOF")
            .expect("join error");
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn worker_loops_for_multiple_requests() {
        let (mut host, _join) = spawn_worker();
        for _ in 0..5 {
            write_message(&mut host, &Request::Ping).await.unwrap();
            let resp: Response = read_message(&mut host).await.unwrap().unwrap();
            assert_eq!(resp, Response::Pong);
        }
    }

    // ── FS handlers ──────────────────────────────────────────────────

    #[tokio::test]
    async fn read_handler_returns_file_contents() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("hello.txt");
        std::fs::write(&path, b"hello from worker").unwrap();

        let (mut host, _join) = spawn_worker();
        write_message(
            &mut host,
            &Request::Read {
                path,
                max_bytes: None,
            },
        )
        .await
        .unwrap();
        let resp: Response = read_message(&mut host).await.unwrap().unwrap();
        assert_eq!(
            resp,
            Response::Read {
                content: b"hello from worker".to_vec()
            }
        );
    }

    #[tokio::test]
    async fn write_handler_creates_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("out.txt");

        let (mut host, _join) = spawn_worker();
        write_message(
            &mut host,
            &Request::Write {
                path: path.clone(),
                content: b"written".to_vec(),
            },
        )
        .await
        .unwrap();
        let resp: Response = read_message(&mut host).await.unwrap().unwrap();
        assert_eq!(resp, Response::Write { bytes_written: 7 });
        assert_eq!(std::fs::read(&path).unwrap(), b"written");
    }

    #[tokio::test]
    async fn edit_handler_replaces_string() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("e.txt");
        std::fs::write(&path, b"foo bar baz").unwrap();

        let (mut host, _join) = spawn_worker();
        write_message(
            &mut host,
            &Request::Edit {
                path: path.clone(),
                old_string: "bar".to_string(),
                new_string: "BAR".to_string(),
            },
        )
        .await
        .unwrap();
        let resp: Response = read_message(&mut host).await.unwrap().unwrap();
        assert_eq!(resp, Response::Edit { replacements: 1 });
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "foo BAR baz");
    }

    #[tokio::test]
    async fn stat_handler_reports_file_metadata() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("s.txt");
        std::fs::write(&path, b"123456").unwrap();

        let (mut host, _join) = spawn_worker();
        write_message(&mut host, &Request::Stat { path })
            .await
            .unwrap();
        let resp: Response = read_message(&mut host).await.unwrap().unwrap();
        assert_eq!(
            resp,
            Response::Stat {
                size: 6,
                is_dir: false,
                is_symlink: false
            }
        );
    }

    #[tokio::test]
    async fn read_missing_file_returns_io_error() {
        let dir = TempDir::new().unwrap();
        let (mut host, _join) = spawn_worker();
        write_message(
            &mut host,
            &Request::Read {
                path: dir.path().join("nope"),
                max_bytes: None,
            },
        )
        .await
        .unwrap();
        let resp: Response = read_message(&mut host).await.unwrap().unwrap();
        assert!(matches!(
            resp,
            Response::Error {
                code: ErrorCode::Io,
                ..
            }
        ));
    }
}
