//! WebFetch tool — retrieve content from a URL.
//!
//! Fetches a web page and converts HTML to readable text.
//! Body cap is set by `OutputCaps` (context-scaled).
//!
//! ## Parameters
//!
//! - **`url`** (required) — The URL to fetch
//!
//! ## Behavior
//!
//! - HTML pages are converted to clean text (strips tags, scripts, styles)
//! - JSON and plain text are returned as-is
//! - Output is truncated to context-scaled caps
//! - Follows redirects (up to 10 hops, see `MAX_REDIRECTS`), re-validating SSRF
//!   safety on every hop (#1280). Redirects to loopback / RFC1918 private
//!   ranges / link-local cloud-metadata IPs are blocked even if the
//!   initial URL was public.
//! - **DNS-rebinding TOCTOU defense (#1314)**: every hop's `reqwest::Client`
//!   is built with `resolve_to_addrs(host, &validated_addrs)` so the
//!   actual TCP connect uses the same IPs we validated. A malicious DNS
//!   server with a low-TTL A-record swap cannot pass the initial
//!   `validate_url_safety` check then resolve to `127.0.0.1` /
//!   `169.254.169.254` on the connect.
//! - Timeout: 15 seconds (see `DEFAULT_TIMEOUT_SECS`) for the entire
//!   fetch including all redirect hops.

use crate::providers::ToolDefinition;
use anyhow::Result;
use serde_json::{Value, json};
use std::net::SocketAddr;

const DEFAULT_TIMEOUT_SECS: u64 = 15;

/// Maximum number of HTTP redirect hops WebFetch will follow.
///
/// Matches reqwest's default of 10. The original web_fetch implementation
/// inherited this from `reqwest::redirect::Policy::default()` but did NOT
/// re-validate safety on each hop. Now made explicit because we own the
/// redirect loop ourselves (#1280).
pub(crate) const MAX_REDIRECTS: usize = 10;

const USER_AGENT: &str = "Koda/0.1 (AI coding agent)";

/// Return tool definitions for the LLM.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "WebFetch".to_string(),
        description: "Fetch content from a URL. HTML is stripped to readable text by default; \
            set raw=true for raw HTML. Only use URLs from tool results or user input — \
            never guess or generate URLs from memory. \
            For documentation lookup, prefer reading local files first."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL to fetch (must start with http:// or https://)"
                },
                "raw": {
                    "type": "boolean",
                    "description": "If true, return raw HTML instead of stripped text (default: false)"
                }
            },
            "required": ["url"]
        }),
    }]
}

/// A URL that has passed every SSRF safety check, plus the resolved
/// socket addresses to pin reqwest's connect to.
///
/// Returned by [`validate_url_safety`] and consumed by
/// [`safely_follow_redirects`] / [`pinned_client_for`].
///
/// `addrs` is empty when the URL's host is an IP literal (no DNS lookup
/// happened, so there's nothing to pin — reqwest will connect to the
/// IP in the URL directly, which we already validated).
///
/// When `addrs` is non-empty, the per-hop client is built with
/// `reqwest::ClientBuilder::resolve_to_addrs(host, &addrs)`, which
/// makes reqwest skip its own DNS lookup and connect directly to one
/// of the validated IPs. This closes the DNS-rebinding TOCTOU window
/// (#1314).
#[derive(Debug, Clone)]
pub(crate) struct ValidatedTarget {
    pub url: url::Url,
    pub addrs: Vec<SocketAddr>,
}

/// Validate a URL against koda's SSRF policy.
///
/// Combines the synchronous `is_safe_url` host-list / IP-range checks
/// with a DNS pre-check that resolves domain names and rejects any
/// resolution to a private/internal IP. Returns a [`ValidatedTarget`]
/// on success, carrying both the parsed URL and the resolved socket
/// addresses so callers can pin reqwest's connect to the same IPs we
/// just validated.
///
/// This is the **single seam** all WebFetch reachability decisions go
/// through — both the initial URL check and every redirect hop call this
/// function. Adding a new SSRF check here is automatically applied to
/// redirect chains too (#1280).
///
/// **DNS rebinding (#1314):** the returned `addrs` are the *exact* IPs
/// validated here. By passing them to
/// [`reqwest::ClientBuilder::resolve_to_addrs`] the caller ensures
/// reqwest's TCP connect targets one of these IPs rather than
/// re-resolving the hostname (which a malicious low-TTL DNS server
/// could swap to a private IP between our check and the connect).
async fn validate_url_safety(url_str: &str) -> Result<ValidatedTarget> {
    if !url_str.starts_with("http://") && !url_str.starts_with("https://") {
        anyhow::bail!("URL must start with http:// or https://");
    }

    if !is_safe_url(url_str) {
        anyhow::bail!(
            "URL blocked: requests to internal/private networks are not allowed. \
             This includes localhost, private IPs, and cloud metadata endpoints."
        );
    }

    let parsed = url::Url::parse(url_str)
        .map_err(|e| anyhow::anyhow!("Failed to parse URL '{url_str}': {e}"))?;

    // IP-literal hosts: nothing to pin — the IP is already in the URL
    // and was checked by `is_safe_url`. Leave `addrs` empty so the
    // per-hop client builder skips `resolve_to_addrs` (which would be a
    // no-op anyway, since reqwest doesn't look up IP literals).
    let mut addrs = Vec::new();

    if let Some(host) = parsed.host_str()
        && parsed
            .host()
            .is_some_and(|h| matches!(h, url::Host::Domain(_)))
    {
        let port = parsed.port_or_known_default().unwrap_or(80);
        match tokio::net::lookup_host(format!("{host}:{port}")).await {
            Ok(resolved) => {
                for addr in resolved {
                    if !is_safe_ip(addr.ip()) {
                        anyhow::bail!(
                            "URL blocked: domain '{host}' resolves to private/internal IP {}.",
                            addr.ip()
                        );
                    }
                    addrs.push(addr);
                }
                if addrs.is_empty() {
                    anyhow::bail!("DNS resolution returned no addresses for '{host}'");
                }
            }
            Err(e) => {
                anyhow::bail!("DNS resolution failed for '{host}': {e}");
            }
        }
    }

    Ok(ValidatedTarget { url: parsed, addrs })
}

