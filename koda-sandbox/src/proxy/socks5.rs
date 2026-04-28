//! Built-in SOCKS5 proxy server (Phase 3d.1 of #934).
//!
//! Implements the *minimum* SOCKS5 surface needed for sandboxed
//! processes that don't honor `HTTPS_PROXY` (git over ssh, raw-TCP
//! tools, gRPC clients) but do honor `ALL_PROXY=socks5h://...`.
//!
//! ## Subset
//!
//! | RFC 1928 feature | Supported | Why / why not |
//! |---|---|---|
//! | VER = 5 | yes | The whole point of the file. |
//! | METHOD = `0x00` (no auth) | yes | Loopback-only listener; the kernel-level allow list (Phase 3c/3c.1) is the real auth boundary. |
//! | METHOD = `0x02` (user/pass) | no | Same reason: nothing on `127.0.0.1` justifies it. |
//! | CMD = `0x01` (CONNECT) | yes | The verb every TCP client uses. |
//! | CMD = `0x02` (BIND) | no | Reverse connections from upstream are a sandbox-egress hole by design — hard `0x07 Command not supported`. |
//! | CMD = `0x03` (UDP ASSOCIATE) | no | Same. UDP egress isn't in scope for this issue (would need its own filter model). |
//! | ATYP = `0x03` (DOMAIN) | yes | The whole *point* of `socks5h://` — the proxy resolves, so we can run the hostname through [`super::Filter`]. |
//! | ATYP = `0x01` (IPv4) | no | A pre-resolved IP literal *bypasses* hostname filtering. Hard `0x08 Address type not supported`. Forces clients to use `socks5h://`. |
//! | ATYP = `0x04` (IPv6) | no | Same. |
//!
//! Loud rejection (proper REP code) instead of silent allow keeps the
//! behaviour discoverable: the client surfaces "proxy refused" rather
//! than mysteriously connecting to the wrong place.
//!
//! ## Filter contract
//!
//! Identical to [`super::server`]: every `CONNECT` target host is
//! checked against a [`super::Filter`]. Reject → `0x02 Connection not
//! allowed by ruleset`. Allow → connect upstream, send `0x00 succeeded`,
//! splice with [`tokio::io::copy_bidirectional`].
//!
//! ## Why hand-rolled?
//!
//! Codex pulls in the `rama_socks5` framework (~kLOC, dozens of
//! transitive deps); CC has no SOCKS5 at all. For our subset the wire
//! format is ~80 bytes of state machine — a crate would dwarf the
//! actual logic. See #934 §6 Phase 3 acceptance: "≤200 LOC".

use super::Filter;
use anyhow::{Context, Result, bail};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, warn};

/// How long to wait for the client to finish the greeting + request
/// handshake. Generous — both messages together are <300 bytes and
/// arrive in a single TCP segment in practice.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum DOMAIN length per RFC 1928 (the length byte is `u8`, so
/// 255 is the wire ceiling). Valid hostnames cap at 253; we accept up
/// to 255 to match the spec but the hostname allowlist will reject
/// anything dodgy anyway.
const MAX_DOMAIN_LEN: usize = 255;

// ── REP codes (RFC 1928 §6) ────────────────────────────────────────────────
const REP_SUCCEEDED: u8 = 0x00;
const REP_GENERAL_FAILURE: u8 = 0x01;
const REP_NOT_ALLOWED: u8 = 0x02;
const REP_NETWORK_UNREACHABLE: u8 = 0x03;
const REP_HOST_UNREACHABLE: u8 = 0x04;
const REP_CONNECTION_REFUSED: u8 = 0x05;
const REP_COMMAND_NOT_SUPPORTED: u8 = 0x07;
const REP_ADDRESS_TYPE_NOT_SUPPORTED: u8 = 0x08;

/// Built-in SOCKS5 server. Cheap to construct, run with [`Self::serve`].
#[derive(Debug)]
pub struct Socks5Server {
    listener: TcpListener,
    filter: Filter,
    port: u16,
    /// Upstream-connection policy snapshotted from the koda process's
    /// env at bind time (Phase 3d.3 of #934). `Direct` in plain dev
    /// environments; `HttpProxy` when the user has corp-set
    /// `HTTPS_PROXY` so we can chain through Zscaler / Squid / etc.
    /// Even though SOCKS5 itself is wire-incompatible with HTTP
    /// CONNECT, the *upstream* hop only needs raw TCP — we open it
    /// via CONNECT to the corp proxy and treat the returned tunnel
    /// as a transparent socket.
    upstream: crate::proxy::upstream::UpstreamConfig,
}

