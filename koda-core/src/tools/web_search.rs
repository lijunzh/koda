//! WebSearch tool: query the web via DuckDuckGo.
//!
//! Returns the top N results (title, URL, snippet) with no API key required.
//! Uses the DuckDuckGo HTML endpoint — the same SSRF-safe HTTP client that
//! `web_fetch` uses, but targets a fixed trusted domain so no SSRF checks
//! are needed here.

use crate::providers::ToolDefinition;
use anyhow::Result;
use serde_json::{Value, json};
use std::time::Duration;

const DEFAULT_RESULTS: usize = 5;
const MAX_RESULTS: usize = 10;
const TIMEOUT_SECS: u64 = 15;
const DDG_HTML_URL: &str = "https://html.duckduckgo.com/html/";

// ── Tool definition ────────────────────────────────────────────────────────

/// Return tool definitions for the LLM.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "WebSearch".to_string(),
        description: "Search the web via DuckDuckGo and return the top results (title, URL, \
            snippet). Use when you need current information, documentation, or facts not in \
            your training data. For a full page, follow up with WebFetch on a result URL."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query"
                },
                "num_results": {
                    "type": "integer",
                    "description": "Number of results to return (default: 5, max: 10)"
                }
            },
            "required": ["query"]
        }),
    }]
}

// ── Handler ────────────────────────────────────────────────────────────────