/// Build a [`reqwest::Client`] that pins TCP connects for `target.url`'s
/// host to `target.addrs`, defeating DNS-rebinding TOCTOU (#1314).
///
/// Inherits all the standard koda HTTP client config (timeouts, proxy,
/// localhost-TLS-bypass) from
/// [`crate::providers::build_http_client_builder`].
///
/// When `target.addrs` is empty (IP-literal URLs), no `resolve_to_addrs`
/// override is applied — reqwest connects to the IP in the URL directly.
fn pinned_client_for(target: &ValidatedTarget) -> reqwest::Client {
    let mut builder =
        crate::providers::build_http_client_builder(None, reqwest::redirect::Policy::none());

    if !target.addrs.is_empty()
        && let Some(host) = target.url.host_str()
    {
        // `resolve_to_addrs` overrides DNS for `host` only. Other hosts
        // (e.g. proxy CONNECT targets) still use the system resolver,
        // which is what we want.
        builder = builder.resolve_to_addrs(host, &target.addrs);
    }

    builder.build().unwrap_or_else(|_| reqwest::Client::new())
}

/// Follow HTTP redirects manually, re-validating SSRF safety on every hop.
///
/// A fresh [`reqwest::Client`] is built per hop via [`pinned_client_for`]
/// so that **the actual TCP connect uses the IPs we just validated**, not
/// whatever the system resolver returns at connect time. This closes the
/// DNS-rebinding TOCTOU window (#1314): a malicious DNS server with a
/// low-TTL A-record swap can no longer pass `validate_url_safety` then
/// have reqwest connect to `127.0.0.1` / `169.254.169.254`.
///
/// `validator` is the safety check applied to every redirect target.
/// Production callers pass [`validate_url_safety`]; tests can pass a
/// permissive validator to exercise the redirect loop itself without
/// hitting SSRF blocks on a loopback test server.
///
/// Headers added to the initial request (User-Agent) are re-applied to
/// each redirected request. We deliberately do NOT carry forward any
/// caller-supplied `Authorization`, `Cookie`, or `Proxy-Authorization`
/// headers across redirects because (a) WebFetch doesn't set any today,
/// and (b) this guards against future API additions accidentally
/// leaking secrets to a redirect target.
///
/// Method is GET throughout (WebFetch only does GETs); see the function
/// body for the RFC 7231 method-preservation note we'd need to revisit
/// if WebFetch ever gains POST.
///
/// Per-hop client construction sacrifices connection pooling between
/// hops, but WebFetch is a low-throughput single-shot tool so the
/// extra TCP+TLS handshake per redirect is acceptable.
pub(crate) async fn safely_follow_redirects<F, Fut>(
    initial: ValidatedTarget,
    max_hops: usize,
    validator: F,
) -> Result<reqwest::Response>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Result<ValidatedTarget>>,
{
    let mut current = initial;
    // hop 0 is the initial request; redirects 1..=max_hops are the followed ones.
    for hop in 0..=max_hops {
        let client = pinned_client_for(&current);
        let response = client
            .get(current.url.clone())
            .header("User-Agent", USER_AGENT)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("HTTP request failed: {e}"))?;

        let status = response.status();
        if !status.is_redirection() {
            return Ok(response);
        }

        // Only the standard redirect codes follow a Location header.
        // 304 Not Modified and other 3xx values are returned to the caller as-is.
        if !matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
            return Ok(response);
        }

        if hop == max_hops {
            anyhow::bail!(
                "WebFetch exceeded max redirect hops ({max_hops}); last URL: {}",
                current.url
            );
        }

        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Redirect status {status} from {} but no Location header",
                    current.url
                )
            })?
            .to_str()
            .map_err(|e| {
                anyhow::anyhow!(
                    "Redirect Location header from {} is not valid UTF-8: {e}",
                    current.url
                )
            })?
            .to_string();

        // `Url::join` handles absolute URLs, scheme-relative URLs (`//foo/bar`),
        // path-absolute (`/foo`), and relative (`foo`) Location values per RFC 3986.
        let next_url = current.url.join(&location).map_err(|e| {
            anyhow::anyhow!(
                "Failed to resolve redirect Location '{location}' against {}: {e}",
                current.url
            )
        })?;

        // Re-validate the redirect target. This is the whole point of #1280:
        // the initial is_safe_url+DNS check does NOT cover redirect chains, so
        // a public URL redirecting to 169.254.169.254 would have been silently
        // followed before. Now every hop is checked.
        //
        // The returned ValidatedTarget carries the resolved IPs that
        // pinned_client_for() will pass to resolve_to_addrs() on the
        // NEXT iteration — so #1314's TOCTOU window is also closed for
        // every redirect hop, not just the initial fetch.
        let prev_url = current.url.clone();
        current = validator(next_url.to_string()).await.map_err(|e| {
            anyhow::anyhow!("Redirect from {prev_url} to {next_url} blocked by SSRF policy: {e}")
        })?;
    }

    // Loop exits via the `is_redirection() == false` early return or the
    // max_hops bail; this line is unreachable but the compiler can't prove it.
    unreachable!("safely_follow_redirects loop exited without returning")
}

