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
use crate::path_defense::{is_dangerous_system_path, is_path_inside, paths_for_write_check};
use crate::policy::SandboxPolicy;
use crate::policy_check::is_fully_denied;
use anyhow::Result;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{debug, warn};

// ── Context ──────────────────────────────────────────────────────────────

/// Per-connection state threaded through every handler.
///
/// Phase 2f adds `writable_root` and `policy` so the handlers can
/// enforce write policy on each path before touching the filesystem.
struct Context {
    fs: LocalFileSystem,
    /// Canonical path the slot is allowed to write to.
    /// `None` means no write-root enforcement (legacy / test callers).
    writable_root: Option<PathBuf>,
    policy: SandboxPolicy,
}

impl Default for Context {
    fn default() -> Self {
        Self {
            fs: LocalFileSystem::new(),
            writable_root: None,
            policy: SandboxPolicy::default(),
        }
    }
}

impl Context {
    fn with_policy(writable_root: PathBuf, policy: SandboxPolicy) -> Self {
        Self {
            fs: LocalFileSystem::new(),
            writable_root: Some(writable_root),
            policy,
        }
    }
}

// ── Dispatch loop ─────────────────────────────────────────────────────────

/// Run the worker dispatch loop against an arbitrary duplex transport.
///
/// Creates a default context (bare `LocalFileSystem`, no policy
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

/// Run the worker with a sandbox policy and writable root.
///
/// This is the entry point used by the production worker binary when
/// spawned by [`crate::worker_client::WorkerClient`]. The policy is
/// applied to every `Write` and `Edit` request.
pub async fn run_with_policy<R, W>(
    writable_root: PathBuf,
    policy: SandboxPolicy,
    reader: &mut R,
    writer: &mut W,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let ctx = Context::with_policy(writable_root, policy);
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

/// Canonicalize `path` for a policy containment check.
///
/// `std::fs::canonicalize` fails with `ENOENT` for paths that don't
/// exist yet (e.g. the target of a Write request). We resolve the
/// deepest existing prefix instead, then rejoin the non-existing tail.
/// This correctly resolves platform symlinks like macOS `/var` →
/// `/private/var` even when the leaf file hasn't been created yet.
fn canonicalize_for_check(path: &Path) -> PathBuf {
    // Fast path: the path already exists and canonicalize succeeds.
    if let Ok(c) = std::fs::canonicalize(path) {
        return c;
    }
    // Slow path: walk up to the first existing ancestor, canonicalize it,
    // then rejoin the non-existing tail segments.
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut dir = path.to_path_buf();
    loop {
        match std::fs::canonicalize(&dir) {
            Ok(c) => {
                return tail.iter().rev().fold(c, |acc, seg| acc.join(seg));
            }
            Err(_) => {
                if let Some(name) = dir.file_name() {
                    tail.push(name.to_os_string());
                }
                match dir.parent() {
                    Some(p) => dir = p.to_path_buf(),
                    None => return path.to_path_buf(), // gave up
                }
            }
        }
    }
}

/// Validate `path` against the slot's write policy.
///
/// Runs three layers in order:
/// 1. **Fully-denied** — koda's own credential DB (`~/.config/koda/db`).
/// 2. **Dangerous system path** — `/`, `/usr`, `/etc`, etc.
/// 3. **Writable-root containment** — the path (and every hop in its
///    symlink chain) must resolve to inside the slot's writable root.
///
/// Returns `Ok(())` when all checks pass, `Err(PolicyDenied { … })` otherwise.
fn check_write_path(ctx: &Context, path: &Path) -> Result<(), FsError> {
    // 1. Absolute fully-denied paths — e.g. koda's own API-key database.
    if is_fully_denied(path) {
        return Err(FsError::PolicyDenied {
            message: format!(
                "write denied: '{}' is a fully-protected path",
                path.display()
            ),
        });
    }

    // 2. Catastrophic system-path guard — regardless of allow rules.
    if is_dangerous_system_path(path) {
        return Err(FsError::PolicyDenied {
            message: format!(
                "write denied: '{}' is a critical system path",
                path.display()
            ),
        });
    }

    // 3. Writable-root containment — only enforced when a root is configured.
    let Some(ref root) = ctx.writable_root else {
        return Ok(()); // legacy/test mode: no root enforcement
    };

    // Expand the full symlink chain so a symlink pointing outside the root
    // is caught even when the link itself lives inside it. The chain depth
    // is policy-derived (Plan/Safe/Auto get different ceilings via
    // `policy_for_agent`), with a hardcoded floor of MIN_SAFE_SYMLINK_DEPTH
    // inside the helper so a low policy value can't weaken this check.
    let chain = paths_for_write_check(path, ctx.policy.fs.mandatory_deny_search_depth);

    for candidate in &chain {
        // Absolute-deny overrides a broad allow-write rule.
        if is_fully_denied(candidate) || is_dangerous_system_path(candidate) {
            return Err(FsError::PolicyDenied {
                message: format!(
                    "write denied: symlink chain reaches protected path '{}'",
                    candidate.display()
                ),
            });
        }

        // Canonical containment check.
        // For new files, `canonicalize(candidate)` returns ENOENT. Instead
        // canonicalize the parent (which exists) and rejoin the filename.
        // This handles the macOS `/var` → `/private/var` symlink, etc.
        let canonical_candidate = canonicalize_for_check(candidate);
        let canonical_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());

        if !is_path_inside(&canonical_candidate, &canonical_root) {
            let explicitly_allowed = ctx
                .policy
                .fs
                .allow_write
                .iter()
                .any(|pat| is_path_inside(&canonical_candidate, pat.as_path()));

            let explicitly_denied = ctx
                .policy
                .fs
                .deny_write_within_allow
                .iter()
                .any(|pat| is_path_inside(&canonical_candidate, pat.as_path()));

            if !explicitly_allowed || explicitly_denied {
                return Err(FsError::PolicyDenied {
                    message: format!(
                        "write denied: '{}' is outside writable root '{}'",
                        path.display(),
                        root.display()
                    ),
                });
            }
        } else {
            // Inside the root — check deny_write_within_allow carve-outs.
            let carved_out = ctx
                .policy
                .fs
                .deny_write_within_allow
                .iter()
                .any(|pat| is_path_inside(&canonical_candidate, pat.as_path()));
            if carved_out {
                return Err(FsError::PolicyDenied {
                    message: format!(
                        "write denied: '{}' is in a write-protected sub-path",
                        path.display()
                    ),
                });
            }
        }
    }

    Ok(())
}

