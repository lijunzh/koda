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

use super::{Filter, pick_ephemeral_port};
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

/// How long to wait for the upstream TCP handshake. Anything past
/// this and the host is effectively unreachable.
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Built-in HTTP CONNECT proxy.
///
/// Cheap-to-construct. Holds a [`TcpListener`] and a [`Filter`]; spawn
/// it onto a runtime via [`Server::serve`].
#[derive(Debug)]
pub struct Server {
    listener: TcpListener,
    filter: Filter,
    port: u16,
}

impl Server {
    /// Bind on `127.0.0.1:port` (or an ephemeral port if `port` is `None`).
    ///
    /// Returns immediately with a configured server. Call [`Self::serve`]
    /// to start accepting connections.
    pub async fn bind(port: Option<u16>, filter: Filter) -> Result<Self> {
        let port = match port {
            Some(p) => p,
            None => pick_ephemeral_port().context("pick ephemeral port for built-in proxy")?,
        };
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .with_context(|| format!("bind built-in proxy on 127.0.0.1:{port}"))?;
        let actual = listener
            .local_addr()
            .context("read local_addr from listener")?
            .port();
        debug!(
            "built-in proxy bound: port={} filter_size={}",
            actual,
            filter.len()
        );
        Ok(Self {
            listener,
            filter,
            port: actual,
        })
    }

    /// Port the server is listening on. Useful when [`Self::bind`] was
    /// called with `None` and the caller wants the ephemeral port.
    pub fn port(&self) -> u16 {
        self.port
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
        loop {
            let (sock, peer) = match self.listener.accept().await {
                Ok(t) => t,
                Err(e) => {
                    warn!("built-in proxy accept failed: {e}");
                    return;
                }
            };
            let f = filter.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_one(sock, &f).await {
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
async fn handle_one(mut client: TcpStream, filter: &Filter) -> Result<()> {
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
    let mut upstream = match connect_upstream(&target).await {
        Ok(s) => s,
        Err(e) => {
            warn!("proxy: upstream connect to {target} failed: {e:#}");
            write_status(&mut client, 502, "Bad Gateway").await?;
            return Ok(());
        }
    };

    write_status(&mut client, 200, "Connection Established").await?;

    // Bidirectional copy until either side closes. tokio's helper does
    // the half-close dance correctly: when one direction reports EOF
    // it shuts down the corresponding write half on the other socket
    // so the peer can observe the close and tear down. Without this
    // (e.g. naive tokio::join! of two copy()s) we'd deadlock — clients
    // typically never close their write side after CONNECT.
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
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

/// `target` is `host:port` per the CONNECT spec (RFC 9110 §9.3.6).
async fn connect_upstream(target: &str) -> Result<TcpStream> {
    let stream = tokio::time::timeout(UPSTREAM_CONNECT_TIMEOUT, TcpStream::connect(target))
        .await
        .with_context(|| format!("upstream connect to {target} timed out"))?
        .with_context(|| format!("upstream connect to {target} failed"))?;
    Ok(stream)
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

    #[tokio::test]
    async fn server_bind_uses_ephemeral_port_when_none() {
        let s = Server::bind(None, Filter::default()).await.unwrap();
        let p = s.port();
        assert!(p > 0, "ephemeral port must be non-zero");
    }

    #[tokio::test]
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

    #[tokio::test]
    async fn rejects_disallowed_host_with_403() {
        let server = Server::bind(None, Filter::new(["github.com"]).unwrap())
            .await
            .unwrap();
        let port = server.port();
        tokio::spawn(server.serve());

        let (status, _) = do_connect(port, "evil.example.com:443").await;
        assert!(status.starts_with("HTTP/1.1 403"), "got: {status:?}");
    }

    #[tokio::test]
    async fn allows_listed_host_and_tunnels_payload() {
        // The payload our fake upstream sends; the test asserts the
        // proxy relays it byte-for-byte to us through the tunnel.
        let payload = "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        let (up_port, _up_addr) = fake_upstream(payload).await;

        // Allowlist the upstream's loopback "host". 127.0.0.1 is an exact
        // match in the filter — works because the filter is just doing
        // string matching on the host part of "host:port".
        let server = Server::bind(None, Filter::new(["127.0.0.1"]).unwrap())
            .await
            .unwrap();
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

    #[tokio::test]
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