/// Get-or-init the WebFetch HTTP client.
///
/// **Deprecated as of #1314**: kept only for backwards-compat with any
/// test or external caller that still expects a shared client. Production
/// `web_fetch` no longer uses this — [`safely_follow_redirects`] builds
/// a fresh client per hop via [`pinned_client_for`] so DNS-validated
/// IPs are passed to `resolve_to_addrs`. A shared client cannot do that
/// because the resolver overrides are baked in at `Client::build()`
/// time and the host changes per redirect.
#[allow(dead_code)]
fn web_fetch_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        crate::providers::build_http_client_with_redirect_policy(
            None,
            reqwest::redirect::Policy::none(),
        )
    })
}

/// Fetch a URL and return its content.
pub async fn web_fetch(args: &Value, max_body_chars: usize) -> Result<String> {
    let url_str = args["url"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'url' argument"))?;
    let raw = args["raw"].as_bool().unwrap_or(false);

    let initial_target = validate_url_safety(url_str).await?;

    let response = tokio::time::timeout(
        std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        safely_follow_redirects(initial_target, MAX_REDIRECTS, |u| async move {
            validate_url_safety(&u).await
        }),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Request timed out after {DEFAULT_TIMEOUT_SECS}s"))??;

    let final_url = response.url().clone();
    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("HTTP {status} for {final_url}");
    }

    let body = response
        .text()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read response body: {e}"))?;

    let content = if raw { body } else { strip_html(&body) };

    if content.len() > max_body_chars {
        Ok(format!(
            "{}\n\n[TRUNCATED: response was {} chars. \
             Consider fetching a more specific URL.]",
            &content[..max_body_chars],
            content.len()
        ))
    } else {
        Ok(content)
    }
}

/// Check if an IP address is safe (not private/internal/loopback).
pub(crate) fn is_safe_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ipv4) => {
            let octets = ipv4.octets();
            // Loopback, private, link-local, unspecified
            if octets[0] == 127
                || octets[0] == 10
                || (octets[0] == 172 && (16..=31).contains(&octets[1]))
                || (octets[0] == 192 && octets[1] == 168)
                || (octets[0] == 169 && octets[1] == 254)
                || ipv4.is_unspecified()
            {
                return false;
            }
            true
        }
        std::net::IpAddr::V6(ipv6) => {
            if ipv6.is_loopback() || ipv6.is_unspecified() {
                return false;
            }
            if let Some(ipv4) = ipv6.to_ipv4_mapped() {
                return is_safe_ip(std::net::IpAddr::V4(ipv4));
            }
            true
        }
    }
}

/// Check if a URL is safe to fetch (not internal/private network).
/// Uses the `url` crate for robust parsing (handles userinfo@, IPv6, etc.).
pub(crate) fn is_safe_url(url_str: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url_str) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };

    // Block known metadata hostnames
    let blocked_hosts = [
        "169.254.169.254",
        "metadata.google.internal",
        "metadata.internal",
        "localhost",
        "0.0.0.0",
    ];
    if blocked_hosts.contains(&host) {
        return false;
    }

    // Block .internal and .local TLDs
    if host.ends_with(".internal") || host.ends_with(".local") {
        return false;
    }

    // Block private/reserved IPs using the parsed host
    match parsed.host() {
        Some(url::Host::Ipv4(ip)) => {
            if !is_safe_ip(std::net::IpAddr::V4(ip)) {
                return false;
            }
        }
        Some(url::Host::Ipv6(ip)) => {
            if !is_safe_ip(std::net::IpAddr::V6(ip)) {
                return false;
            }
        }
        Some(url::Host::Domain(_)) => {
            // Domain names — hostname checks above are sufficient
            // (DNS resolution check happens separately in web_fetch)
        }
        None => return false,
    }

    true
}

