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
//! - Timeout: 15 seconds (see `DEFAULT_TIMEOUT_SECS`) for the entire
//!   fetch including all redirect hops.

use crate::providers::ToolDefinition;
use anyhow::Result;
use serde_json::{Value, json};

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

/// Validate a URL against koda's SSRF policy.
///
/// Combines the synchronous `is_safe_url` host-list / IP-range checks
/// with a DNS pre-check that resolves domain names and rejects any
/// resolution that rivate/internal IP. Returns the parsed
/// [`url::Url`] on success so callers don't re-parse.
///
/// This is the **single seam** all WebFetch reachability decisions go
/// through — both the initial URL check and every redirect hop call this
/// function. Adding a new SSRF check here is automatically applied to
/// redirect chains too (#1280).
async fn validate_url_safety(url_str: &str) -> Result<url::Url> {
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

    if let Some(host) = parsed.host_str()
        && parsed
            .host()
            .is_some_and(|h| matches!(h, url::Host::Domain(_)))
    {
        let port = parsed.port_or_known_default().unwrap_or(80);
        match tokio::net::lookup_host(format!("{host}:{port}")).await {
            Ok(addrs) => {
                for addr in addrs {
                    if !is_safe_ip(addr.ip()) {
                        anyhow::bail!(
                            "URL blocked: domain '{host}' resolves to private/internal IP {}.",
                            addr.ip()
                        );
                    }
                }
            }
            Err(e) => {
                anyhow::bail!("DNS resolution failed for '{host}': {e}");
            }
        }
    }

    Ok(parsed)
}

/// Follow HTTP redirects manually, re-validating SSRF safety on every hop.
///
/// `client` MUST be configured with `redirect::Policy::none()` (which is
/// what [`web_fetch_client`] does); otherwise reqwest will silently
/// follow redirects without re-validation, defeating the whole point.
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
pub(crate) async fn safely_follow_redirects<F, Fut>(
    client: &reqwest::Client,
    initial_url: url::Url,
    max_hops: usize,
    validator: F,
) -> Result<reqwest::Response>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Result<url::Url>>,
{
    let mut current_url = initial_url;
    // hop 0 is the initial request; redirects 1..=max_hops are the followed ones.
    for hop in 0..=max_hops {
        let response = client
            .get(current_url.clone())
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
                "WebFetch exceeded max redirect hops ({max_hops}); last URL: {current_url}"
            );
        }

        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Redirect status {status} from {current_url} but no Location header"
                )
            })?
            .to_str()
            .map_err(|e| {
                anyhow::anyhow!(
                    "Redirect Location header from {current_url} is not valid UTF-8: {e}"
                )
            })?
            .to_string();

        // `Url::join` handles absolute URLs, scheme-relative URLs (`//foo/bar`),
        // path-absolute (`/foo`), and relative (`foo`) Location values per RFC 3986.
        let next_url = current_url.join(&location).map_err(|e| {
            anyhow::anyhow!(
                "Failed to resolve redirect Location '{location}' against {current_url}: {e}"
            )
        })?;

        // Re-validate the redirect target. This is the whole point of #1280:
        // the initial is_safe_url+DNS check does NOT cover redirect chains, so
        // a public URL redirecting to 169.254.169.254 would have been silently
        // followed before. Now every hop is checked.
        current_url = validator(next_url.to_string()).await.map_err(|e| {
            anyhow::anyhow!("Redirect from {current_url} to {next_url} blocked by SSRF policy: {e}")
        })?;
    }

    // Loop exits via the `is_redirection() == false` early return or the
    // max_hops bail; this line is unreachable but the compiler can't prove it.
    unreachable!("safely_follow_redirects loop exited without returning")
}

/// Get-or-init the WebFetch HTTP client. Configured with redirect policy
/// **disabled** (`Policy::none()`) so [`safely_follow_redirects`] owns
/// the redirect loop and can re-validate every hop against SSRF policy.
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

    let initial_url = validate_url_safety(url_str).await?;
    let client = web_fetch_client();

    let response = tokio::time::timeout(
        std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        safely_follow_redirects(client, initial_url, MAX_REDIRECTS, |u| async move {
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
    async fn permissive_validator(url: String) -> Result<url::Url> {
        url::Url::parse(&url).map_err(|e| anyhow::anyhow!("parse: {e}"))
    }

    fn test_client() -> reqwest::Client {
        crate::providers::build_http_client_with_redirect_policy(
            None,
            reqwest::redirect::Policy::none(),
        )
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

        let initial = url::Url::parse(&server_url).unwrap();
        let client = test_client();
        let result = safely_follow_redirects(&client, initial, MAX_REDIRECTS, |u| async move {
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

        let initial = url::Url::parse(&server_url).unwrap();
        let client = test_client();
        let result = safely_follow_redirects(&client, initial, MAX_REDIRECTS, |u| async move {
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

        let initial = url::Url::parse(&server_url).unwrap();
        let client = test_client();
        let result = safely_follow_redirects(&client, initial, 3, permissive_validator).await;

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

        let initial = url::Url::parse(&format!("{server_url}/a")).unwrap();
        let client = test_client();
        let response =
            safely_follow_redirects(&client, initial, MAX_REDIRECTS, permissive_validator)
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

        let initial = url::Url::parse(&server_url).unwrap();
        let client = test_client();
        let result = safely_follow_redirects(&client, initial, MAX_REDIRECTS, |u| async move {
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

        let initial = url::Url::parse(&server_url).unwrap();
        let client = test_client();
        let response =
            safely_follow_redirects(&client, initial, MAX_REDIRECTS, permissive_validator)
                .await
                .expect("happy path should succeed");
        let body = response.text().await.unwrap();

        ct.cancel();
        assert_eq!(body, "hello world");
    }
}
