//! Built-in HTTP CONNECT proxy server (Phase 3b of #934).
//!
//! Implements the *minimum* HTTP/1.1 surface needed for the CONNECT
//! method — the verb that every modern HTTP client uses to tunnel
//! HTTPS traffic through a forward proxy. After the tunnel is
//! established the server stops being an HTTP server and becomes a
//! plain bidirectional TCP relay.
//!
//! ## Why CONNECT-only
//!
//! Plaintext HTTP forward proxying (`GET http://foo/...` arriving at
//! the proxy) requires parsing HTTP/1.1 requests and rewriting them.
//! That's brittle and security-sensitive. Modern dev tools all use
//! HTTPS, which means CONNECT only. Plaintext forward proxying is
//! intentionally unsupported — we send `405 Method Not Allowed` for
//! any non-CONNECT verb.
//!
//! ## Filter contract
//!
//! Every CONNECT target host is checked against a [`super::Filter`].
//! Reject → `403 Forbidden`. Allow → connect upstream, send `200
//! Connection Established`, splice. Same model as Squid, mitmproxy,
//! Burp Suite.
//!
//! ## What's not here
//!
//! - **No TLS interception (MITM)** — Phase 3d. Today the proxy never
//!   sees the cleartext payload; it just blindly relays bytes.
//! - **No HTTP/2 / QUIC** — out of scope. Curl / Node / Python / Go
//!   all degrade cleanly to HTTP/1.1 CONNECT when `HTTPS_PROXY` is set.
//! - **No SOCKS5** — Phase 3d (separate ≤200-LOC module).
//! - **No upload/idle timeouts** — Phase 3d (resource limits).

use super::Filter;
#[cfg(test)]
use super::pick_ephemeral_port;
use super::relay;
use anyhow::{Context, Result, bail};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, warn};

/// Maximum bytes we'll read while parsing the CONNECT request line +
/// headers. 8 KiB is what nginx ships with by default; bigger than
/// any legitimate CONNECT request will ever be.
const MAX_REQUEST_BYTES: usize = 8 * 1024;

/// How long to wait for the client to finish sending its request
/// headers. Generous default — the request is tiny and arrives in
/// one TCP segment.
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Built-in HTTP CONNECT proxy.
///
/// Cheap-to-construct. Holds a [`TcpListener`] and a [`Filter`]; spawn
/// it onto a runtime via [`Server::serve`].
#[derive(Debug)]
pub struct Server {
    listener: TcpListener,
    filter: Filter,
    port: u16,
    /// Upstream-connection policy snapshotted from the koda process's
    /// env at bind time (Phase 3d.3 of #934). `Direct` in plain dev
    /// environments; `HttpProxy` when the user has corp-set
    /// `HTTPS_PROXY` so we can chain through Zscaler / Squid / etc.
    upstream: crate::proxy::upstream::UpstreamConfig,
}

impl Server {
    /// Bind on `127.0.0.1:port` (or an ephemeral port if `port` is `None`).
    ///
    /// Returns immediately with a configured server. Call [`Self::serve`]
    /// to start accepting connections.
    pub async fn bind(port: Option<u16>, filter: Filter) -> Result<Self> {
        // Phase 3c drive-by: bind directly with port `0` for the
        // ephemeral case instead of going through `pick_ephemeral_port`
        // (which has a TOCTOU race — another process can grab the port
        // between drop and re-bind, and it intermittently does in CI /
        // parallel test runs). Letting the kernel pick + keep the port
        // in one syscall closes the race entirely.
        let bind_port = port.unwrap_or(0);
        let listener = TcpListener::bind(("127.0.0.1", bind_port))
            .await
            .with_context(|| format!("bind built-in proxy on 127.0.0.1:{bind_port}"))?;
        let actual = listener
            .local_addr()
            .context("read local_addr from listener")?
            .port();
        debug!(
            "built-in proxy bound: port={} filter_size={}",
            actual,
            filter.len()
        );
        let upstream = crate::proxy::upstream::UpstreamConfig::from_env();
        if !matches!(upstream, crate::proxy::upstream::UpstreamConfig::Direct) {
            debug!("built-in proxy chaining upstream via {upstream:?}");
        }
        Ok(Self {
            listener,
            filter,
            port: actual,
            upstream,
        })
    }

    /// Port the server is listening on. Useful when [`Self::bind`] was
    /// called with `None` and the caller wants the ephemeral port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Override the upstream-connection policy that [`Self::bind`]
    /// snapshotted from `HTTPS_PROXY`. Used by tests that need a
    /// deterministic upstream config without racing on the process
    /// env (which is shared across parallel `cargo test` runs).
    /// Production code should rely on the env snapshot.
    pub fn with_upstream(mut self, upstream: crate::proxy::upstream::UpstreamConfig) -> Self {
        self.upstream = upstream;
        self
    }