/// Execute a web search and return formatted results.
pub async fn web_search(args: &Value) -> Result<String> {
    let query = args["query"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'query' argument"))?;
    if query.trim().is_empty() {
        anyhow::bail!("'query' must not be empty");
    }

    let num_results = args["num_results"]
        .as_u64()
        .map(|n| n as usize)
        .unwrap_or(DEFAULT_RESULTS)
        .clamp(1, MAX_RESULTS);

    let form_body: String = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("q", query)
        .append_pair("kl", "wt-wt") // worldwide locale, avoids regional results
        .finish();

    static HTTP_CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let client = HTTP_CLIENT
        .get_or_init(|| crate::providers::build_http_client(None))
        .clone();

    let response = tokio::time::timeout(
        Duration::from_secs(TIMEOUT_SECS),
        client
            .post(DDG_HTML_URL)
            .header("Content-Type", "application/x-www-form-urlencoded")
            // Identify as a browser so DDG returns the full HTML result page.
            .header(
                "User-Agent",
                "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0",
            )
            .body(form_body)
            .send(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Search timed out after {TIMEOUT_SECS}s"))?
    .map_err(|e| anyhow::anyhow!("Search request failed: {e}"))?;

    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("DuckDuckGo returned HTTP {status}");
    }

    let html = response
        .text()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read search response: {e}"))?;

    let results = parse_results(&html, num_results);

    if results.is_empty() {
        return Ok(format!(
            "No results found for: \"{query}\"\n\
             Tip: try a more specific query, or use WebFetch to read a known URL directly."
        ));
    }

    let mut out = format!("Web search results for \"{query}\":\n\n");
    for (i, r) in results.iter().enumerate() {
        out.push_str(&format!("{}. {}\n   {}\n", i + 1, r.title, r.url));
        if !r.snippet.is_empty() {
            out.push_str(&format!("   {}\n", r.snippet));
        }
        out.push('\n');
    }

    Ok(out)
}

// ── HTML parsing ───────────────────────────────────────────────────────────

struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

/// Parse DuckDuckGo HTML results.
///
/// Splits on the `uddg=` marker that appears in every result anchor's `href`
/// (`//duckduckgo.com/l/?uddg=<encoded_url>&…`).  Each segment starting from
/// index 1 begins with the percent-encoded actual URL.
fn parse_results(html: &str, max: usize) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let segments: Vec<&str> = html.split("uddg=").collect();

    for segment in segments.iter().skip(1) {
        if results.len() >= max {
            break;
        }

        // URL: everything up to the first &, ", or whitespace
        let url_end = segment
            .find(|c: char| c == '&' || c == '"' || c.is_whitespace())
            .unwrap_or_else(|| segment.len().min(300));
        let url = percent_decode(&segment[..url_end]);

        // Only keep real http/https results — skips ads, disambiguation boxes, etc.
        if !url.starts_with("http") {
            continue;
        }

        // Title: skip past the closing > of the <a …> tag, then read until </a>
        let title = segment
            .find('>')
            .map(|close| {
                let after = &segment[close + 1..];
                let end = after.find("</a>").unwrap_or_else(|| after.len().min(200));
                strip_inline(&after[..end])
            })
            .unwrap_or_default();

        if title.is_empty() {
            continue;
        }

        // Snippet: look within the next ~2 KB of this segment (same result block)
        let window = &segment[..segment.len().min(2048)];
        let snippet = extract_snippet(window);

        results.push(SearchResult {
            title,
            url,
            snippet,
        });
    }

    results
}

/// Extract text from the `class="result__snippet"` element closest to `pos`.
fn extract_snippet(html: &str) -> String {
    let marker = "result__snippet\"";
    let Some(m) = html.find(marker) else {
        return String::new();
    };
    let after_marker = &html[m + marker.len()..];
    // Skip the rest of the opening tag (> closes it)
    let Some(close) = after_marker.find('>') else {
        return String::new();
    };
    let text_region = &after_marker[close + 1..];
    let end = text_region
        .find("</div>")
        .unwrap_or_else(|| text_region.len().min(600));
    let raw = strip_inline(&text_region[..end]);
    if raw.len() > 300 {
        format!("{}…", raw[..297].trim_end())
    } else {
        raw
    }
}

/// Strip inline HTML tags and decode common entities; collapse whitespace.
fn strip_inline(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    let decoded = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");
    // Collapse whitespace
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Percent-decode a URL-encoded string (`%XX` sequences and `+` → space).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = char::from(bytes[i + 1]).to_digit(16);
            let lo = char::from(bytes[i + 2]).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push(char::from(((h * 16) + l) as u8));
                i += 3;
                continue;
            }
        } else if bytes[i] == b'+' {
            out.push(' ');
            i += 1;
            continue;
        }
        out.push(char::from(bytes[i]));
        i += 1;
    }
    out
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Minimal DDG-style HTML with two results.
    const FIXTURE: &str = r#"
        <div class="result__body">
            <h2 class="result__title">
                <a rel="nofollow" class="result__a"
                   href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdocs.rs%2Ftokio&rut=abc">
                    Tokio — async runtime
                </a>
            </h2>
            <div class="result__snippet">
                An async runtime for Rust &amp; friends.
            </div>
        </div>
        <div class="result__body">
            <h2 class="result__title">
                <a rel="nofollow" class="result__a"
                   href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fgithub.com%2Ftokio-rs%2Ftokio&rut=xyz">
                    tokio-rs/tokio on GitHub
                </a>
            </h2>
            <div class="result__snippet">
                The async Rust &lt;runtime&gt;.
            </div>
        </div>
    "#;

    #[test]
    fn parse_two_results() {
        let results = parse_results(FIXTURE, 10);
        assert_eq!(results.len(), 2);

        assert_eq!(results[0].title, "Tokio — async runtime");
        assert_eq!(results[0].url, "https://docs.rs/tokio");
        assert!(results[0].snippet.contains("async runtime"));
        assert!(results[0].snippet.contains('&'), "entity should be decoded");

        assert_eq!(results[1].title, "tokio-rs/tokio on GitHub");
        assert_eq!(results[1].url, "https://github.com/tokio-rs/tokio");
        assert!(results[1].snippet.contains('<'), "lt/gt should be decoded");
    }

    #[test]
    fn parse_respects_max() {
        let results = parse_results(FIXTURE, 1);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Tokio — async runtime");
    }

    #[test]
    fn percent_decode_basic() {
        assert_eq!(percent_decode("hello%20world"), "hello world");
        assert_eq!(
            percent_decode("https%3A%2F%2Fdocs.rs%2Ftokio"),
            "https://docs.rs/tokio"
        );
        assert_eq!(percent_decode("a+b+c"), "a b c");
        assert_eq!(percent_decode("no+encoding"), "no encoding");
    }

    #[test]
    fn percent_decode_invalid_escape_passthrough() {
        // Malformed %XX should be passed through unchanged
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn strip_inline_removes_tags_and_decodes() {
        assert_eq!(strip_inline("<b>hello</b> &amp; world"), "hello & world");
        assert_eq!(strip_inline("  lots  of   spaces  "), "lots of spaces");
    }

    #[test]
    fn extract_snippet_finds_text() {
        let html = r#"<div class="result__snippet">Fast &amp; reliable</div>"#;
        assert_eq!(extract_snippet(html), "Fast & reliable");
    }

    #[test]
    fn extract_snippet_missing_returns_empty() {
        assert_eq!(extract_snippet("<div>no snippet here</div>"), "");
    }

    #[tokio::test]
    async fn missing_query_is_error() {
        let result = web_search(&json!({})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("query"));
    }

    #[tokio::test]
    async fn empty_query_is_error() {
        let result = web_search(&json!({"query": "   "})).await;
        assert!(result.is_err());
    }
}
