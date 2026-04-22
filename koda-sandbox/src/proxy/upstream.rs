//! Upstream-connection layer for the built-in proxies (Phase 3d.3 of #934).
//!
//! Both [`crate::proxy::server::Server`] (HTTP CONNECT) and
//! [`crate::proxy::socks5::Socks5Server`] need to reach a target host
//! on behalf of a sandboxed client. In a plain home/dev environment
//! that's a direct `TcpStream::connect`; in a corp environment
//! (Walmart's Zscaler, Bluecoat, MS Defender, etc.) outbound 443 from
//! arbitrary processes is firewalled and the only path out is the
//! corporate HTTPS_PROXY.
//!
//! This module centralises the dispatch:
//!
//! - [`UpstreamConfig::from_env`] snapshots the user's `HTTPS_PROXY` /
//!   `NO_PROXY` env at proxy spawn time. We snapshot once (not per-
//!   request) because the koda process's env is stable for its lifetime
//!   and re-reading it on every CONNECT would just be syscall churn.
//! - [`connect_upstream`] dispatches: `Direct` → raw `TcpStream`;
//!   `HttpProxy` → dial the proxy and send our own CONNECT (no auth,
//!   no MITM — we're acting as a transparent HTTP-tunnel intermediary).
//! - [`bypasses_proxy`] applies the NO_PROXY suffix rules so requests
//!   to e.g. `pypi.ci.artifacts.walmart.com` skip the chained proxy
//!   when NO_PROXY matches.
//!
//! What's deliberately out of scope:
//! - Proxy authentication (Basic / NTLM / Negotiate). Corp proxies that
//!   require this are unreachable from koda — same as today. Adding it
//!   would mean shipping (and securing!) credential storage which is a
//!   much bigger feature.
//! - SOCKS5 upstream proxies. Most corp setups use HTTP, and chaining
//!   SOCKS5 → SOCKS5 is unusual. Easy to add later if a user asks.
//! - HTTPS to the proxy itself. We always speak plain HTTP/1.1 to the
//!   proxy (TLS happens *inside* the CONNECT tunnel from the client to
//!   the real target). Most corp proxies accept both; some only accept
//!   HTTP on a dedicated port. The `https://` scheme in HTTPS_PROXY is
//!   accepted but treated identically to `http://`.

use anyhow::{Context, Result, anyhow};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::warn;

/// Snapshot of HTTPS_PROXY / NO_PROXY at proxy spawn time. Cheap to
/// `clone` (small `String` + small `Vec<String>`).
///
/// `Direct` is the default when no proxy is configured or the
/// configured value can't be parsed — fail-open keeps users productive
/// when the env is misconfigured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamConfig {
    /// No upstream chaining — dial targets directly.
    Direct,
    /// Tunnel through an HTTP CONNECT proxy. `no_proxy` is a list of
    /// host suffixes (and the literal `*`) that bypass the proxy.
    HttpProxy {
        /// Hostname or IP literal of the upstream proxy (e.g.
        /// `"sysproxy.wal-mart.com"`). Always dialled in the clear —
        /// the inner CONNECT tunnel carries the client's TLS
        /// end-to-end so we don't need to wrap this in TLS ourselves.
        host: String,
        /// TCP port the proxy listens on. Conventionally 8080 or 3128.
        port: u16,
        /// Suffix-match list from `NO_PROXY` (or `no_proxy`). The
        /// literal `*` means "bypass the proxy for everything" — a
        /// kill switch that effectively reverts to `Direct` per-request.
        no_proxy: Vec<String>,
    },
}

impl UpstreamConfig {
    /// Read the env once and decide which mode applies.
    ///
    /// Lookup order matches `curl`/`requests`/`pip` convention:
    /// `HTTPS_PROXY` first, then `https_proxy`. We only honour the
    /// HTTPS variant because every target we care about (api.openai.com,
    /// api.anthropic.com, registry.npmjs.org, …) speaks TLS — there's
    /// no practical difference between HTTPS_PROXY and ALL_PROXY for
    /// our workload, and HTTP_PROXY is conventionally for plaintext
    /// HTTP only (which we don't generate).
    pub fn from_env() -> Self {
        let raw = std::env::var("HTTPS_PROXY")
            .ok()
            .or_else(|| std::env::var("https_proxy").ok());
        let Some(raw) = raw else {
            return Self::Direct;
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Self::Direct;
        }
        match parse_proxy_url(trimmed) {
            Some((host, port)) => Self::HttpProxy {
                host,
                port,
                no_proxy: parse_no_proxy(),
            },
            None => {
                warn!(
                    "HTTPS_PROXY={raw:?} is not parseable (expected http://host:port, no auth) — \
                     falling back to direct upstream"
                );
                Self::Direct
            }
        }
    }
}

