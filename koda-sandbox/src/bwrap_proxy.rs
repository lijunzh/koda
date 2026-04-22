//! UDS↔TCP proxy bridge for the bwrap kernel-enforced egress path
//! (Phase 3c.1 of #934).
//!
//! ## Why this exists
//!
//! On macOS, seatbelt's SBPL has a built-in
//! `(allow network-outbound (remote tcp "localhost:PORT"))` rule, so
//! kernel-enforced "deny all egress except the proxy port" is one
//! profile line (see `seatbelt::network_proxied_rules`, macOS-only).
//!
//! On Linux, bubblewrap's `--unshare-net` can deny ALL networking
//! including loopback — but it can't selectively allow a single
//! loopback port. Worse, the host's proxy on `127.0.0.1:PORT` is
//! unreachable from inside a fresh net namespace because the netns
//! has its own (initially-down) loopback that doesn't share state
//! with the host's.
//!
//! ## How codex solved this (and we borrowed the pattern)
//!
//! **Unix domain sockets cross network namespaces** because they're
//! filesystem objects, not network objects. So we can:
//!
//! 1. Spawn a **host bridge** (this module) that listens on a UDS
//!    path and forwards each accepted connection to the host's TCP
//!    proxy port.
//! 2. Bind-mount that UDS path into the bwrap sandbox via
//!    `--bind <uds> <uds>`.
//! 3. Inside the sandbox, after `--unshare-net` strips all networking,
//!    spawn a **local bridge** ([`crate::stage2`]) that brings up
//!    `lo`, binds an in-netns TCP listener, and forwards each accepted
//!    connection to the UDS.
//! 4. Set `HTTPS_PROXY=http://127.0.0.1:LOCAL_PORT` inside the sandbox
//!    so well-behaved clients use the in-netns TCP listener.
//!
//! Direct TCP from inside the sandbox to anywhere else **fails at
//! the kernel level** because the netns has no routes to anywhere
//! except its own loopback. That's the kernel enforcement.
//!
//! ## What this module owns
//!
//! Only the host-side half: the UDS listener that accepts connections
//! from inside the sandbox (via the bind-mount) and forwards them to
//! the host's TCP proxy. Pure tokio, lives as a task in the agent
//! process, dies when the [`tokio::task::JoinHandle`] is dropped.
//!
//! The in-sandbox local bridge lives in [`crate::stage2`] and runs as
//! a forked child of the `koda-sandbox-stage2` helper binary — it
//! can't be tokio because stage 2 must be a tiny synchronous helper
//! (no big runtime, executes before the user command).
//!
//! ## Reference
//!
//! Adapted from codex's `linux-sandbox/src/proxy_routing.rs`
//! (see `../../codex` checkout). Codex's version forks for both
//! bridges because their `linux-sandbox` is a CLI binary that exits
//! after spawning bwrap; koda's host-side bridge can be a tokio task
//! because koda's agent process is long-lived and already runs a
//! tokio runtime for the proxy itself.

#![cfg(target_os = "linux")]

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpStream, UnixListener};
use tokio::task::JoinHandle;
use tracing::{debug, trace, warn};

/// Env var the parent process sets in the bwrap-spawned `koda-sandbox-stage2`
/// child to identify the UDS path it must bridge to.
///
/// Stage 2 reads this, opens a TCP listener on the netns loopback,
/// and bridges accepted connections to the UDS.
pub const STAGE2_UDS_ENV_KEY: &str = "KODA_SANDBOX_STAGE2_UDS";

/// Env var the parent process sets in the bwrap-spawned `koda-sandbox-stage2`
/// child listing the comma-separated env keys whose values must be
/// rewritten to point at the in-netns TCP port that stage 2 binds.
///
/// Example value: `HTTP_PROXY,HTTPS_PROXY,ALL_PROXY`.
pub const STAGE2_REWRITE_KEYS_ENV_KEY: &str = "KODA_SANDBOX_STAGE2_REWRITE_KEYS";