impl Socks5Server {
    /// Bind on `127.0.0.1:port` (or kernel-picked ephemeral if `None`).
    ///
    /// Same TOCTOU-free pattern as [`super::server::Server::bind`]:
    /// we hand `port = 0` directly to the kernel rather than running
    /// our own pick-then-rebind dance.
    pub async fn bind(port: Option<u16>, filter: Filter) -> Result<Self> {
        let bind_port = port.unwrap_or(0);
        let listener = TcpListener::bind(("127.0.0.1", bind_port))
            .await
            .with_context(|| format!("bind socks5 listener on 127.0.0.1:{bind_port}"))?;
        let port = listener.local_addr()?.port();
        let upstream = crate::proxy::upstream::UpstreamConfig::from_env();
        Ok(Self {
            listener,
            filter,
            port,
            upstream,
        })
    }

    /// Port the server is listening on. Useful when [`Self::bind`] was
    /// called with `None`.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Override the upstream-connection policy that [`Self::bind`]
    /// snapshotted from `HTTPS_PROXY`. Sibling of
    /// [`super::server::Server::with_upstream`] — see that fn for
    /// rationale.
    pub fn with_upstream(mut self, upstream: crate::proxy::upstream::UpstreamConfig) -> Self {
        self.upstream = upstream;
        self
    }

    /// Run the accept loop forever. Caller drops the server (or aborts
    /// the spawning [`tokio::task::JoinHandle`]) for shutdown.
    pub async fn serve(self) {
        let filter = self.filter;
        let upstream = std::sync::Arc::new(self.upstream);
        loop {
            let (sock, peer) = match self.listener.accept().await {
                Ok(t) => t,
                Err(e) => {
                    warn!("socks5 accept failed: {e}");
                    return;
                }
            };
            let f = filter.clone();
            let up = std::sync::Arc::clone(&upstream);
            tokio::spawn(async move {
                if let Err(e) = handle_one(sock, &f, &up).await {
                    debug!("socks5 connection from {peer} ended: {e:#}");
                }
            });
        }
    }
}

/// Handle one client: greet, parse request, filter, splice.
///
/// Returns `Ok(())` for cleanly-rejected requests (any REP code) AND
/// for the happy path; `Err` is reserved for socket failures the
/// caller can't act on beyond logging.
async fn handle_one(
    mut client: TcpStream,
    filter: &Filter,
    upstream: &crate::proxy::upstream::UpstreamConfig,
) -> Result<()> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        greet(&mut client).await?;
        let target = read_request(&mut client).await?;

        let target = match target {
            RequestOutcome::Reject { rep, atyp } => {
                send_reply(&mut client, rep, atyp).await?;
                return Ok(());
            }
            RequestOutcome::Connect(t) => t,
        };

        if !filter.allows(&target.host) {
            debug!("socks5: blocked CONNECT {} (not in allowlist)", target.host);
            send_reply(&mut client, REP_NOT_ALLOWED, 0x03).await?;
            return Ok(());
        }

        // Allowed. Resolve + dial upstream — either directly or through
        // the chained corp proxy. The `connect_upstream` helper handles
        // the dispatch, including NO_PROXY bypass for hosts that the
        // user's environment marks as direct-routable.
        let upstream_addr = format!("{}:{}", target.host, target.port);
        let dial = crate::proxy::upstream::connect_upstream(&upstream_addr, upstream).await;

        let upstream_sock = match dial {
            Ok(s) => s,
            Err(e) => {
                // The upstream layer wraps anyhow::Error around a root
                // cause that may be an `io::Error` (direct dial) or a
                // CONNECT-status error (chained dial). For the former we
                // can map to a precise REP code; for the latter we use
                // REP_GENERAL_FAILURE because none of the SOCKS5 codes
                // really capture "corp proxy refused".
                let rep = e
                    .downcast_ref::<std::io::Error>()
                    .map(io_error_to_rep)
                    .unwrap_or(REP_GENERAL_FAILURE);
                warn!("socks5: upstream connect to {upstream_addr} failed: {e:#}");
                send_reply(&mut client, rep, 0x03).await?;
                return Ok(());
            }
        };

        send_reply(&mut client, REP_SUCCEEDED, 0x03).await?;

        // Bidirectional copy with idle + total timeouts (Phase 3f of
        // #934). Same rationale as [`super::server`]: SOCKS5 clients
        // (git-over-https, raw TCP) never half-close after the reply,
        // so without an idle deadline a wedged peer pins the slot for
        // the kernel keepalive's two-hour default. See
        // [`super::relay::relay_with_timeouts`] for the design notes.
        let _ = super::relay::relay_with_timeouts(
            client,
            upstream_sock,
            super::relay::DEFAULT_IDLE_TIMEOUT,
            super::relay::DEFAULT_TOTAL_TIMEOUT,
        )
        .await;
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("socks5 handshake timed out")??;
    Ok(())
}

