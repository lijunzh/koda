//! Spawn-shaped wrapper around [`super::Socks5Server`] (Phase 3d.1 of #934).
//!
//! Mirrors [`super::builtin::BuiltInProxy`] for the SOCKS5 path: same
//! `spawn() -> ProxyHandle` shape so callers can hold the SOCKS5 and
//! HTTP CONNECT proxies in symmetric `Option<ProxyHandle>` slots.
//!
//! See [`super::socks5`] module docs for the wire-protocol subset.

use super::{Filter, ProxyHandle, Socks5Server};
use anyhow::{Context, Result};
use std::time::Duration;
use tracing::debug;

/// Spec for the in-process built-in SOCKS5 proxy.
///
/// All fields default-friendly via [`BuiltInSocks5Proxy::new`].
#[derive(Debug, Clone)]
pub struct BuiltInSocks5Proxy {
    /// Hostname allowlist. Same `Filter` type used by the HTTP CONNECT
    /// proxy — by design, both proxies share one allow list so a host
    /// permitted via HTTPS is also permitted via SOCKS5 (and vice
    /// versa). Two separate allow lists would invite skew between the
    /// two enforcement paths.
    pub filter: Filter,

    /// Bind port. `None` → ephemeral.
    pub port: Option<u16>,

    /// Maximum wait for the proxy to start accepting. Largely
    /// informational (see [`super::builtin::BuiltInProxy::startup_timeout`]).
    pub startup_timeout: Duration,
}

impl BuiltInSocks5Proxy {
    /// Construct with sensible defaults: deny-all filter, ephemeral
    /// port, 5 s startup timeout.
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
    /// task. Same fail-open contract as
    /// [`super::builtin::BuiltInProxy::spawn`]: on `Err`, callers should
    /// `warn!` and continue without SOCKS5 — well-behaved clients will
    /// still route through the HTTP CONNECT proxy.
    ///
    /// **No UDS bridge.** The Phase 3c.1 bwrap kernel-enforced path
    /// only bridges the HTTP proxy today; SOCKS5 is reachable through
    /// the env-var tier only. A SOCKS5-over-UDS bridge would need a
    /// matching `socks5h://` env override in the in-netns shell, which
    /// we'll add in a follow-up if there's a real client that needs it
    /// (git over ssh inside a sandbox is the obvious candidate).
    pub async fn spawn(&self) -> Result<ProxyHandle> {
        let server = Socks5Server::bind(self.port, self.filter.clone())
            .await
            .context("bind built-in socks5 proxy")?;
        let port = server.port();
        let task = tokio::spawn(server.serve());

        debug!(
            "built-in socks5 proxy spawned: port={} filter_size={}",
            port,
            self.filter.len()
        );
        Ok(ProxyHandle::from_task(port, task))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    #[test]
    fn new_sets_defaults() {
        let p = BuiltInSocks5Proxy::new(Filter::default());
        assert!(p.port.is_none());
        assert_eq!(p.startup_timeout, Duration::from_secs(5));
        assert!(p.filter.is_empty());
    }

    #[tokio::test]
    async fn spawn_returns_handle_with_assigned_port() {
        let p = BuiltInSocks5Proxy::new(Filter::new(["github.com"]).unwrap());
        let h = p.spawn().await.unwrap();
        assert!(h.port > 0);
    }

    #[tokio::test]
    async fn spawned_proxy_serves_403_equivalent_for_disallowed_host() {
        let p = BuiltInSocks5Proxy::new(Filter::new(["github.com"]).unwrap());
        let h = p.spawn().await.unwrap();

        let mut sock = TcpStream::connect(("127.0.0.1", h.port)).await.unwrap();
        // Greeting: VER=5, NMETHODS=1, METHOD=0x00 (no auth).
        sock.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut greet = [0u8; 2];
        sock.read_exact(&mut greet).await.unwrap();
        assert_eq!(greet, [0x05, 0x00]);

        // CONNECT-DOMAIN evil.example.com:443.
        let host = "evil.example.com";
        let mut req = vec![0x05, 0x01, 0x00, 0x03, host.len() as u8];
        req.extend_from_slice(host.as_bytes());
        req.extend_from_slice(&443u16.to_be_bytes());
        sock.write_all(&req).await.unwrap();

        let mut reply = [0u8; 4];
        sock.read_exact(&mut reply).await.unwrap();
        // 0x02 = "Connection not allowed by ruleset" — the SOCKS5
        // analogue of HTTP 403.
        assert_eq!(reply[1], 0x02);
    }

    #[tokio::test]
    async fn dropping_handle_stops_accepting() {
        let p = BuiltInSocks5Proxy::new(Filter::new(["github.com"]).unwrap());
        let h = p.spawn().await.unwrap();
        let port = h.port;

        // Live: accepts connections.
        let _ = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

        drop(h);

        // tokio::task::abort is async — give the runtime a tick. Same
        // pattern as the HTTP proxy test.
        tokio::time::sleep(Duration::from_millis(50)).await;

        match TcpStream::connect(("127.0.0.1", port)).await {
            Err(_) => {} // ECONNREFUSED — listener gone.
            Ok(mut sock) => {
                let mut buf = [0u8; 16];
                let n = tokio::time::timeout(
                    Duration::from_millis(200),
                    sock.read(&mut buf),
                )
                .await
                .expect("read must not hang post-drop")
                .expect("read must not error");
                assert_eq!(n, 0, "post-drop socket must EOF immediately");
            }
        }
    }
}