/// Returns the deterministic UDS path for a given proxy port.
///
/// The path includes the parent PID + port so concurrent koda
/// processes don't collide on `/tmp/koda-sandbox-proxy-<pid>-<port>.sock`.
///
/// Lives under `/tmp` (bind-mounted into the sandbox by
/// `bwrap::build_command`'s `--bind /tmp /tmp`), so the
/// per-session sandbox can `connect()` to it without an extra
/// `--bind` rule. We also emit an explicit `--bind <uds> <uds>` for
/// clarity and to survive future bwrap layout changes that might
/// strip the broad `/tmp` mount.
pub fn proxy_uds_path(parent_pid: u32, proxy_port: u16) -> PathBuf {
    std::env::temp_dir().join(format!("koda-sandbox-proxy-{parent_pid}-{proxy_port}.sock"))
}

/// Spawn a tokio task that listens on `uds_path` and forwards each
/// accepted connection to `127.0.0.1:tcp_port`.
///
/// Returns the [`JoinHandle`] so callers (typically [`crate::proxy::ProxyHandle`])
/// can `abort()` it on drop. The UDS file is removed before binding
/// (in case a stale socket is left over from a crashed previous run)
/// and again best-effort when the task aborts.
///
/// ## Failure modes
///
/// - `bind` fails: returned `Err`. Likely cause: file exists and
///   isn't ours, or `/tmp` isn't writable. Caller should warn and
///   fall back to env-var-only enforcement.
/// - Per-connection forwarding errors: logged at `trace!` and the
///   connection is dropped. Doesn't affect other connections.
pub async fn spawn_uds_bridge(uds_path: PathBuf, tcp_port: u16) -> Result<JoinHandle<()>> {
    // Remove any stale socket file. Ignored if it doesn't exist.
    let _ = tokio::fs::remove_file(&uds_path).await;

    let listener = UnixListener::bind(&uds_path)
        .with_context(|| format!("bind UDS bridge at {}", uds_path.display()))?;

    debug!(
        "uds bridge listening on {} → 127.0.0.1:{tcp_port}",
        uds_path.display()
    );

    // Cleanup-on-drop: when the spawned task is aborted, the destructor
    // chain doesn't run (tokio just stops polling), so a `Drop` impl on a
    // guard wouldn't help. We rely on the caller having already removed the
    // file via `cleanup_uds_path()` after aborting the handle. The struct
    // that owns the handle (today, `ProxyHandle`) does this in its own
    // `Drop` impl. Keep `uds_path` here only because the `listener` keeps
    // the inode bound until aborted, regardless of whether anyone reads it.
    let _bound_path = uds_path;

    let task = tokio::spawn(async move {
        loop {
            let (uds_stream, _) = match listener.accept().await {
                Ok(pair) => pair,
                Err(e) => {
                    // EBADF after listener drop is the expected shutdown path.
                    trace!("uds bridge accept error (likely shutdown): {e}");
                    return;
                }
            };

            tokio::spawn(async move {
                let tcp_stream = match TcpStream::connect(("127.0.0.1", tcp_port)).await {
                    Ok(s) => s,
                    Err(e) => {
                        trace!("uds bridge: TCP connect to 127.0.0.1:{tcp_port} failed: {e}");
                        return;
                    }
                };
                if let Err(e) = bridge_streams(uds_stream, tcp_stream).await {
                    trace!("uds bridge: pipe error: {e}");
                }
            });
        }
    });

    Ok(task)
}

/// Best-effort cleanup of a stale UDS path. Called by [`crate::proxy::ProxyHandle::shutdown`]
/// after aborting the bridge task.
///
/// Errors are swallowed — if the file doesn't exist (the common case)
/// or we lack permissions (shouldn't happen for our own `/tmp` files),
/// there's nothing actionable.
pub fn cleanup_uds_path(path: &Path) {
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        warn!(
            "uds bridge cleanup: failed to remove {}: {e}",
            path.display()
        );
    }
}

