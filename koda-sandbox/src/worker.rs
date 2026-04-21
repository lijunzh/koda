//! FS worker dispatch loop (Phase 2a of #934).
//!
//! The worker is a tiny event loop:
//!
//! ```text
//! loop {
//!     read_message(transport) -> Request
//!     handle(request)         -> Response
//!     write_message(transport, response)
//! }
//! ```
//!
//! It exits when:
//! - The peer closes the transport (clean EOF on `read_message`).
//! - It receives [`Request::Shutdown`] (sends `Pong`-equivalent ack
//!   then breaks the loop).
//! - An IO error occurs (logs and exits non-zero).
//!
//! Phase 2a only implements `Ping` and `Shutdown`. Every other request
//! kind returns `Response::Error { code: Unimplemented }`. The handlers
//! land in Phase 2c when the [`crate::ipc::Request::Read`] et al.
//! variants get real implementations against an enforced policy.
//!
//! ## Why a separate process at all?
//!
//! In-tree file-tool code runs in the same process as the LLM client
//! and the agent loop. That process is *the* trusted supervisor —
//! the kernel sandbox enforces nothing on it. Putting the FS code in a
//! worker process means we can:
//!
//! 1. **Run the worker outside the kernel sandbox** (it needs to
//!    actually do FS work), but still **enforce the policy in code**
//!    for every request. Matches CC's `SandboxedFileSystem` shape.
//! 2. **Bind the worker's lifecycle to a sub-agent slot** (Phase 4).
//!    Per-slot policy = per-slot worker = no cross-talk.
//! 3. **Crash isolation** — a panicking file handler kills the worker,
//!    not the host. The host can respawn.
//!
//! See #934 §4.5 for the full motivation.

use crate::ipc::{ErrorCode, Request, Response, read_message, write_message};
use anyhow::Result;
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{debug, warn};

/// Run the worker dispatch loop against an arbitrary duplex transport.
///
/// Production callers ([`run_stdio`] today, Unix socket spawner in
/// Phase 2c) wrap their transport and call this. Tests pass an
/// in-memory `tokio::io::duplex` half.
///
/// Returns `Ok(())` on clean shutdown (peer EOF or `Request::Shutdown`).
/// Returns `Err(...)` only for genuine transport errors — malformed
/// requests are reported back as `Response::Error` and the loop
/// continues.
pub async fn run<R, W>(reader: &mut R, writer: &mut W) -> Result<()>
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
                // Try to tell the peer it sent garbage before bailing.
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
        let resp = handle(req).await;
        write_message(writer, &resp).await?;

        if is_shutdown {
            debug!("worker: shutdown requested, exiting cleanly");
            return Ok(());
        }
    }
}

/// Handle a single request. Pure function of input → output (no shared
/// state today; that comes when handlers need a policy reference).
async fn handle(req: Request) -> Response {
    match req {
        Request::Ping => Response::Pong,
        // Shutdown's response is just a Pong-style ack so the host knows
        // the worker received the request before exiting. Any non-Error
        // variant would do; Pong is the obvious one.
        Request::Shutdown => Response::Pong,
        // ── Phase 2c stubs ─────────────────────────────────────────────
        Request::Read { .. }
        | Request::Write { .. }
        | Request::Edit { .. }
        | Request::Glob { .. }
        | Request::Grep { .. }
        | Request::Stat { .. } => Response::Error {
            code: ErrorCode::Unimplemented,
            message: "FS request handlers land in Phase 2c of #934".into(),
        },
    }
}

/// Convenience entry point: run the dispatch loop against process
/// stdin/stdout. This is what the `koda-fs-worker` binary calls.
///
/// Phase 2c will add `run_unix_socket(path)` for the production
/// per-slot wiring; stdio is what the unit tests use today.
pub async fn run_stdio() -> Result<()> {
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    run(&mut stdin, &mut stdout).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{ErrorCode, Request, Response};
    use std::path::PathBuf;
    use tokio::io::duplex;

    /// Spawn the worker against one half of a duplex pair, return the
    /// other half for the test to drive. The worker task is spawned on
    /// the runtime so the test can interleave reads and writes against
    /// it like a real peer.
    fn spawn_worker() -> (tokio::io::DuplexStream, tokio::task::JoinHandle<Result<()>>) {
        let (host, worker) = duplex(4096);
        let (mut wr, mut ww) = tokio::io::split(worker);
        let join = tokio::spawn(async move { run(&mut wr, &mut ww).await });
        (host, join)
    }

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
        // Drop our end so the worker sees clean EOF if it didn't exit yet.
        drop(host);
        tokio::time::timeout(std::time::Duration::from_secs(2), join)
            .await
            .expect("worker must exit within 2s of Shutdown")
            .expect("worker join error")
            .expect("worker returned Err");
    }

    #[tokio::test]
    async fn unimplemented_request_returns_error_response() {
        // Phase 2c will replace this assertion. Until then, every
        // FS-flavored variant must surface as Unimplemented so callers
        // get a clear signal instead of silent success.
        let (mut host, _join) = spawn_worker();
        write_message(
            &mut host,
            &Request::Read {
                path: PathBuf::from("/etc/passwd"),
                max_bytes: None,
            },
        )
        .await
        .unwrap();
        let resp: Response = read_message(&mut host).await.unwrap().unwrap();
        match resp {
            Response::Error {
                code: ErrorCode::Unimplemented,
                ..
            } => {}
            other => panic!("expected Unimplemented error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn worker_loops_for_multiple_requests() {
        // Pipelining sanity check: the loop must process N requests
        // back-to-back without losing framing or returning early.
        let (mut host, _join) = spawn_worker();
        for _ in 0..5 {
            write_message(&mut host, &Request::Ping).await.unwrap();
            let resp: Response = read_message(&mut host).await.unwrap().unwrap();
            assert_eq!(resp, Response::Pong);
        }
    }

    #[tokio::test]
    async fn peer_eof_exits_loop_cleanly() {
        // Drop the host end immediately. The worker must see EOF on
        // its next read and return Ok(()) — not panic, not block forever.
        let (host, join) = spawn_worker();
        drop(host);
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), join)
            .await
            .expect("worker must exit within 2s of EOF")
            .expect("worker join error");
        assert!(result.is_ok(), "expected clean exit, got {result:?}");
    }
}