/// SOCKS5 greeting. Reads `[VER][NMETHODS][METHODS...]` and replies
/// `[VER=5][METHOD=0x00]` if no-auth was offered, else `[VER=5][0xFF]`
/// and bails. RFC 1928 §3.
async fn greet(client: &mut TcpStream) -> Result<()> {
    let mut header = [0u8; 2];
    client.read_exact(&mut header).await?;
    if header[0] != 0x05 {
        bail!("not a socks5 client (VER={:#x})", header[0]);
    }
    let nmethods = header[1] as usize;
    let mut methods = vec![0u8; nmethods];
    if nmethods > 0 {
        client.read_exact(&mut methods).await?;
    }
    if !methods.contains(&0x00) {
        client.write_all(&[0x05, 0xFF]).await?;
        bail!("client offered no acceptable auth methods");
    }
    client.write_all(&[0x05, 0x00]).await?;
    Ok(())
}

/// Parsed CONNECT target.
#[derive(Debug)]
struct ConnectTarget {
    host: String,
    port: u16,
}

/// Outcome of [`read_request`]: either a host to connect to, or the
/// REP code we should send back unconditionally.
#[derive(Debug)]
enum RequestOutcome {
    Connect(ConnectTarget),
    /// Send `rep`, with BND.ATYP = `atyp` (echoes whatever the client
    /// sent so well-behaved clients can pretty-print the failure).
    Reject {
        rep: u8,
        atyp: u8,
    },
}

/// Parse `[VER=5][CMD][RSV=0][ATYP][DST.ADDR][DST.PORT]`. RFC 1928 §4.
async fn read_request(client: &mut TcpStream) -> Result<RequestOutcome> {
    let mut header = [0u8; 4];
    client.read_exact(&mut header).await?;
    let [ver, cmd, _rsv, atyp] = header;
    if ver != 0x05 {
        bail!("bad request version {ver:#x}");
    }
    if cmd != 0x01 {
        // BIND or UDP ASSOCIATE — see module docstring.
        // Drain whatever address is coming so we send a clean reply.
        drain_addr_port(client, atyp).await?;
        return Ok(RequestOutcome::Reject {
            rep: REP_COMMAND_NOT_SUPPORTED,
            atyp,
        });
    }

    let host = match atyp {
        0x03 => {
            // DOMAIN: 1-byte length + N bytes.
            let mut len_buf = [0u8; 1];
            client.read_exact(&mut len_buf).await?;
            let len = len_buf[0] as usize;
            if len == 0 || len > MAX_DOMAIN_LEN {
                drain_port(client).await?;
                return Ok(RequestOutcome::Reject {
                    rep: REP_GENERAL_FAILURE,
                    atyp,
                });
            }
            let mut buf = vec![0u8; len];
            client.read_exact(&mut buf).await?;
            match String::from_utf8(buf) {
                Ok(s) => s,
                Err(_) => {
                    drain_port(client).await?;
                    return Ok(RequestOutcome::Reject {
                        rep: REP_GENERAL_FAILURE,
                        atyp,
                    });
                }
            }
        }
        0x01 | 0x04 => {
            // IPv4 / IPv6 literal — see module docstring on why we refuse.
            drain_addr_port(client, atyp).await?;
            return Ok(RequestOutcome::Reject {
                rep: REP_ADDRESS_TYPE_NOT_SUPPORTED,
                atyp,
            });
        }
        other => {
            // Drain unknown — but we have no idea how long, so just bail.
            bail!("unknown ATYP {other:#x}");
        }
    };

    let mut port_buf = [0u8; 2];
    client.read_exact(&mut port_buf).await?;
    let port = u16::from_be_bytes(port_buf);

    Ok(RequestOutcome::Connect(ConnectTarget { host, port }))
}