/// Bidirectional pipe between a Unix socket stream and a TCP stream.
///
/// Splits both sides and uses `tokio::io::copy` in each direction,
/// joining when either half EOFs. Mirrors `proxy_bidirectional` in
/// codex's `proxy_routing.rs` but tokio-async instead of std-blocking.
async fn bridge_streams(
    uds_stream: tokio::net::UnixStream,
    tcp_stream: TcpStream,
) -> std::io::Result<()> {
    let (mut uds_r, mut uds_w) = uds_stream.into_split();
    let (mut tcp_r, mut tcp_w) = tcp_stream.into_split();

    let uds_to_tcp = async move {
        let _ = tokio::io::copy(&mut uds_r, &mut tcp_w).await;
        let _ = tcp_w.shutdown().await;
    };
    let tcp_to_uds = async move {
        let _ = tokio::io::copy(&mut tcp_r, &mut uds_w).await;
        let _ = uds_w.shutdown().await;
    };
    tokio::join!(uds_to_tcp, tcp_to_uds);
    Ok(())
}

/// Pure helper: rewrite the port of an `http://host:port` proxy URL.
///
/// Used by stage 2 to redirect e.g. `HTTPS_PROXY=http://127.0.0.1:54321`
/// (host-side port that's now unreachable inside the netns) to
/// `http://127.0.0.1:NEW_PORT` (the local bridge's in-netns port).
///
/// Returns `None` if the URL doesn't parse or doesn't have an authority
/// section we can replace. The grammar is intentionally narrow — we
/// only ever generate proxy URLs of the form `http://127.0.0.1:PORT`
/// in [`crate::proxy::env::proxy_env_vars`], so the corner cases are
/// few. For broader URL shapes (socks5h, etc.) callers should reach
/// for the `url` crate.
pub fn rewrite_proxy_url_port(url: &str, new_port: u16) -> Option<String> {
    // Accept `http://`, `https://`, `socks5://`, `socks5h://`, etc.
    let scheme_end = url.find("://")?;
    let after_scheme = &url[scheme_end + 3..];
    let authority_end = after_scheme.find('/').unwrap_or(after_scheme.len());
    let authority = &after_scheme[..authority_end];
    let rest = &after_scheme[authority_end..];

    // Authority is `[userinfo@]host[:port]`. We only rewrite the port.
    let (userinfo, host_port) = match authority.rsplit_once('@') {
        Some((u, hp)) => (Some(u), hp),
        None => (None, authority),
    };
    let host = match host_port.rsplit_once(':') {
        Some((h, _)) => h,
        None => host_port,
    };
    if host.is_empty() {
        return None;
    }

    let mut out = String::with_capacity(url.len() + 6);
    out.push_str(&url[..scheme_end + 3]);
    if let Some(u) = userinfo {
        out.push_str(u);
        out.push('@');
    }
    out.push_str(host);
    out.push(':');
    out.push_str(&new_port.to_string());
    out.push_str(rest);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, UnixStream};

    #[test]
    fn proxy_uds_path_includes_pid_and_port() {
        let p = proxy_uds_path(12345, 54321);
        let s = p.to_string_lossy();
        assert!(s.contains("12345"), "pid missing: {s}");
        assert!(s.contains("54321"), "port missing: {s}");
        assert!(s.ends_with(".sock"), "extension wrong: {s}");
        assert!(p.starts_with(std::env::temp_dir()), "not in tmp: {s}");
    }

    #[test]
    fn proxy_uds_path_distinct_for_distinct_inputs() {
        // Sanity: two different ports produce different paths.
        // Without this, concurrent koda sessions would collide.
        let a = proxy_uds_path(1, 100);
        let b = proxy_uds_path(1, 101);
        assert_ne!(a, b);
        let c = proxy_uds_path(2, 100);
        assert_ne!(a, c);
    }

    #[test]
    fn rewrite_proxy_url_port_handles_basic_http() {
        let out = rewrite_proxy_url_port("http://127.0.0.1:1234", 9999).unwrap();
        assert_eq!(out, "http://127.0.0.1:9999");
    }

    #[test]
    fn rewrite_proxy_url_port_handles_no_existing_port() {
        // The proxy_env_vars output always has an explicit port, but
        // be robust to user-provided values that omit it.
        let out = rewrite_proxy_url_port("http://localhost", 9999).unwrap();
        assert_eq!(out, "http://localhost:9999");
    }

    #[test]
    fn rewrite_proxy_url_port_preserves_path() {
        let out = rewrite_proxy_url_port("http://127.0.0.1:1234/path?q=1", 9999).unwrap();
        assert_eq!(out, "http://127.0.0.1:9999/path?q=1");
    }

    #[test]
    fn rewrite_proxy_url_port_preserves_userinfo() {
        let out = rewrite_proxy_url_port("http://user:pw@127.0.0.1:1234", 9999).unwrap();
        assert_eq!(out, "http://user:pw@127.0.0.1:9999");
    }

    #[test]
    fn rewrite_proxy_url_port_handles_socks_scheme() {
        // We don't generate these but stage 2 should pass them
        // through if the user provided one in their env.
        let out = rewrite_proxy_url_port("socks5h://127.0.0.1:1080", 9999).unwrap();
        assert_eq!(out, "socks5h://127.0.0.1:9999");
    }

    #[test]
    fn rewrite_proxy_url_port_returns_none_for_garbage() {
        assert_eq!(rewrite_proxy_url_port("not a url", 9999), None);
        assert_eq!(rewrite_proxy_url_port("http://", 9999), None);
    }

    #[tokio::test]
    async fn uds_bridge_forwards_bytes_to_tcp_listener() {
        // End-to-end: spin up a TCP listener, spawn the bridge, write
        // through the UDS, expect the bytes to arrive at the TCP side.
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tcp_port = tcp.local_addr().unwrap().port();

        // Echo server on the TCP side: read N bytes, write them back.
        tokio::spawn(async move {
            let (mut sock, _) = tcp.accept().await.unwrap();
            let mut buf = [0u8; 5];
            sock.read_exact(&mut buf).await.unwrap();
            sock.write_all(&buf).await.unwrap();
        });

        let uds_dir = tempfile::tempdir().unwrap();
        let uds_path = uds_dir.path().join("bridge.sock");
        let bridge = spawn_uds_bridge(uds_path.clone(), tcp_port).await.unwrap();

        // Connect via UDS, write 5 bytes, read them back through the bridge.
        let mut client = UnixStream::connect(&uds_path).await.unwrap();
        client.write_all(b"hello").await.unwrap();
        let mut got = [0u8; 5];
        tokio::time::timeout(Duration::from_secs(2), client.read_exact(&mut got))
            .await
            .expect("must echo within 2s")
            .unwrap();
        assert_eq!(&got, b"hello");

        bridge.abort();
        cleanup_uds_path(&uds_path);
    }

    #[tokio::test]
    async fn uds_bridge_removes_stale_socket_file() {
        // Pre-create a stale file at the UDS path; the bridge must
        // unlink-and-rebind rather than failing with EADDRINUSE.
        let uds_dir = tempfile::tempdir().unwrap();
        let uds_path = uds_dir.path().join("stale.sock");
        std::fs::write(&uds_path, b"leftover").unwrap();
        assert!(uds_path.exists());

        // No real TCP backend needed for this test; we just want the
        // bind to succeed. Use port 1 (definitely-no-listener) so any
        // stray connection attempt fails fast in the trace logs.
        let bridge = spawn_uds_bridge(uds_path.clone(), 1).await.unwrap();
        // bind succeeded → file is now a socket
        assert!(uds_path.exists());

        bridge.abort();
        cleanup_uds_path(&uds_path);
    }
}