    /// Run the accept loop forever.
    ///
    /// Each accepted connection is dispatched to a tokio task so a
    /// slow upstream doesn't head-of-line block the listener. The loop
    /// itself only stops on a fatal accept error (ENFILE, etc.) — for
    /// graceful shutdown the caller drops the [`Server`] (or its
    /// containing JoinHandle is aborted).
    pub async fn serve(self) {
        let filter = self.filter;
        let upstream = std::sync::Arc::new(self.upstream);
        loop {
            let (sock, peer) = match self.listener.accept().await {
                Ok(t) => t,
                Err(e) => {
                    warn!("built-in proxy accept failed: {e}");
                    return;
                }
            };
            let f = filter.clone();
            let up = std::sync::Arc::clone(&upstream);
            tokio::spawn(async move {
                if let Err(e) = handle_one(sock, &f, &up).await {
                    debug!("proxy connection from {peer} ended: {e:#}");
                }
            });
        }
    }
}

/// Handle a single client connection: parse CONNECT, filter, splice.
///
/// Returns `Ok(())` for the happy path AND for cleanly-rejected requests
/// (403/405) — the `Err` return is reserved for socket/IO failures the
/// caller can't do anything about beyond logging.
async fn handle_one(
    mut client: TcpStream,
    filter: &Filter,
    upstream: &crate::proxy::upstream::UpstreamConfig,
) -> Result<()> {
    let req = read_request(&mut client).await?;
    let (method, target) = parse_request_line(&req)?;

    if method != "CONNECT" {
        // 405: we only do tunnel mode. See module docs.
        write_status(&mut client, 405, "Method Not Allowed").await?;
        return Ok(());
    }

    if !filter.allows(&target) {
        write_status(&mut client, 403, "Forbidden").await?;
        debug!("proxy: blocked CONNECT {target} (not in allowlist)");
        return Ok(());
    }

    // Allowed. Connect upstream and bridge.
    let upstream_sock = match crate::proxy::upstream::connect_upstream(&target, upstream).await {
        Ok(s) => s,
        Err(e) => {
            warn!("proxy: upstream connect to {target} failed: {e:#}");
            write_status(&mut client, 502, "Bad Gateway").await?;
            return Ok(());
        }
    };

    write_status(&mut client, 200, "Connection Established").await?;

    // Bidirectional copy with idle + total timeouts (Phase 3f of
    // #934). We use [`crate::proxy::relay`] instead of tokio's
    // copy_bidirectional so a wedged peer (corp middlebox eating
    // RST, NAT silently dropping) can't pin a task slot for the
    // kernel keepalive's two-hour default. Errors are intentionally
    // swallowed: the connection is already torn down by the time we
    // see them, and surfacing them as proxy-side log spam would just
    // be noise — clients can't act on it.
    let _ = relay::relay_with_timeouts(
        client,
        upstream_sock,
        relay::DEFAULT_IDLE_TIMEOUT,
        relay::DEFAULT_TOTAL_TIMEOUT,
    )
    .await;
    Ok(())
}

/// Read the CONNECT request line + headers (everything up to the
/// first CRLF CRLF). Cap the read at [`MAX_REQUEST_BYTES`] to prevent
/// a slow-loris-style memory-bomb attack.
async fn read_request(client: &mut TcpStream) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(512);
    let read = tokio::time::timeout(REQUEST_READ_TIMEOUT, async {
        let mut chunk = [0u8; 1024];
        loop {
            let n = client.read(&mut chunk).await?;
            if n == 0 {
                bail!("client closed before sending request");
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                return Ok::<_, anyhow::Error>(());
            }
            if buf.len() > MAX_REQUEST_BYTES {
                bail!("request headers exceed {} bytes", MAX_REQUEST_BYTES);
            }
        }
    })
    .await;

    match read {
        Ok(Ok(())) => Ok(buf),
        Ok(Err(e)) => Err(e),
        Err(_) => bail!("client request timed out after {REQUEST_READ_TIMEOUT:?}"),
    }
}