/// Skip `addr + port` bytes for a known ATYP. Used after we've decided
/// to reject so the client gets a clean reply instead of a half-read
/// socket.
async fn drain_addr_port(client: &mut TcpStream, atyp: u8) -> Result<()> {
    match atyp {
        0x01 => {
            let mut buf = [0u8; 4 + 2];
            client.read_exact(&mut buf).await?;
        }
        0x03 => {
            let mut len = [0u8; 1];
            client.read_exact(&mut len).await?;
            let mut buf = vec![0u8; len[0] as usize + 2];
            client.read_exact(&mut buf).await?;
        }
        0x04 => {
            let mut buf = [0u8; 16 + 2];
            client.read_exact(&mut buf).await?;
        }
        _ => {}
    }
    Ok(())
}

/// Drain just the trailing 2-byte port. Used when we already consumed
/// the address but want a clean reply.
async fn drain_port(client: &mut TcpStream) -> Result<()> {
    let mut port_buf = [0u8; 2];
    client.read_exact(&mut port_buf).await?;
    Ok(())
}

/// Send a SOCKS5 reply. BND.ADDR/BND.PORT are zeroed — we don't need
/// to echo a real bound address for CONNECT, and zeroing matches what
/// most permissive clients expect when the server isn't in BIND mode.
async fn send_reply(client: &mut TcpStream, rep: u8, atyp: u8) -> Result<()> {
    // [VER=5][REP][RSV=0][ATYP][zeros for addr][zeros for port]
    let addr_len = match atyp {
        0x01 => 4,
        0x04 => 16,
        // For DOMAIN we send a 1-byte length of 0 + 0 bytes of addr,
        // which is the cheapest legal encoding.
        _ => 1,
    };
    let mut reply = vec![0u8; 4 + addr_len + 2];
    reply[0] = 0x05;
    reply[1] = rep;
    reply[2] = 0x00;
    reply[3] = if matches!(atyp, 0x01 | 0x04) {
        atyp
    } else {
        0x03
    };
    // Bytes [4..] already zero from `vec![0u8; ...]`.
    client.write_all(&reply).await?;
    Ok(())
}

