//! WebFetch tool: retrieve content from a URL.
//!
//! Fetches a web page and returns the textual content,
//! stripping HTML tags for readability.

use crate::providers::ToolDefinition;
use anyhow::Result;
use serde_json::{Value, json};

const MAX_BODY_CHARS: usize = 15_000;
const DEFAULT_TIMEOUT_SECS: u64 = 15;

/// Return tool definitions for the LLM.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "WebFetch".to_string(),
        description: "Fetch content from a URL. Returns the page text with HTML tags stripped. \
            Useful for reading documentation, APIs, or any public web page."
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

/// Fetch a URL and return its content.
pub async fn web_fetch(args: &Value) -> Result<String> {
    let url = args["url"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'url' argument"))?;
    let raw = args["raw"].as_bool().unwrap_or(false);

    if !url.starts_with("http://") && !url.starts_with("https://") {
        anyhow::bail!("URL must start with http:// or https://");
    }

    // SSRF protection: block requests to internal/private networks
    if !is_safe_url(url) {
        anyhow::bail!(
            "URL blocked: requests to internal/private networks are not allowed. \
             This includes localhost, private IPs, and cloud metadata endpoints."
        );
    }

    static HTTP_CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let client = HTTP_CLIENT
        .get_or_init(crate::providers::build_http_client)
        .clone();
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        client
            .get(url)
            .header("User-Agent", "Koda/0.1 (AI coding agent)")
            .send(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Request timed out after {DEFAULT_TIMEOUT_SECS}s"))?
    .map_err(|e| anyhow::anyhow!("HTTP request failed: {e}"))?;

    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("HTTP {status} for {url}");
    }

    let body = response
        .text()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read response body: {e}"))?;

    let content = if raw { body } else { strip_html(&body) };

    if content.len() > MAX_BODY_CHARS {
        Ok(format!(
            "{}\n\n[TRUNCATED: response was {} chars. \
             Consider fetching a more specific URL.]",
            &content[..MAX_BODY_CHARS],
            content.len()
        ))
    } else {
        Ok(content)
    }
}

/// Check if a URL is safe to fetch (not internal/private network).
/// Uses the `url` crate for robust parsing (handles userinfo@, IPv6, etc.).
fn is_safe_url(url_str: &str) -> bool {
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
            let octets = ip.octets();
            // Loopback (127.0.0.0/8)
            if octets[0] == 127 {
                return false;
            }
            // Private 10.0.0.0/8
            if octets[0] == 10 {
                return false;
            }
            // Private 172.16.0.0/12
            if octets[0] == 172 && (16..=31).contains(&octets[1]) {
                return false;
            }
            // Private 192.168.0.0/16
            if octets[0] == 192 && octets[1] == 168 {
                return false;
            }
            // Link-local 169.254.0.0/16
            if octets[0] == 169 && octets[1] == 254 {
                return false;
            }
            // Unspecified
            if ip.is_unspecified() {
                return false;
            }
        }
        Some(url::Host::Ipv6(ip)) => {
            if ip.is_loopback() || ip.is_unspecified() {
                return false;
            }
            // Check for IPv4-mapped IPv6 (::ffff:x.x.x.x)
            if let Some(ipv4) = ip.to_ipv4_mapped() {
                let octets = ipv4.octets();
                if octets[0] == 127 {
                    return false;
                }
                if octets[0] == 10 {
                    return false;
                }
                if octets[0] == 172 && (16..=31).contains(&octets[1]) {
                    return false;
                }
                if octets[0] == 192 && octets[1] == 168 {
                    return false;
                }
                if octets[0] == 169 && octets[1] == 254 {
                    return false;
                }
            }
        }
        Some(url::Host::Domain(_)) => {
            // Domain names — hostname checks above are sufficient
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

    #[tokio::test]
    async fn test_web_fetch_bad_url() {
        let args = json!({ "url": "not-a-url" });
        let result = web_fetch(&args).await;
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

    #[tokio::test]
    async fn test_web_fetch_blocks_ssrf() {
        let args = json!({ "url": "http://169.254.169.254/latest/meta-data/" });
        let result = web_fetch(&args).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("blocked"));
    }

    #[tokio::test]
    async fn test_web_fetch_missing_url() {
        let args = json!({});
        let result = web_fetch(&args).await;
        assert!(result.is_err());
    }
}