/// Parse `METHOD TARGET HTTP/x.y\r\n` and return `(method, target)`.
///
/// Headers after the request line are ignored — for CONNECT there's
/// nothing useful in them (Host: is just a duplicate of the target).
fn parse_request_line(req: &[u8]) -> Result<(String, String)> {
    let line_end = req
        .windows(2)
        .position(|w| w == b"\r\n")
        .context("malformed request: no CRLF after request line")?;
    let line = std::str::from_utf8(&req[..line_end])
        .context("malformed request: request line not UTF-8")?;
    let mut parts = line.splitn(3, ' ');
    let method = parts
        .next()
        .context("malformed request: missing method")?
        .to_string();
    let target = parts
        .next()
        .context("malformed request: missing target")?
        .to_string();
    let version = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/") {
        bail!("malformed request: unexpected version {version:?}");
    }
    Ok((method, target))
}

/// Write `HTTP/1.1 <code> <reason>\r\n\r\n`. Best-effort; failure to
/// write means the client already gave up on us.
async fn write_status(client: &mut TcpStream, code: u16, reason: &str) -> Result<()> {
    let resp = format!("HTTP/1.1 {code} {reason}\r\n\r\n");
    client
        .write_all(resp.as_bytes())
        .await
        .with_context(|| format!("write {code} response"))?;
    client.flush().await.ok();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener as StdTcpListener;

    /// Spawn a fake upstream HTTP server that responds with a fixed body
    /// to any TCP connect. Used for end-to-end CONNECT-then-payload tests.
    /// Returns (port, JoinHandle).
    async fn fake_upstream(body: &'static str) -> (u16, SocketAddr) {
        let l = StdTcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = l.accept().await {
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (addr.port(), addr)
    }

    /// Open a CONNECT request to the proxy, return (response_status_line,
    /// response_full_until_close). The response_full_until_close lets us
    /// check both the headers and any tunneled payload.
    async fn do_connect(proxy_port: u16, target: &str) -> (String, Vec<u8>) {
        let mut sock = TcpStream::connect(("127.0.0.1", proxy_port)).await.unwrap();
        let req = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n");
        sock.write_all(req.as_bytes()).await.unwrap();

        let (r, _w) = sock.split();
        let mut reader = BufReader::new(r);
        let mut status_line = String::new();
        reader.read_line(&mut status_line).await.unwrap();

        let mut rest = Vec::new();
        let _ = reader.read_to_end(&mut rest).await;
        (status_line, rest)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn server_bind_uses_ephemeral_port_when_none() {
        let s = Server::bind(None, Filter::default()).await.unwrap();
        let p = s.port();
        assert!(p > 0, "ephemeral port must be non-zero");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rejects_non_connect_with_405() {
        let server = Server::bind(None, Filter::new(["github.com"]).unwrap())
            .await
            .unwrap();
        let port = server.port();
        tokio::spawn(server.serve());

        let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        sock.write_all(b"GET / HTTP/1.1\r\nHost: foo\r\n\r\n")
            .await
            .unwrap();

        let mut buf = String::new();
        BufReader::new(sock).read_line(&mut buf).await.unwrap();
        assert!(buf.starts_with("HTTP/1.1 405"), "got: {buf:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rejects_disallowed_host_with_403() {
        let server = Server::bind(None, Filter::new(["github.com"]).unwrap())
            .await
            .unwrap();
        let port = server.port();
        tokio::spawn(server.serve());

        let (status, _) = do_connect(port, "evil.example.com:443").await;
        assert!(status.starts_with("HTTP/1.1 403"), "got: {status:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn allows_listed_host_and_tunnels_payload() {
        // The payload our fake upstream sends; the test asserts the
        // proxy relays it byte-for-byte to us through the tunnel.
        let payload = "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        let (up_port, _up_addr) = fake_upstream(payload).await;

        // Allowlist the upstream's loopback "host". 127.0.0.1 is an exact
        // match in the filter — works because the filter is just doing
        // string matching on the host part of "host:port".
        //
        // `.with_upstream(Direct)` shields the test from any ambient
        // `HTTPS_PROXY` in the dev's shell (e.g. corp-network setups).
        // Without it, `Server::bind` snapshots the env var and the
        // proxy under test tries to chain its 127.0.0.1 connection
        // through the corp proxy, returning 502. See the doc comment
        // on `Self::with_upstream`.
        let server = Server::bind(None, Filter::new(["127.0.0.1"]).unwrap())
            .await
            .unwrap()
            .with_upstream(crate::proxy::upstream::UpstreamConfig::Direct);
        let proxy_port = server.port();
        tokio::spawn(server.serve());

        let (status, body) = do_connect(proxy_port, &format!("127.0.0.1:{up_port}")).await;
        assert!(status.starts_with("HTTP/1.1 200"), "got: {status:?}");
        // Skip the empty-line CRLF after the proxy's 200 response; the
        // remaining bytes should be exactly what the upstream sent.
        let body_str = String::from_utf8_lossy(&body);
        let body_str = body_str.trim_start_matches("\r\n");
        assert_eq!(body_str, payload, "tunneled body mismatch");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn returns_502_when_upstream_unreachable() {
        // Allowlist contains the host but the port is dead.
        let server = Server::bind(None, Filter::new(["127.0.0.1"]).unwrap())
            .await
            .unwrap();
        let proxy_port = server.port();
        tokio::spawn(server.serve());

        // Pick a port nobody's listening on (use pick_ephemeral_port +
        // immediately drop, then race the connect — same TOCTOU caveat
        // as elsewhere; in practice the kernel won't reassign in the
        // single-digit microseconds between drop and our connect).
        let dead = pick_ephemeral_port().unwrap();
        let (status, _) = do_connect(proxy_port, &format!("127.0.0.1:{dead}")).await;
        assert!(status.starts_with("HTTP/1.1 502"), "got: {status:?}");
    }

    /// Phase 3d.3: when configured with an HttpProxy upstream, the
    /// CONNECT must be tunnelled through that proxy rather than dialed
    /// directly. We stand up a fake corp proxy that records the
    /// CONNECT it receives, then chain through it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn chains_through_upstream_http_proxy() {
        // Real upstream that the corp proxy will dial on our behalf.
        let payload = "PAYLOAD-FROM-REAL-UPSTREAM";
        let (real_port, _) = fake_upstream(payload).await;

        // Fake corp proxy: accept CONNECT, record the target, dial it,
        // reply 200, splice. Minimal logic but exercises the same wire
        // format real corp proxies (Squid/Zscaler) speak.
        let connect_log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let corp_listener = StdTcpListener::bind("127.0.0.1:0").await.unwrap();
        let corp_port = corp_listener.local_addr().unwrap().port();
        let log_for_corp = std::sync::Arc::clone(&connect_log);
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = corp_listener.accept().await {
                let log = std::sync::Arc::clone(&log_for_corp);
                tokio::spawn(async move {
                    // Read just the CONNECT request line.
                    let mut reader = BufReader::new(&mut sock);
                    let mut req = String::new();
                    reader.read_line(&mut req).await.unwrap();
                    // Drain headers up to the empty line.
                    loop {
                        let mut line = String::new();
                        let n = reader.read_line(&mut line).await.unwrap();
                        if n == 0 || line == "\r\n" || line == "\n" {
                            break;
                        }
                    }
                    // Parse the target out of "CONNECT host:port HTTP/1.1".
                    let target = req.split_whitespace().nth(1).unwrap().to_string();
                    log.lock().unwrap().push(target.clone());
                    // Dial the real target on behalf of the client.
                    let mut up = TcpStream::connect(&target).await.unwrap();
                    sock.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                        .await
                        .unwrap();
                    let _ = tokio::io::copy_bidirectional(&mut sock, &mut up).await;
                });
            }
        });

        // Stand up our proxy and inject the corp proxy as upstream.
        let server = Server::bind(None, Filter::new(["127.0.0.1"]).unwrap())
            .await
            .unwrap()
            .with_upstream(crate::proxy::upstream::UpstreamConfig::HttpProxy {
                host: "127.0.0.1".into(),
                port: corp_port,
                no_proxy: vec![],
            });
        let proxy_port = server.port();
        tokio::spawn(server.serve());

        let target = format!("127.0.0.1:{real_port}");
        let (status, body) = do_connect(proxy_port, &target).await;
        assert!(status.starts_with("HTTP/1.1 200"), "got: {status:?}");
        let body_str = String::from_utf8_lossy(&body);
        let body_str = body_str.trim_start_matches("\r\n");
        assert_eq!(body_str, payload, "tunneled body mismatch");

        // The corp proxy must have seen exactly the original target —
        // no rewriting, no leakage of internal request structure.
        let log = connect_log.lock().unwrap();
        assert_eq!(*log, vec![target], "corp proxy must see original target");
    }

    /// Phase 3d.3: NO_PROXY entries take a host out of the chained
    /// upstream and back onto the direct path. Critical because
    /// `*.walmart.com` (and friends) are corp-internal and routing
    /// them through Zscaler produces 403s.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn chains_skips_upstream_for_no_proxy_hosts() {
        let payload = "DIRECT-NOT-CHAINED";
        let (real_port, _) = fake_upstream(payload).await;

        // Bind a corp proxy that will NEVER receive a connection —
        // we'll fail the test if it does. Don't even spawn an accept
        // loop: an unbound port would be the cleanest signal, but the
        // upstream layer treats "connect refused" as a real failure
        // (502 to client) and would mask a NO_PROXY bug. Instead bind
        // and assert no accepts happen by counting through a channel.
        let corp_listener = StdTcpListener::bind("127.0.0.1:0").await.unwrap();
        let corp_port = corp_listener.local_addr().unwrap().port();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        tokio::spawn(async move {
            while corp_listener.accept().await.is_ok() {
                let _ = tx.send(());
            }
        });

        let server = Server::bind(None, Filter::new(["127.0.0.1"]).unwrap())
            .await
            .unwrap()
            .with_upstream(crate::proxy::upstream::UpstreamConfig::HttpProxy {
                host: "127.0.0.1".into(),
                port: corp_port,
                no_proxy: vec!["127.0.0.1".into()],
            });
        let proxy_port = server.port();
        tokio::spawn(server.serve());

        let (status, body) = do_connect(proxy_port, &format!("127.0.0.1:{real_port}")).await;
        assert!(status.starts_with("HTTP/1.1 200"), "got: {status:?}");
        let body_str = String::from_utf8_lossy(&body);
        let body_str = body_str.trim_start_matches("\r\n");
        assert_eq!(body_str, payload);

        // Give the accept loop a moment; corp proxy must not have been
        // contacted because 127.0.0.1 is in NO_PROXY.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            rx.try_recv().is_err(),
            "NO_PROXY-listed host must bypass upstream proxy"
        );
    }

    /// Phase 3d.3: a chained upstream that rejects with 407 (auth
    /// required) must surface as 502 to the client — we have no
    /// credentials to retry with, and silently sending the request
    /// direct would defeat the point of the corp policy.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn surfaces_upstream_407_as_502() {
        let corp_listener = StdTcpListener::bind("127.0.0.1:0").await.unwrap();
        let corp_port = corp_listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = corp_listener.accept().await {
                tokio::spawn(async move {
                    // Drain request, reply 407, hang up.
                    let mut buf = [0u8; 1024];
                    let _ = sock.read(&mut buf).await;
                    let _ = sock
                        .write_all(
                            b"HTTP/1.1 407 Proxy Authentication Required\r\n\
                              Proxy-Authenticate: Basic\r\n\r\n",
                        )
                        .await;
                    let _ = sock.shutdown().await;
                });
            }
        });

        let server = Server::bind(None, Filter::new(["127.0.0.1"]).unwrap())
            .await
            .unwrap()
            .with_upstream(crate::proxy::upstream::UpstreamConfig::HttpProxy {
                host: "127.0.0.1".into(),
                port: corp_port,
                no_proxy: vec![],
            });
        let proxy_port = server.port();
        tokio::spawn(server.serve());

        let (status, _) = do_connect(proxy_port, "127.0.0.1:1").await;
        assert!(
            status.starts_with("HTTP/1.1 502"),
            "upstream auth failure must surface as 502, got: {status:?}"
        );
    }

    // ── parse_request_line ──────────────────────────────────────────────

    #[test]
    fn parse_connect_request() {
        let req = b"CONNECT github.com:443 HTTP/1.1\r\nHost: github.com:443\r\n\r\n";
        let (m, t) = parse_request_line(req).unwrap();
        assert_eq!(m, "CONNECT");
        assert_eq!(t, "github.com:443");
    }

    #[test]
    fn parse_get_request() {
        let req = b"GET /index HTTP/1.1\r\n\r\n";
        let (m, t) = parse_request_line(req).unwrap();
        assert_eq!(m, "GET");
        assert_eq!(t, "/index");
    }

    #[test]
    fn parse_rejects_no_crlf() {
        let req = b"CONNECT github.com:443 HTTP/1.1";
        let err = parse_request_line(req).expect_err("must reject");
        assert!(err.to_string().contains("no CRLF"));
    }

    #[test]
    fn parse_rejects_missing_version() {
        let req = b"CONNECT github.com:443\r\n\r\n";
        let err = parse_request_line(req).expect_err("must reject");
        assert!(err.to_string().contains("unexpected version"));
    }

    #[test]
    fn parse_rejects_bad_version() {
        let req = b"CONNECT github.com:443 SPDY/1\r\n\r\n";
        let err = parse_request_line(req).expect_err("must reject");
        assert!(err.to_string().contains("unexpected version"));
    }

    #[test]
    fn parse_rejects_non_utf8() {
        let req = &[
            0xff, 0xff, b' ', b'/', b' ', b'H', b'T', b'T', b'P', b'\r', b'\n',
        ];
        let err = parse_request_line(req).expect_err("must reject");
        assert!(err.to_string().contains("not UTF-8"));
    }
}