/// Map a `tokio::io::Error` from `TcpStream::connect` to the closest
/// SOCKS5 REP code so well-behaved clients can render a helpful error.
fn io_error_to_rep(e: &std::io::Error) -> u8 {
    use std::io::ErrorKind::*;
    match e.kind() {
        ConnectionRefused => REP_CONNECTION_REFUSED,
        // `NetworkUnreachable` and `HostUnreachable` only stabilised in
        // 1.83; gate on the kind via a string match to stay on stable
        // MSRV without a version dance.
        _ => {
            let s = e.to_string().to_ascii_lowercase();
            if s.contains("network is unreachable") {
                REP_NETWORK_UNREACHABLE
            } else if s.contains("no route to host") || s.contains("host is unreachable") {
                REP_HOST_UNREACHABLE
            } else {
                REP_GENERAL_FAILURE
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spawn a server on an ephemeral port and return its port.
    ///
    /// Always pins the upstream to `Direct` so the test is immune to
    /// any ambient `HTTPS_PROXY` in the dev's shell (e.g. corp-network
    /// setups). Without this, `Socks5Server::bind` would snapshot the
    /// env var and chain 127.0.0.1 connections through the corp proxy,
    /// returning failures that look like product bugs but aren't. See
    /// the doc comment on [`Socks5Server::with_upstream`]. Any future
    /// socks5 test that needs a non-`Direct` upstream should call
    /// `Socks5Server::bind(...).with_upstream(...)` directly instead
    /// of this helper.
    async fn spawn(filter: Filter) -> (u16, tokio::task::JoinHandle<()>) {
        let server = Socks5Server::bind(None, filter)
            .await
            .unwrap()
            .with_upstream(crate::proxy::upstream::UpstreamConfig::Direct);
        let port = server.port();
        let task = tokio::spawn(server.serve());
        (port, task)
    }

    /// Greet and read the server's method-selection reply.
    async fn greet_noauth(sock: &mut TcpStream) -> [u8; 2] {
        sock.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut reply = [0u8; 2];
        sock.read_exact(&mut reply).await.unwrap();
        reply
    }

    /// Send a CONNECT-DOMAIN request and read back the 4-byte reply
    /// header (VER, REP, RSV, BND.ATYP).
    async fn connect_domain(sock: &mut TcpStream, host: &str, port: u16) -> [u8; 4] {
        let mut req = vec![0x05, 0x01, 0x00, 0x03, host.len() as u8];
        req.extend_from_slice(host.as_bytes());
        req.extend_from_slice(&port.to_be_bytes());
        sock.write_all(&req).await.unwrap();
        let mut reply = [0u8; 4];
        sock.read_exact(&mut reply).await.unwrap();
        reply
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn greeting_accepts_noauth() {
        let (port, _task) = spawn(Filter::default()).await;
        let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        assert_eq!(greet_noauth(&mut sock).await, [0x05, 0x00]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn greeting_rejects_no_acceptable_auth() {
        let (port, _task) = spawn(Filter::default()).await;
        let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        // Offer only username/password (0x02) — we only support 0x00.
        sock.write_all(&[0x05, 0x01, 0x02]).await.unwrap();
        let mut reply = [0u8; 2];
        sock.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [0x05, 0xFF]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connect_domain_blocked_by_filter() {
        let (port, _task) = spawn(Filter::new(["github.com"]).unwrap()).await;
        let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let _ = greet_noauth(&mut sock).await;
        let reply = connect_domain(&mut sock, "evil.example.com", 443).await;
        assert_eq!(reply[0], 0x05);
        assert_eq!(reply[1], REP_NOT_ALLOWED);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ipv4_literal_rejected_with_atyp_unsupported() {
        // IP literal bypasses hostname filter, so we hard-refuse.
        let (port, _task) = spawn(Filter::new(["github.com"]).unwrap()).await;
        let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let _ = greet_noauth(&mut sock).await;
        // CONNECT 1.2.3.4:443 with ATYP=0x01.
        sock.write_all(&[0x05, 0x01, 0x00, 0x01, 1, 2, 3, 4, 0x01, 0xBB])
            .await
            .unwrap();
        let mut reply = [0u8; 4];
        sock.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[0], 0x05);
        assert_eq!(reply[1], REP_ADDRESS_TYPE_NOT_SUPPORTED);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ipv6_literal_rejected_with_atyp_unsupported() {
        let (port, _task) = spawn(Filter::new(["github.com"]).unwrap()).await;
        let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let _ = greet_noauth(&mut sock).await;
        let mut req = vec![0x05, 0x01, 0x00, 0x04];
        req.extend_from_slice(&[0u8; 16]);
        req.extend_from_slice(&443u16.to_be_bytes());
        sock.write_all(&req).await.unwrap();
        let mut reply = [0u8; 4];
        sock.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[1], REP_ADDRESS_TYPE_NOT_SUPPORTED);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bind_command_refused() {
        let (port, _task) = spawn(Filter::default()).await;
        let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let _ = greet_noauth(&mut sock).await;
        // CMD=0x02 (BIND), ATYP=0x03 (DOMAIN), 1-byte name "x", port 80.
        sock.write_all(&[0x05, 0x02, 0x00, 0x03, 0x01, b'x', 0x00, 0x50])
            .await
            .unwrap();
        let mut reply = [0u8; 4];
        sock.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[1], REP_COMMAND_NOT_SUPPORTED);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn allowed_domain_succeeds_then_relays() {
        // Stand up a tiny upstream that echoes one byte.
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = upstream.accept().await.unwrap();
            let mut byte = [0u8; 1];
            s.read_exact(&mut byte).await.unwrap();
            s.write_all(&byte).await.unwrap();
        });

        // Allow `localhost` so the filter passes for our upstream.
        let (port, _task) = spawn(Filter::new(["localhost"]).unwrap()).await;
        let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let _ = greet_noauth(&mut sock).await;
        let reply = connect_domain(&mut sock, "localhost", upstream_port).await;
        assert_eq!(reply[1], REP_SUCCEEDED);

        // Skip BND.ADDR + BND.PORT (we send DOMAIN with len=0, so 1 + 2).
        let mut tail = [0u8; 3];
        sock.read_exact(&mut tail).await.unwrap();

        sock.write_all(&[0x42]).await.unwrap();
        let mut echo = [0u8; 1];
        sock.read_exact(&mut echo).await.unwrap();
        assert_eq!(echo[0], 0x42);
    }

    #[test]
    fn io_error_mapping_picks_sensible_codes() {
        let refused = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        assert_eq!(io_error_to_rep(&refused), REP_CONNECTION_REFUSED);

        let other = std::io::Error::other("network is unreachable");
        assert_eq!(io_error_to_rep(&other), REP_NETWORK_UNREACHABLE);

        let mystery = std::io::Error::other("kaboom");
        assert_eq!(io_error_to_rep(&mystery), REP_GENERAL_FAILURE);
    }
}
