//! Spawn-shaped wrapper around [`super::Server`] (Phase 3b of #934).
//!
//! Exists so callers don't care whether the proxy is a child process
//! ([`super::ExternalProxy`]) or an in-process tokio task: both return
//! the same [`super::ProxyHandle`], with the same `port` and `Drop`
//! contract. The slot manager / session factory then has a single
//! `Option<ProxyHandle>` field instead of a polymorphic enum it has
//! to match on.
//!
//! ## When you'd reach for this vs `ExternalProxy`
//!
//! - **`BuiltInProxy`**: default. The user has no upstream proxy
//!   infrastructure, no corporate MITM, no `mitmdump` running. We
//!   filter by hostname allowlist and that's it.
//!
//! - **`ExternalProxy`**: user *already* has a proxy doing the right
//!   thing (Zscaler, Squid, mitmdump for debugging). Stacking ours on
//!   top would create double-MITM chains. See [`super`] for the longer
//!   discussion.
//!
//! Phase 3d will add a third path: `BuiltInProxy` chained to an
//! `ExternalProxy` upstream via a Unix socket, for corp users who want
//! both hostname filtering AND TLS interception.

use super::{Filter, ProxyHandle, Server};
use anyhow::{Context, Result};
use std::time::Duration;
use tracing::debug;

/// Spec for the in-process built-in HTTP CONNECT proxy.
///
/// All fields default-friendly via [`BuiltInProxy::new`]. Configure by
/// mutating after construction or by literal struct syntax.
#[derive(Debug, Clone)]
pub struct BuiltInProxy {
    /// Hostname allowlist. `Filter::default()` is deny-all (refuses
    /// every CONNECT with `403`); use [`super::Filter::new`] with a
    /// list of patterns. See [`super::DEFAULT_DEV_ALLOWLIST`] for a
    /// reasonable seed.
    pub filter: Filter,

    /// Bind port. `None` selects an ephemeral port. Same defaulting
    /// rationale as [`super::ExternalProxy::port`].
    pub port: Option<u16>,

    /// Maximum wait for the proxy to start accepting. The built-in
    /// path is synchronous-bind so this is largely informational —
    /// kept for API parity with `ExternalProxy` so config can be
    /// shared between the two paths.
    pub startup_timeout: Duration,
}

impl BuiltInProxy {
    /// Construct with sensible defaults: deny-all filter, ephemeral
    /// port, 5 s startup timeout (matching `ExternalProxy`).
    pub fn new(filter: Filter) -> Self {
        Self {
            filter,
            port: None,
            startup_timeout: Duration::from_secs(5),
        }
    }

    /// Bind the listener and spawn the accept loop.
    ///
    /// Returns a [`ProxyHandle`] whose `Drop` impl aborts the spawned
    /// task (closing the listener). Same fail-open contract as
    /// [`super::ExternalProxy::spawn`]: on `Err`, callers should
    /// `warn!` and continue without traffic interception.
    ///
    /// Why no `wait_for_bind` poll loop: [`Server::bind`] synchronously
    /// completes the `TcpListener::bind` syscall before returning, so
    /// the listener is already accepting when this function returns.
    /// External proxies need polling because they're separate
    /// processes whose readiness we can't observe directly.
    pub async fn spawn(&self) -> Result<ProxyHandle> {
        let server = Server::bind(self.port, self.filter.clone())
            .await
            .context("bind built-in proxy")?;
        let port = server.port();
        let task = tokio::spawn(server.serve());

        debug!(
            "built-in proxy spawned: port={} filter_size={}",
            port,
            self.filter.len()
        );
        let handle = ProxyHandle::from_task(port, task);

        // Linux-only: also bind a UDS bridge that the bwrap
        // kernel-enforced sandbox (Phase 3c.1) can `connect()` through
        // after `--unshare-net` cuts host TCP. Bridge failure is not
        // fatal — the env-var enforcement tier still works for
        // well-behaved clients on every backend; we just lose the
        // kernel-enforced tier on Linux for this session.
        #[cfg(target_os = "linux")]
        let handle = match attach_uds_bridge(handle, port).await {
            Ok(h) => h,
            Err((h, e)) => {
                tracing::warn!(
                    "koda-sandbox: UDS bridge spawn failed: {e}. Linux kernel-enforced \
                     egress unavailable for this session; well-behaved clients still \
                     route through the env-var tier."
                );
                h
            }
        };

        Ok(handle)
    }
}