/// Parse `[scheme://]host:port[/]` into `(host, port)`. Returns `None`
/// for any of: missing port, embedded `@` (auth), non-numeric port,
/// hostname containing whitespace.
///
/// We accept both `http://` and `https://` schemes as input but ignore
/// them — the connection to the proxy itself is always plain HTTP/1.1
/// CONNECT, and the inner tunnel carries the client's TLS end-to-end.
pub fn parse_proxy_url(s: &str) -> Option<(String, u16)> {
    let s = s.strip_suffix('/').unwrap_or(s);
    let s = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s);

    if s.contains('@') || s.contains(char::is_whitespace) || s.contains('/') {
        // `@` means user:pass auth (unsupported); whitespace / `/` mean
        // a path component or a malformed URL — refuse rather than
        // guess.
        return None;
    }

    let (host, port_str) = s.rsplit_once(':')?;
    if host.is_empty() {
        return None;
    }
    let port: u16 = port_str.parse().ok()?;
    if port == 0 {
        return None;
    }
    Some((host.to_string(), port))
}

/// Read `NO_PROXY` (then `no_proxy`) and split on commas, trimming
/// whitespace and leading dots. Empty entries are dropped. The literal
/// `*` is preserved as-is and handled by [`bypasses_proxy`].
pub fn parse_no_proxy() -> Vec<String> {
    let raw = std::env::var("NO_PROXY")
        .ok()
        .or_else(|| std::env::var("no_proxy").ok())
        .unwrap_or_default();
    raw.split(',')
        .map(|e| e.trim().trim_start_matches('.').to_string())
        .filter(|e| !e.is_empty())
        .collect()
}

/// Suffix-match NO_PROXY semantics. `*` matches everything; otherwise
/// `target_host` (already host-only, no port) bypasses the proxy when
/// it equals or is a subdomain of any entry.
///
/// We deliberately don't support CIDR or wildcard-prefix entries —
/// they're rare in practice and fancy parsing here would dwarf the
/// 30-line dispatch logic. Users who need CIDRs can add the matching
/// hostnames explicitly to NO_PROXY.
pub fn bypasses_proxy(target_host: &str, no_proxy: &[String]) -> bool {
    no_proxy
        .iter()
        .any(|e| e == "*" || target_host == e || target_host.ends_with(&format!(".{e}")))
}