/// Strip HTML tags and collapse whitespace for readability.
fn strip_html(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut in_script = false;
    let mut in_style = false;
    let mut last_was_space = false;

    let lower = html.to_lowercase();
    let chars: Vec<char> = html.chars().collect();
    let lower_chars: Vec<char> = lower.chars().collect();

    let mut i = 0;
    while i < chars.len() {
        if in_script {
            // Skip until </script>
            if i + 9 <= lower_chars.len()
                && lower_chars[i..i + 9].iter().collect::<String>() == "</script>"
            {
                in_script = false;
                i += 9;
            } else {
                i += 1;
            }
            continue;
        }
        if in_style {
            if i + 8 <= lower_chars.len()
                && lower_chars[i..i + 8].iter().collect::<String>() == "</style>"
            {
                in_style = false;
                i += 8;
            } else {
                i += 1;
            }
            continue;
        }

        if chars[i] == '<' {
            // Check for <script or <style
            if i + 7 <= lower_chars.len()
                && lower_chars[i..i + 7].iter().collect::<String>() == "<script"
            {
                in_script = true;
            } else if i + 6 <= lower_chars.len()
                && lower_chars[i..i + 6].iter().collect::<String>() == "<style"
            {
                in_style = true;
            }
            in_tag = true;
            // Block-level tags → newline
            let tag_start: String = lower_chars[i..std::cmp::min(i + 10, lower_chars.len())]
                .iter()
                .collect();
            if tag_start.starts_with("<br")
                || tag_start.starts_with("<p")
                || tag_start.starts_with("<div")
                || tag_start.starts_with("<h")
                || tag_start.starts_with("<li")
                || tag_start.starts_with("<tr")
            {
                result.push('\n');
                last_was_space = true;
            }
            i += 1;
            continue;
        }

        if chars[i] == '>' {
            in_tag = false;
            i += 1;
            continue;
        }

        if !in_tag {
            let ch = chars[i];
            if ch.is_whitespace() {
                if !last_was_space {
                    result.push(' ');
                    last_was_space = true;
                }
            } else {
                result.push(ch);
                last_was_space = false;
            }
        }
        i += 1;
    }

    // Decode common HTML entities
    result
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

// =============================================================
// Tool trait implementation (#1265 item 5, PR-7/N).
//
// `WebFetch` is read-only — GET-only fetch, no undo, no mutation.
// Reads `caps.web_body_chars` off the context.
// =============================================================

use crate::tools::{Tool, ToolEffect, ToolExecCtx, ToolResult};
use async_trait::async_trait;

/// `WebFetch` — GET a URL and return body (HTML to plain text).
pub struct WebFetchTool;

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &'static str {
        "WebFetch"
    }
    fn definition(&self) -> ToolDefinition {
        definitions()
            .into_iter()
            .find(|d| d.name == "WebFetch")
            .expect("web_fetch::definitions() must contain WebFetch")
    }
    fn classify(&self, _args: &serde_json::Value) -> ToolEffect {
        ToolEffect::ReadOnly
    }
    async fn execute(&self, ctx: &ToolExecCtx<'_>, args: &serde_json::Value) -> ToolResult {
        let r = web_fetch(args, ctx.caps.web_body_chars).await;
        crate::tools::wrap_result(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_html_basic() {
        let html = "<h1>Hello</h1><p>World &amp; friends</p>";
        let result = strip_html(html);
        assert!(result.contains("Hello"));
        assert!(result.contains("World & friends"));
        assert!(!result.contains("<h1>"));
    }

    #[test]
    fn test_strip_html_script_removal() {
        let html = "<p>Before</p><script>alert('xss')</script><p>After</p>";
        let result = strip_html(html);
        assert!(result.contains("Before"));
        assert!(result.contains("After"));
        assert!(!result.contains("alert"));
    }

    #[test]
    fn test_strip_html_whitespace_collapse() {
        let html = "<p>  lots   of    spaces  </p>";
        let result = strip_html(html);
        assert!(!result.contains("   ")); // No triple spaces
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_web_fetch_bad_url() {
        let args = json!({ "url": "not-a-url" });
        let result = web_fetch(&args, 15_000).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_is_safe_url_blocks_metadata() {
        assert!(!is_safe_url("http://169.254.169.254/latest/meta-data/"));
        assert!(!is_safe_url("http://metadata.google.internal/"));
    }

    #[test]
    fn test_is_safe_url_blocks_localhost() {
        assert!(!is_safe_url("http://localhost:8080/admin"));
        assert!(!is_safe_url("http://127.0.0.1/secret"));
        assert!(!is_safe_url("http://0.0.0.0/"));
    }

    #[test]
    fn test_is_safe_url_blocks_private_ips() {
        assert!(!is_safe_url("http://10.0.0.1/internal"));
        assert!(!is_safe_url("http://172.16.0.1/admin"));
        assert!(!is_safe_url("http://192.168.1.1/config"));
    }

    #[test]
    fn test_is_safe_url_blocks_userinfo_bypass() {
        // RFC 3986 userinfo@ component should not fool the parser
        assert!(!is_safe_url(
            "http://evil.com@169.254.169.254/latest/meta-data/"
        ));
        assert!(!is_safe_url("http://user:pass@127.0.0.1/"));
    }

    #[test]
    fn test_is_safe_url_blocks_ipv6_mapped() {
        assert!(!is_safe_url("http://[::ffff:127.0.0.1]/"));
        assert!(!is_safe_url("http://[::1]/"));
    }

    #[test]
    fn test_is_safe_url_allows_public() {
        assert!(is_safe_url("https://docs.rs/tokio/latest/tokio/"));
        assert!(is_safe_url("https://api.github.com/repos"));
        assert!(is_safe_url("https://example.com"));
    }

    // ── is_safe_ip tests (#526) ──

    #[test]
    fn test_is_safe_ip_blocks_private() {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
        assert!(!is_safe_ip(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(!is_safe_ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(!is_safe_ip(IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1))));
        assert!(!is_safe_ip(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
        assert!(!is_safe_ip(IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))));
        assert!(!is_safe_ip(IpAddr::V4(Ipv4Addr::UNSPECIFIED)));
        assert!(!is_safe_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(!is_safe_ip(IpAddr::V6(Ipv6Addr::UNSPECIFIED)));
    }

    #[test]
    fn test_is_safe_ip_allows_public() {
        use std::net::{IpAddr, Ipv4Addr};
        assert!(is_safe_ip(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(is_safe_ip(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
        assert!(is_safe_ip(IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_web_fetch_blocks_ssrf() {
        let args = json!({ "url": "http://169.254.169.254/latest/meta-data/" });
        let result = web_fetch(&args, 15_000).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("blocked"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_web_fetch_missing_url() {
        let args = json!({});
        let result = web_fetch(&args, 15_000).await;
        assert!(result.is_err());
    }

    // ========================================================================
    // Redirect re-validation tests (#1280)
    //
    // The bug we're guarding against: pre-#1280, web_fetch validated only
    // the initial URL via is_safe_url + DNS check, then handed off to a
    // shared reqwest::Client whose default redirect policy follows up to
    // 10 hops with NO re-validation. A public URL could redirect to
    // 127.0.0.1, 169.254.169.254, an RFC1918 host, etc., and reqwest
    // would silently follow.
    //
    // Strategy: spin up a real tiny HTTP server on a loopback port, have it
    // serve a configurable sequence of redirects, and assert that
    // safely_follow_redirects() either re-validates each hop (when given the
    // production validator) or honors a hop limit (when given a permissive
    // validator). Using a real server matters: a mock that bypasses the
    // reqwest Client wouldn't exercise the actual redirect policy wiring.
    // ========================================================================

    use axum::{Router, extract::State, http::StatusCode, response::IntoResponse, routing::get};
    use std::sync::{Arc, Mutex as StdMutex};
    use tokio_util::sync::CancellationToken;

    /// One step in a scripted server response sequence.
    #[derive(Clone, Debug)]
    enum Step {
        /// Respond 302 with the given Location header value.
        Redirect(String),
        /// Respond 200 OK with the given body.
        Ok(String),
    }

    #[derive(Clone)]
    struct ServerState {
        /// Pop-front queue of scripted responses. After exhaustion, the server
        /// returns 500 so test failures are loud.
        steps: Arc<StdMutex<Vec<Step>>>,
    }

    async fn handler(
        State(state): State<ServerState>,
        uri: axum::http::Uri,
    ) -> axum::response::Response {
        let step = state.steps.lock().expect("steps mutex poisoned").pop();
        match step {
            Some(Step::Redirect(loc)) => (
                StatusCode::FOUND,
                [(axum::http::header::LOCATION, loc)],
                String::new(),
            )
                .into_response(),
            Some(Step::Ok(body)) => (StatusCode::OK, body).into_response(),
            None => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("unexpected request to {uri} — test script exhausted"),
            )
                .into_response(),
        }
    }

    /// Spin up an axum server on 127.0.0.1:0 with a scripted response queue.
    /// Returns the base URL (e.g. `http://127.0.0.1:54321`) and a cancel
    /// token the test must trigger to shut the server down.
    async fn spawn_test_server(steps: Vec<Step>) -> (String, CancellationToken) {
        // Steps are pushed back into a stack-style Vec so handler can `pop()`
        // in O(1) and the test reads top-down. Reverse here so the first
        // request gets steps[0].
        let mut reversed = steps;
        reversed.reverse();
        let state = ServerState {
            steps: Arc::new(StdMutex::new(reversed)),
        };
        let app = Router::new()
            .route("/{*path}", get(handler))
            .route("/", get(handler))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let ct = CancellationToken::new();
        let ct_server = ct.clone();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { ct_server.cancelled_owned().await })
                .await
                .ok();
        });
        (url, ct)
    }

    /// A validator that allows any URL. Lets us exercise the redirect loop
    /// itself (loopback test server hitting loopback redirect targets)
    /// without the production SSRF check rejecting our own test fixtures.
    ///
    /// Returns an empty `addrs` list, which makes [`pinned_client_for`]
    /// skip the `resolve_to_addrs` override — reqwest then uses the
    /// system resolver, which correctly resolves loopback IP literals
    /// in the test URLs.
    async fn permissive_validator(url: String) -> Result<ValidatedTarget> {
        let parsed = url::Url::parse(&url).map_err(|e| anyhow::anyhow!("parse: {e}"))?;
        Ok(ValidatedTarget {
            url: parsed,
            addrs: Vec::new(),
        })
    }

    /// Build a [`ValidatedTarget`] for an IP-literal URL (test-only helper).
    /// Empty `addrs` is correct for IP literals — reqwest connects to the
    /// IP in the URL directly without DNS lookup.
    fn ip_literal_target(url_str: &str) -> ValidatedTarget {
        ValidatedTarget {
            url: url::Url::parse(url_str).expect("valid URL"),
            addrs: Vec::new(),
        }
    }

    /// The headline #1280 bug: a public-looking URL redirects to loopback,
    /// and the production validator must reject the redirect target even
    /// though the initial server URL was "reachable."
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_redirect_to_loopback_is_blocked_by_production_validator() {
        // Server's first response: 302 -> http://127.0.0.1:1/secret.
        // The production validator MUST reject this redirect target.
        let (server_url, ct) = spawn_test_server(vec![Step::Redirect(
            "http://127.0.0.1:1/secret".to_string(),
        )])
        .await;

        let initial = ip_literal_target(&server_url);
        let result = safely_follow_redirects(initial, MAX_REDIRECTS, |u| async move {
            validate_url_safety(&u).await
        })
        .await;

        ct.cancel();
        let err = result.expect_err("redirect to loopback must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("blocked") || msg.contains("SSRF"),
            "error should mention SSRF/blocked, got: {msg}"
        );
    }

    /// Same bug, cloud-metadata variant. The classic GCP/AWS
    /// credential-exfil target. Must be rejected on redirect just like a
    /// direct hit.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_redirect_to_cloud_metadata_is_blocked() {
        let (server_url, ct) = spawn_test_server(vec![Step::Redirect(
            "http://169.254.169.254/latest/meta-data/iam/security-credentials/".to_string(),
        )])
        .await;

        let initial = ip_literal_target(&server_url);
        let result = safely_follow_redirects(initial, MAX_REDIRECTS, |u| async move {
            validate_url_safety(&u).await
        })
        .await;

        ct.cancel();
        assert!(
            result.is_err(),
            "redirect to 169.254.169.254 must be rejected"
        );
    }

    /// Exceeding the hop limit must be a hard error, not silently truncated.
    /// Uses the permissive validator so we isolate the hop-count enforcement
    /// from the SSRF check.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_max_redirect_hops_enforced() {
        // 11 redirects in a chain — max_hops=3 should bail at hop 3.
        let mut steps: Vec<Step> = (0..11)
            .map(|i| Step::Redirect(format!("/hop{i}")))
            .collect();
        steps.push(Step::Ok("never reached".to_string()));
        let (server_url, ct) = spawn_test_server(steps).await;

        let initial = ip_literal_target(&server_url);
        let result = safely_follow_redirects(initial, 3, permissive_validator).await;

        ct.cancel();
        let err = result.expect_err("hop limit must be enforced");
        let msg = err.to_string();
        assert!(
            msg.contains("max redirect hops"),
            "error should mention hop cap, got: {msg}"
        );
    }

    /// Relative `Location` headers must resolve against the current URL
    /// (RFC 3986) and be re-validated like absolute ones.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_relative_redirect_resolves_against_current_url() {
        // Two-hop relative redirect: /a -> /b (relative) -> 200 OK.
        let (server_url, ct) = spawn_test_server(vec![
            Step::Redirect("/b".to_string()),
            Step::Ok("final body".to_string()),
        ])
        .await;

        let initial = ip_literal_target(&format!("{server_url}/a"));
        let response = safely_follow_redirects(initial, MAX_REDIRECTS, permissive_validator)
            .await
            .expect("relative redirect should succeed");

        let final_url = response.url().clone();
        let body = response.text().await.unwrap();

        ct.cancel();
        assert_eq!(body, "final body");
        assert!(
            final_url.path().ends_with("/b"),
            "final URL should be the relative-resolved /b, got: {final_url}"
        );
    }

    /// Scheme-relative `Location: //evil.com/...` resolves against the
    /// current URL's scheme — verify it doesn't sneak past the validator.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_scheme_relative_redirect_revalidated() {
        // `//127.0.0.1:1/x` resolves to `http://127.0.0.1:1/x` against an
        // http base; production validator must still reject.
        let (server_url, ct) =
            spawn_test_server(vec![Step::Redirect("//127.0.0.1:1/x".to_string())]).await;

        let initial = ip_literal_target(&server_url);
        let result = safely_follow_redirects(initial, MAX_REDIRECTS, |u| async move {
            validate_url_safety(&u).await
        })
        .await;

        ct.cancel();
        assert!(
            result.is_err(),
            "scheme-relative redirect to loopback must be rejected"
        );
    }

    /// Happy path: a normal 302 chain that ends at 200 with permissive
    /// validation works end to end. Sanity check that the loop returns the
    /// right body.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_happy_path_two_hop_redirect_chain() {
        let (server_url, ct) = spawn_test_server(vec![
            Step::Redirect("/step2".to_string()),
            Step::Redirect("/final".to_string()),
            Step::Ok("hello world".to_string()),
        ])
        .await;

        let initial = ip_literal_target(&server_url);
        let response = safely_follow_redirects(initial, MAX_REDIRECTS, permissive_validator)
            .await
            .expect("happy path should succeed");
        let body = response.text().await.unwrap();

        ct.cancel();
        assert_eq!(body, "hello world");
    }

    // ========================================================================
    // DNS-rebinding TOCTOU regression tests (#1314)
    //
    // The bug we're guarding against: pre-#1314, validate_url_safety called
    // tokio::net::lookup_host to confirm a domain resolves to a public IP,
    // then handed the URL (NOT the resolved IPs) off to a shared
    // reqwest::Client whose connect performs its OWN DNS resolution. A
    // malicious DNS server with a low-TTL A-record swap could pass the
    // validation lookup with `8.8.8.8` then resolve to `127.0.0.1` /
    // `169.254.169.254` on the connect.
    //
    // Strategy: prove `resolve_to_addrs` pinning is active by using an
    // RFC-2606-reserved `.invalid` hostname (guaranteed to fail real DNS)
    // and a target whose `addrs` points at a real local server. If pinning
    // works, the request succeeds (reqwest uses the pinned addr, never
    // touches DNS). If pinning is broken, the request fails on NXDOMAIN.
    // Counterexample test: an empty addrs list with a `.invalid` host
    // MUST fail — confirming the test would catch a regression.
    // ========================================================================

    /// validate_url_safety must populate `addrs` with non-empty resolved
    /// IPs for a domain host so callers can pin reqwest's connect.
    ///
    /// Uses `localhost` (NOT a private IP per `is_safe_url`'s blocklist)
    /// — wait, localhost IS blocked. Use `example.com` which resolves to
    /// a public IP via real DNS. This test requires network; if DNS fails
    /// (offline CI, etc.) it's skipped via the Err arm rather than
    /// flaking. The behavior we care about (non-empty addrs on success)
    /// is what we assert.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_validate_url_safety_returns_resolved_addrs_for_domain() {
        let result = validate_url_safety("https://example.com/").await;
        match result {
            Ok(target) => {
                assert!(
                    !target.addrs.is_empty(),
                    "validate_url_safety must return resolved socket addrs for a domain so \
                     callers can pin reqwest's connect (#1314); got empty addrs for example.com"
                );
                // Sanity: every returned addr must be safe (the validator
                // already enforced this, but double-check).
                for addr in &target.addrs {
                    assert!(
                        is_safe_ip(addr.ip()),
                        "validate_url_safety returned unsafe IP {}",
                        addr.ip()
                    );
                }
            }
            Err(e) => {
                eprintln!(
                    "SKIPPING test_validate_url_safety_returns_resolved_addrs_for_domain: {e} \
                     (likely offline CI — the addrs-pinning behavior is also covered by \
                     test_pinned_client_uses_resolve_to_addrs which doesn't need real DNS)"
                );
            }
        }
    }

    /// IP-literal URLs need no DNS lookup, so `addrs` must be empty.
    /// (Production code uses `addrs.is_empty()` as the signal to skip
    /// `resolve_to_addrs`, which would be a no-op for IP literals anyway
    /// but keeps the wire clean.)
    ///
    /// Uses `8.8.8.8` (Google DNS — public IP, passes is_safe_url) so
    /// validate_url_safety doesn't reject it for being private.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_validate_url_safety_empty_addrs_for_ip_literal() {
        let target = validate_url_safety("http://8.8.8.8/")
            .await
            .expect("public IP literal must validate");
        assert!(
            target.addrs.is_empty(),
            "IP-literal URLs need no DNS pinning; addrs must stay empty, got: {:?}",
            target.addrs
        );
    }

    /// **The headline #1314 regression test.** Proves `pinned_client_for`
    /// actually wires `resolve_to_addrs` into the reqwest client.
    ///
    /// Setup: real local axum server on 127.0.0.1:PORT. ValidatedTarget
    /// uses `http://attacker.invalid:PORT/` (RFC-2606 `.invalid` TLD,
    /// guaranteed NXDOMAIN on real DNS) but pins `addrs = [127.0.0.1:PORT]`.
    ///
    /// If pinning works → reqwest connects to 127.0.0.1:PORT, request
    /// succeeds, body matches. If pinning is broken → reqwest tries to
    /// resolve `attacker.invalid` via system DNS, gets NXDOMAIN, fails.
    ///
    /// This test would FAIL if someone reverted #1314 by removing the
    /// `resolve_to_addrs` call from pinned_client_for.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_pinned_client_uses_resolve_to_addrs() {
        let (server_url, ct) = spawn_test_server(vec![Step::Ok("pinning works".to_string())]).await;

        // Extract the port the test server is listening on.
        let parsed = url::Url::parse(&server_url).unwrap();
        let port = parsed
            .port()
            .expect("test server URL must have explicit port");
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

        // Spoofed URL: hostname is RFC-2606 `.invalid` (guaranteed
        // NXDOMAIN). Without `resolve_to_addrs` pinning, this fetch
        // CANNOT succeed.
        let spoofed_url = url::Url::parse(&format!("http://attacker.invalid:{port}/")).unwrap();
        let target = ValidatedTarget {
            url: spoofed_url,
            addrs: vec![addr],
        };

        let response = safely_follow_redirects(target, MAX_REDIRECTS, permissive_validator)
            .await
            .expect(
                "pinned_client_for MUST honor `addrs` via resolve_to_addrs (#1314); \
                 if this fails with a DNS error, the pinning has regressed and the \
                 DNS-rebinding TOCTOU window is reopened",
            );
        let body = response.text().await.unwrap();

        ct.cancel();
        assert_eq!(body, "pinning works");
    }

    /// Counterexample to the test above: prove the test methodology works
    /// by showing that without pinning (empty `addrs`), the same
    /// `.invalid` hostname **does** fail with a DNS error.
    ///
    /// This guards against a sneaky regression where someone breaks
    /// pinning but the headline test passes anyway because of some other
    /// fallback. If THIS test ever passes, the headline test is no
    /// longer trustworthy.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_invalid_hostname_without_pinning_fails() {
        // No server needed — the DNS lookup happens before any TCP
        // connect, so the request never reaches a server.
        let target = ValidatedTarget {
            url: url::Url::parse("http://attacker.invalid:1/").unwrap(),
            addrs: Vec::new(), // <-- the key difference: no pinning
        };

        let result = safely_follow_redirects(target, MAX_REDIRECTS, permissive_validator).await;

        let err = result.expect_err(
            "control test: a `.invalid` hostname WITHOUT pinning must fail (NXDOMAIN); \
             if this passes, the test methodology for `test_pinned_client_uses_resolve_to_addrs` \
             is broken and that test's success no longer proves pinning works",
        );
        // Don't assert on the exact error string — reqwest's DNS error
        // wording varies across platforms. The fact that it errored at
        // all (vs. successfully connecting somewhere) is the signal.
        let msg = err.to_string();
        assert!(
            msg.contains("HTTP request failed"),
            "expected an HTTP/connection failure, got: {msg}"
        );
    }

    /// Redirect chains must also re-pin per hop. If a hop redirects to a
    /// new host, the validator returns a new ValidatedTarget with that
    /// host's resolved addrs, and the next iteration's pinned_client_for
    /// applies the new pinning.
    ///
    /// We simulate this by having the test server redirect to itself
    /// under a `.invalid` hostname (same port), and providing a
    /// custom validator that returns the right pinning for the
    /// redirect target. End-to-end proof that #1314 covers the full
    /// redirect chain, not just the initial fetch.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_pinning_applies_per_redirect_hop() {
        let (server_url, ct) = spawn_test_server(vec![
            Step::Redirect("http://attacker.invalid/final".to_string()),
            Step::Ok("reached final hop".to_string()),
        ])
        .await;

        let parsed = url::Url::parse(&server_url).unwrap();
        let port = parsed
            .port()
            .expect("test server URL must have explicit port");
        let server_addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

        // Validator that pins `attacker.invalid` to the test server's
        // real address. In production this would be validate_url_safety;
        // here we simulate "validation passed and resolved to safe IPs."
        let pinning_validator = move |url: String| async move {
            let parsed = url::Url::parse(&url).map_err(|e| anyhow::anyhow!("parse: {e}"))?;
            let addrs = if parsed.host_str() == Some("attacker.invalid") {
                // Override the redirect's port with the test server's port.
                vec![server_addr]
            } else {
                Vec::new()
            };
            // Rewrite the URL's port too so the request actually reaches
            // our test server (resolve_to_addrs pins the IP but reqwest
            // still uses the URL's port).
            let mut u = parsed;
            if u.host_str() == Some("attacker.invalid") {
                u.set_port(Some(port)).unwrap();
            }
            Ok(ValidatedTarget { url: u, addrs })
        };

        let initial = ip_literal_target(&server_url);
        let response = safely_follow_redirects(initial, MAX_REDIRECTS, pinning_validator)
            .await
            .expect("per-hop pinning must let the redirect chain complete");
        let body = response.text().await.unwrap();

        ct.cancel();
        assert_eq!(body, "reached final hop");
    }
}