/// Attach the Linux UDS bridge to a freshly-spawned `ProxyHandle`.
///
/// Returns the (possibly-decorated) handle. On bridge spawn failure,
/// returns the original handle unchanged together with the error so
/// the caller can log + degrade rather than fail the whole session.
#[cfg(target_os = "linux")]
async fn attach_uds_bridge(
    handle: ProxyHandle,
    tcp_port: u16,
) -> std::result::Result<ProxyHandle, (ProxyHandle, anyhow::Error)> {
    let pid = std::process::id();
    let uds_path = crate::bwrap_proxy::proxy_uds_path(pid, tcp_port);
    match crate::bwrap_proxy::spawn_uds_bridge(uds_path.clone(), tcp_port).await {
        Ok(task) => Ok(handle.with_uds_bridge(uds_path, task)),
        Err(e) => Err((handle, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpStream;

    /// Convenience: open CONNECT, return the response status line.
    async fn connect_and_read_status(port: u16, target: &str) -> String {
        let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let req = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n");
        sock.write_all(req.as_bytes()).await.unwrap();
        let mut line = String::new();
        BufReader::new(sock).read_line(&mut line).await.unwrap();
        line
    }

    #[test]
    fn new_sets_defaults() {
        let p = BuiltInProxy::new(Filter::default());
        assert!(p.port.is_none());
        assert_eq!(p.startup_timeout, Duration::from_secs(5));
        assert!(p.filter.is_empty());
    }

    #[tokio::test]
    async fn spawn_returns_handle_with_assigned_port() {
        let p = BuiltInProxy::new(Filter::new(["github.com"]).unwrap());
        let h = p.spawn().await.unwrap();
        assert!(h.port > 0, "ephemeral port must be non-zero");
    }

    #[tokio::test]
    async fn spawned_proxy_serves_403_for_disallowed_host() {
        let p = BuiltInProxy::new(Filter::new(["github.com"]).unwrap());
        let h = p.spawn().await.unwrap();
        let status = connect_and_read_status(h.port, "evil.example.com:443").await;
        assert!(status.starts_with("HTTP/1.1 403"), "got: {status:?}");
    }

    #[tokio::test]
    async fn dropping_handle_stops_accepting() {
        // Spawn, confirm it accepts, drop, confirm it no longer accepts.
        let p = BuiltInProxy::new(Filter::new(["github.com"]).unwrap());
        let h = p.spawn().await.unwrap();
        let port = h.port;

        // Live: connect succeeds.
        let _ = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("must accept while alive");

        drop(h);

        // tokio::task::abort is async — give the runtime a tick to
        // tear the listener down. This is generous; in practice it
        // takes a few microseconds.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // After drop: connect refused (or at least the proxy doesn't
        // respond — we close immediately on fresh connects since the
        // listener is gone). Accept either ECONNREFUSED at connect
        // time or an immediate EOF on a stray buffered connect.
        match TcpStream::connect(("127.0.0.1", port)).await {
            Err(_) => {} // ECONNREFUSED — listener gone. Good.
            Ok(mut sock) => {
                // Some kernels accept the SYN into the orphaned backlog
                // before delivering the FIN. The connect "succeeds" but
                // the read returns 0. Verify that.
                let mut buf = [0u8; 16];
                let n = tokio::time::timeout(
                    Duration::from_millis(200),
                    tokio::io::AsyncReadExt::read(&mut sock, &mut buf),
                )
                .await
                .expect("read must not hang post-drop")
                .expect("read must not error");
                assert_eq!(n, 0, "post-drop socket must EOF immediately");
            }
        }
    }

    #[tokio::test]
    async fn ca_bundle_is_none_for_builtin() {
        // 3b deliberately ships no MITM. Built-in proxy has no CA to
        // advertise. 3d will swap this to Some(generated_path).
        let p = BuiltInProxy::new(Filter::default());
        let h = p.spawn().await.unwrap();
        assert!(h.ca_bundle().is_none());
    }

    /// Phase 3c.1.b contract: on Linux, every successful built-in proxy
    /// spawn also stands up a UDS bridge so the bwrap kernel-enforced
    /// path has a connectable socket inside the eventual `--unshare-net`
    /// sandbox.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_spawn_attaches_uds_bridge() {
        let p = BuiltInProxy::new(Filter::new(["github.com"]).unwrap());
        let h = p.spawn().await.unwrap();
        let uds = h
            .uds_path()
            .expect("Linux built-in proxy must attach a UDS bridge");
        assert!(
            uds.exists(),
            "UDS path {} should exist after bridge spawn",
            uds.display()
        );
    }

    /// Companion: dropping the handle removes the UDS file so the next
    /// spawn can re-bind cleanly. Without this, repeated session
    /// creation in the same process would accumulate stale sockets.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_drop_removes_uds_path() {
        let p = BuiltInProxy::new(Filter::new(["github.com"]).unwrap());
        let h = p.spawn().await.unwrap();
        let uds = h.uds_path().expect("bridge attached").to_owned();
        assert!(uds.exists());
        drop(h);
        // Cleanup is synchronous in `ProxyHandle::shutdown`, so the
        // file should be gone immediately — no sleep needed.
        assert!(
            !uds.exists(),
            "UDS path {} must be unlinked after drop",
            uds.display()
        );
    }
}