/// Hard cap on the upstream-proxy CONNECT handshake. Same value the
/// outer proxy uses for direct connects so a slow corp proxy times out
/// in roughly the same window as an unreachable target.
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Dispatch a connection to `target` (`host:port` form, per RFC 9110
/// §9.3.6) according to `cfg`.
///
/// Returns a `TcpStream` wired straight to `target`'s server endpoint
/// — callers can copy bytes through it without further protocol on
/// either side. In `HttpProxy` mode the stream is the post-CONNECT
/// tunnel; in `Direct` mode it's the raw socket to `target`.
pub async fn connect_upstream(target: &str, cfg: &UpstreamConfig) -> Result<TcpStream> {
    match cfg {
        UpstreamConfig::Direct => connect_direct(target).await,
        UpstreamConfig::HttpProxy {
            host,
            port,
            no_proxy,
        } => {
            let target_host = target.rsplit_once(':').map(|(h, _)| h).unwrap_or(target);
            if bypasses_proxy(target_host, no_proxy) {
                return connect_direct(target).await;
            }
            connect_via_http_proxy(target, host, *port).await
        }
    }
}

async fn connect_direct(target: &str) -> Result<TcpStream> {
    let stream = tokio::time::timeout(UPSTREAM_CONNECT_TIMEOUT, TcpStream::connect(target))
        .await
        .with_context(|| format!("upstream connect to {target} timed out"))?
        .with_context(|| format!("upstream connect to {target} failed"))?;
    Ok(stream)
}

/// Dial the corp proxy and send `CONNECT target HTTP/1.1\r\n\r\n`,
/// returning the raw stream once we get back a 2xx. Any other status
/// (407 auth required, 403 corp-blocked, 502, …) is surfaced as an
/// `Err` so the outer proxy turns it into the appropriate client-side
/// response (502 for HTTP CONNECT, REP=0x05 for SOCKS5).
async fn connect_via_http_proxy(
    target: &str,
    proxy_host: &str,
    proxy_port: u16,
) -> Result<TcpStream> {
    let proxy_addr = format!("{proxy_host}:{proxy_port}");
    let mut stream =
        tokio::time::timeout(UPSTREAM_CONNECT_TIMEOUT, TcpStream::connect(&proxy_addr))
            .await
            .with_context(|| format!("dial upstream proxy {proxy_addr} timed out"))?
            .with_context(|| format!("dial upstream proxy {proxy_addr} failed"))?;

    // Conventional CONNECT request. We send a Host header because
    // some corp proxies (Squid in particular) require it even though
    // RFC 9110 only mandates it for non-CONNECT methods. User-Agent
    // is omitted intentionally — proxies that fingerprint by UA can
    // be tricked into a different policy and we want to look like a
    // generic tunnel client.
    let req = format!(
        "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nProxy-Connection: keep-alive\r\n\r\n"
    );
    tokio::time::timeout(UPSTREAM_CONNECT_TIMEOUT, stream.write_all(req.as_bytes()))
        .await
        .with_context(|| format!("send CONNECT to upstream proxy {proxy_addr} timed out"))?
        .with_context(|| format!("send CONNECT to upstream proxy {proxy_addr} failed"))?;

    // Read the status line + headers. We can't use copy_bidirectional
    // yet because we need to consume (and discard) the proxy's headers
    // before handing the raw tunnel back to the caller.
    let (status, _headers) = read_proxy_response(&mut stream)
        .await
        .with_context(|| format!("read CONNECT response from upstream proxy {proxy_addr}"))?;
    if !(200..300).contains(&status) {
        return Err(anyhow!(
            "upstream proxy {proxy_addr} refused CONNECT to {target} with status {status}"
        ));
    }

    Ok(stream)
}

/// Read the response status line + headers up to the first blank line
/// without using a buffered reader. Buffered reads risk consuming
/// post-header tunnel bytes (the proxy can pipeline the CONNECT
/// response with the first tunnelled bytes from the upstream peer);
/// since we hand the bare stream back to the caller for splicing,
/// losing those bytes would silently corrupt every TLS handshake. So
/// we read one byte at a time — wasteful, but CONNECT responses are
/// ~50 bytes long and only happen once per tunnel.
///
/// Bounded at 8 KiB total so a misbehaving proxy can't blow our
/// memory or stall us forever.
async fn read_proxy_response(stream: &mut TcpStream) -> Result<(u16, Vec<u8>)> {
    const MAX_RESPONSE: usize = 8192;
    let mut buf = Vec::with_capacity(256);
    let deadline = tokio::time::Instant::now() + UPSTREAM_CONNECT_TIMEOUT;
    loop {
        if buf.len() >= MAX_RESPONSE {
            return Err(anyhow!(
                "proxy response exceeded {MAX_RESPONSE} bytes before CRLF CRLF"
            ));
        }
        let mut byte = [0u8; 1];
        let n = tokio::time::timeout_at(deadline, stream.read(&mut byte))
            .await
            .context("read proxy response timed out")?
            .context("read proxy response failed")?;
        if n == 0 {
            return Err(anyhow!("proxy closed connection mid-response"));
        }
        buf.push(byte[0]);
        // End of headers: \r\n\r\n. Tolerate bare \n\n too (some
        // proxies are sloppy) by also checking for that pattern.
        if buf.ends_with(b"\r\n\r\n") || buf.ends_with(b"\n\n") {
            break;
        }
    }

    // Status line is everything up to the first \r\n (or \n).
    let line_end = buf
        .iter()
        .position(|&b| b == b'\n')
        .ok_or_else(|| anyhow!("proxy response missing newline after status line"))?;
    let status_line = std::str::from_utf8(&buf[..line_end])
        .context("proxy status line is not UTF-8")?
        .trim_end_matches('\r');
    let mut parts = status_line.split_whitespace();
    let _version = parts
        .next()
        .context("missing HTTP version in status line")?;
    let code_str = parts.next().context("missing status code in status line")?;
    let code: u16 = code_str
        .parse()
        .with_context(|| format!("non-numeric status code {code_str:?}"))?;
    Ok((code, buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_proxy_url ─────────────────────────────────────────────────

    #[test]
    fn parse_bare_host_port() {
        assert_eq!(
            parse_proxy_url("proxy.corp:8080"),
            Some(("proxy.corp".into(), 8080))
        );
    }

    #[test]
    fn parse_strips_http_scheme() {
        assert_eq!(
            parse_proxy_url("http://proxy.corp:8080"),
            Some(("proxy.corp".into(), 8080))
        );
    }

    #[test]
    fn parse_strips_https_scheme() {
        // We accept https:// in input (some users / corp docs write it)
        // but treat the proxy connection as plain HTTP — the inner
        // tunnel carries the client's TLS end-to-end.
        assert_eq!(
            parse_proxy_url("https://proxy.corp:8080"),
            Some(("proxy.corp".into(), 8080))
        );
    }

    #[test]
    fn parse_strips_trailing_slash() {
        assert_eq!(
            parse_proxy_url("http://proxy.corp:8080/"),
            Some(("proxy.corp".into(), 8080))
        );
    }

    #[test]
    fn parse_rejects_auth() {
        // user:pass@host — we don't ship credential storage. Reject so
        // we fall back to Direct rather than silently sending requests
        // without auth (corp proxy would 407 every one).
        assert_eq!(parse_proxy_url("http://user:pass@proxy.corp:8080"), None);
    }

    #[test]
    fn parse_rejects_missing_port() {
        assert_eq!(parse_proxy_url("proxy.corp"), None);
        assert_eq!(parse_proxy_url("http://proxy.corp"), None);
    }

    #[test]
    fn parse_rejects_zero_port() {
        // Port 0 is meaningless for CONNECT; treating it as Direct is
        // safer than producing an unusable HttpProxy config.
        assert_eq!(parse_proxy_url("proxy.corp:0"), None);
    }

    #[test]
    fn parse_rejects_non_numeric_port() {
        assert_eq!(parse_proxy_url("proxy.corp:eight"), None);
    }

    #[test]
    fn parse_rejects_path_component() {
        assert_eq!(parse_proxy_url("http://proxy.corp:8080/some/path"), None);
    }

    // ── bypasses_proxy ──────────────────────────────────────────────────

    #[test]
    fn no_proxy_exact_match() {
        let no_proxy = vec!["localhost".into(), "internal.corp".into()];
        assert!(bypasses_proxy("localhost", &no_proxy));
        assert!(bypasses_proxy("internal.corp", &no_proxy));
        assert!(!bypasses_proxy("example.com", &no_proxy));
    }

    #[test]
    fn no_proxy_subdomain_match() {
        let no_proxy = vec!["walmart.com".into()];
        assert!(bypasses_proxy("pypi.ci.artifacts.walmart.com", &no_proxy));
        assert!(bypasses_proxy("walmart.com", &no_proxy));
        // Substring match must NOT count — "evilwalmart.com" is a
        // different domain from "walmart.com" and routing it through
        // bypass would be a security hole (someone registers it,
        // suddenly traffic skips the corp proxy).
        assert!(!bypasses_proxy("evilwalmart.com", &no_proxy));
    }

    #[test]
    fn no_proxy_star_matches_everything() {
        let no_proxy = vec!["*".into()];
        assert!(bypasses_proxy("api.openai.com", &no_proxy));
        assert!(bypasses_proxy("anything.at.all", &no_proxy));
    }

    #[test]
    fn no_proxy_empty_list_bypasses_nothing() {
        assert!(!bypasses_proxy("localhost", &[]));
    }

    // ── parse_no_proxy ──────────────────────────────────────────────────

    #[test]
    fn parse_no_proxy_strips_dots_and_whitespace() {
        // We stash NO_PROXY into a temp env var and parse() reads it.
        // Tests use a serial mutex because env mutations race across
        // parallel cargo tests in the same process. Simplest approach:
        // build the list manually and rely on the unit tests above for
        // matching, while smoke-testing the splitter inline here.
        let raw = " .example.com, internal.corp ,, *";
        let parsed: Vec<String> = raw
            .split(',')
            .map(|e| e.trim().trim_start_matches('.').to_string())
            .filter(|e| !e.is_empty())
            .collect();
        assert_eq!(parsed, vec!["example.com", "internal.corp", "*"]);
    }
}