async fn handle(ctx: &Context, req: Request) -> Response {
    match req {
        Request::Ping | Request::Shutdown => Response::Pong,
        Request::Read { path, max_bytes } => match ctx.fs.read(&path, max_bytes).await {
            Ok(content) => Response::Read { content },
            Err(e) => fs_err_to_resp(e),
        },
        Request::Write { path, content } => {
            if let Err(e) = check_write_path(ctx, &path) {
                return fs_err_to_resp(e);
            }
            match ctx.fs.write(&path, &content).await {
                Ok(bytes_written) => Response::Write { bytes_written },
                Err(e) => fs_err_to_resp(e),
            }
        }
        Request::Edit {
            path,
            old_string,
            new_string,
        } => {
            if let Err(e) = check_write_path(ctx, &path) {
                return fs_err_to_resp(e);
            }
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
        Request::GetEnv { names } => {
            // No fs/policy check — the caller already controls what env vars
            // we see, so this is a mirror, not a leak. Used by Phase 3a tests
            // to verify proxy env-var injection across the process boundary.
            let values = names.into_iter().map(|n| std::env::var(&n).ok()).collect();
            Response::GetEnv { values }
        }
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
/// No policy enforcement — use [`run_unix_socket_with_policy`] for
/// production slots. This entry point exists for tests and legacy callers.
#[cfg(unix)]
pub async fn run_unix_socket(path: &Path) -> Result<()> {
    unix_socket_serve(path, Context::default()).await
}

/// Bind a Unix domain socket at `path` and serve one connection with
/// write-policy enforcement.
///
/// This is the production entry point: the `koda-fs-worker` binary
/// calls this when `--root <path>` is supplied. Every `Write` and `Edit`
/// request is validated against `writable_root` and `policy` via
/// `check_write_path` before touching the filesystem.
///
/// The policy JSON is passed to the binary via the
/// `KODA_FS_WORKER_POLICY` environment variable; the writable root
/// is passed via `--root <path>`. [`crate::worker_client::WorkerClient`]
/// handles both when spawning with [`crate::worker_client::WorkerClient::spawn_with_policy`].
#[cfg(unix)]
pub async fn run_unix_socket_with_policy(
    path: &Path,
    writable_root: PathBuf,
    policy: SandboxPolicy,
) -> Result<()> {
    unix_socket_serve(path, Context::with_policy(writable_root, policy)).await
}

/// Shared Unix-socket bind/accept/serve logic.
///
/// Binds `path`, prints `"ready\n"` to stdout, accepts one connection,
/// runs the dispatch loop with `ctx`. Kept private — callers pick the
/// right context via the public wrappers above.
#[cfg(unix)]
async fn unix_socket_serve(path: &Path, ctx: Context) -> Result<()> {
    use std::io::Write as _;
    use tokio::net::UnixListener;

    let listener = UnixListener::bind(path)?;

    // Signal host — must flush before the host tries to connect.
    println!("ready");
    std::io::stdout().flush()?;

    let (stream, _addr) = listener.accept().await?;
    let (mut reader, mut writer) = tokio::io::split(stream);
    run_with_ctx(&ctx, &mut reader, &mut writer).await
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

    // ── Phase 2f: policy-gate tests ──────────────────────────────

    fn spawn_worker_with_root(
        root: PathBuf,
    ) -> (tokio::io::DuplexStream, tokio::task::JoinHandle<Result<()>>) {
        let (host, worker) = duplex(65536);
        let (mut wr, mut ww) = tokio::io::split(worker);
        let policy = crate::policy::SandboxPolicy::default();
        let join =
            tokio::spawn(async move { run_with_policy(root, policy, &mut wr, &mut ww).await });
        (host, join)
    }

    #[tokio::test]
    async fn write_inside_root_is_allowed() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("allowed.txt");
        let (mut host, _join) = spawn_worker_with_root(dir.path().to_path_buf());
        write_message(
            &mut host,
            &Request::Write {
                path: path.clone(),
                content: b"ok".to_vec(),
            },
        )
        .await
        .unwrap();
        let resp: Response = read_message(&mut host).await.unwrap().unwrap();
        assert!(
            matches!(resp, Response::Write { .. }),
            "expected Write ok, got {resp:?}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"ok");
    }

    #[tokio::test]
    async fn write_outside_root_is_denied() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let path = outside.path().join("escape.txt");
        let (mut host, _join) = spawn_worker_with_root(root.path().to_path_buf());
        write_message(
            &mut host,
            &Request::Write {
                path,
                content: b"evil".to_vec(),
            },
        )
        .await
        .unwrap();
        let resp: Response = read_message(&mut host).await.unwrap().unwrap();
        assert!(
            matches!(
                resp,
                Response::Error {
                    code: ErrorCode::PolicyDenied,
                    ..
                }
            ),
            "expected PolicyDenied, got {resp:?}"
        );
    }

    #[tokio::test]
    async fn dangerous_system_path_always_denied() {
        let dir = TempDir::new().unwrap();
        // Even though root is set, /etc directly is always blocked.
        let (mut host, _join) = spawn_worker_with_root(dir.path().to_path_buf());
        write_message(
            &mut host,
            &Request::Write {
                path: PathBuf::from("/etc"),
                content: b"evil".to_vec(),
            },
        )
        .await
        .unwrap();
        let resp: Response = read_message(&mut host).await.unwrap().unwrap();
        assert!(
            matches!(
                resp,
                Response::Error {
                    code: ErrorCode::PolicyDenied,
                    ..
                }
            ),
            "expected PolicyDenied, got {resp:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_escape_is_denied() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        // Create a symlink inside root that points to outside.
        let link = root.path().join("escape_link");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();

        let through_link = link.join("secret.txt");
        let (mut host, _join) = spawn_worker_with_root(root.path().to_path_buf());
        write_message(
            &mut host,
            &Request::Write {
                path: through_link,
                content: b"evil".to_vec(),
            },
        )
        .await
        .unwrap();
        let resp: Response = read_message(&mut host).await.unwrap().unwrap();
        assert!(
            matches!(
                resp,
                Response::Error {
                    code: ErrorCode::PolicyDenied,
                    ..
                }
            ),
            "symlink escape should be denied, got {resp:?}"
        );
    }
}
