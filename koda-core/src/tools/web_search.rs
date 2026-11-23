//! WebSearch tool — query the web via DuckDuckGo.
//!
//! Returns the top N results (title, URL, snippet) with no API key required.
//! Uses the DuckDuckGo HTML endpoint.
//!
//! ## Parameters
//!
//! - **`query`** (required) — The search query
//! - **`num_results`** (optional, default 5) — Number of results to return
//!
//! ## Usage patterns
//!
//! - Research a library or API: `"rust tokio graceful shutdown"`
//! - Find documentation: `"fastapi middleware docs"`
//! - Combine with WebFetch: search → pick best URL → fetch full page

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
    let html = response
        .text()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read search response: {e}"))?;

    let results = parse_results(&html, num_results);

    if results.is_empty() {
        if is_bot_check(&html) {
            anyhow::bail!(
                "DuckDuckGo answered with a bot check instead of results, so this search \
                 failed; it does not mean nothing matches. Searching again soon will likely \
                 get the same check. Tell the user, or read a known URL with WebFetch."
            );
        }
        // A bot check comes as 202; anything but 200 without results is a failure.
        if status != reqwest::StatusCode::OK {
            anyhow::bail!("DuckDuckGo returned HTTP {status} and no results");
        }
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
/// Each result's title is an `<a class="result__a" href="…">`. The `href`
/// is the page itself, or (in older pages) a `//duckduckgo.com/l/?uddg=<encoded
/// url>&…` redirect, from which the page's URL is taken. The snippet is the
/// `result__snippet` element between this title and the next one.
fn parse_results(html: &str, max: usize) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let anchors = result_anchor_starts(html);

    for (i, &start) in anchors.iter().enumerate() {
        if results.len() >= max {
            break;
        }
        let scope_end = anchors.get(i + 1).copied().unwrap_or(html.len());
        let block = &html[start..scope_end];

        let Some(tag_end) = block.find('>') else {
            continue;
        };
        let Some(url) = attribute(&block[..tag_end], "href").and_then(|h| result_url(&h)) else {
            continue;
        };

        let after = &block[tag_end + 1..];
        let title_end = after
            .find("</a>")
            .unwrap_or_else(|| after.floor_char_boundary(200));
        let title = strip_inline(&after[..title_end]);
        if title.is_empty() {
            continue;
        }

        let rest = &after[title_end..];
        let snippet = extract_snippet(&rest[..rest.floor_char_boundary(4096)]);

        results.push(SearchResult {
            title,
            url,
            snippet,
        });
    }

    results
}

/// Byte offsets of each `<a …>` tag whose class list includes `result__a`.
fn result_anchor_starts(html: &str) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut from = 0;
    while let Some(rel) = html[from..].find("<a") {
        let start = from + rel;
        from = start + 2;
        if !html[from..].starts_with(|c: char| c.is_ascii_whitespace()) {
            continue;
        }
        let tag_end = html[start..].find('>').map_or(html.len(), |e| start + e);
        let is_result = attribute(&html[start..tag_end], "class")
            .is_some_and(|c| c.split_whitespace().any(|c| c == "result__a"));
        if is_result {
            starts.push(start);
        }
    }
    starts
}

/// The value of `name="…"` in an opening tag.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let pos = tag
        .match_indices(&needle)
        .find(|&(i, _)| tag[..i].ends_with(|c: char| c.is_ascii_whitespace()))?
        .0
        + needle.len();
    let end = tag[pos..].find('"')? + pos;
    Some(tag[pos..end].replace("&amp;", "&"))
}

/// The result's page from a title link: a direct link as it is, a
/// DuckDuckGo redirect by its `uddg` parameter. Anything else (an ad, a
/// DuckDuckGo page) is skipped.
fn result_url(href: &str) -> Option<String> {
    let normalized = if href.starts_with("//") {
        format!("https:{href}")
    } else {
        href.to_string()
    };
    let parsed = url::Url::parse(&normalized).ok()?;
    let target = match parsed.query_pairs().find(|(k, _)| k == "uddg") {
        Some((_, v)) => url::Url::parse(&v).ok()?,
        None => parsed,
    };
    let is_web = matches!(target.scheme(), "http" | "https");
    let is_ddg = target
        .host_str()
        .is_some_and(|h| h == "duckduckgo.com" || h.ends_with(".duckduckgo.com"));
    (is_web && !is_ddg).then(|| target.to_string())
}

/// Whether DuckDuckGo answered with its bot check instead of results.
fn is_bot_check(html: &str) -> bool {
    [
        "anomaly-modal",
        "anomaly.js",
        "challenge-form",
        "g-recaptcha",
    ]
    .iter()
    .any(|marker| html.contains(marker))
}

/// Extract the text of the first `class="result__snippet"` element (an `<a>`
/// in current pages, a `<div>` in older ones).
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
    let end = ["</a>", "</div>"]
        .iter()
        .filter_map(|close| text_region.find(close))
        .min()
        .unwrap_or_else(|| text_region.floor_char_boundary(600));
    let raw = strip_inline(&text_region[..end]);
    if raw.len() > 300 {
        format!("{}…", raw[..raw.floor_char_boundary(297)].trim_end())
    } else {
        raw
    }
}

/// Decode the named entities DuckDuckGo uses and any numeric one
/// (`&#39;`, `&#x27;`); anything else is left as written.
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let decoded = rest[1..].find(';').filter(|&n| n <= 10).and_then(|n| {
            let name = &rest[1..=n];
            let ch = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                _ => {
                    let num = name.strip_prefix('#')?;
                    let code = match num.strip_prefix(['x', 'X']) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                        None => num.parse().ok()?,
                    };
                    char::from_u32(code)
                }
            }?;
            Some((ch, n + 2))
        });
        match decoded {
            Some((ch, len)) => {
                out.push(ch);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
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
    let decoded = decode_entities(&out);
    // Collapse whitespace
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

// =============================================================
// Tool trait implementation (#1265 item 5, PR-7/N).
//
// `WebSearch` is read-only — search results are pure reads.
// =============================================================

use crate::tools::{Tool, ToolEffect, ToolExecCtx, ToolResult};
use async_trait::async_trait;

/// `WebSearch` — query a search engine and return results.
pub struct WebSearchTool;

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &'static str {
        "WebSearch"
    }
    fn definition(&self) -> ToolDefinition {
        definitions()
            .into_iter()
            .find(|d| d.name == "WebSearch")
            .expect("web_search::definitions() must contain WebSearch")
    }
    fn classify(&self, _args: &serde_json::Value) -> ToolEffect {
        ToolEffect::ReadOnly
    }
    async fn execute(&self, _ctx: &ToolExecCtx<'_>, args: &serde_json::Value) -> ToolResult {
        let r = web_search(args).await;
        crate::tools::wrap_result(r.map(|results| crate::untrusted::wrap("WebSearch", &results)))
    }
}

// ── Tests ───────────────────────────────────────────────────

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
    fn extract_snippet_long_non_ascii_text_does_not_panic() {
        for padding in ["", "x", "xx"] {
            let text = format!("{padding}{}", "界".repeat(250));
            let closed = format!(r#"<div class="result__snippet">{text}</div>"#);
            let unclosed = format!(r#"<div class="result__snippet">{text}"#);
            assert!(extract_snippet(&closed).ends_with('…'));
            assert!(!extract_snippet(&unclosed).is_empty());
        }
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

    // ── definitions schema pin ─────────────────────────────────────────

    #[test]
    fn definitions_returns_one_tool() {
        let defs = definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "WebSearch");
    }

    #[test]
    fn definitions_query_is_required() {
        let defs = definitions();
        let required: Vec<&str> = defs[0].parameters["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(required.contains(&"query"));
        // num_results is optional
        assert!(!required.contains(&"num_results"));
    }

    // ── strip_inline edge cases ──────────────────────────────────────

    #[test]
    fn strip_inline_plain_text_unchanged() {
        assert_eq!(strip_inline("hello world"), "hello world");
    }

    #[test]
    fn strip_inline_empty_string() {
        assert_eq!(strip_inline(""), "");
    }

    #[test]
    fn strip_inline_nested_tags() {
        assert_eq!(strip_inline("<b><i>bold italic</i></b>"), "bold italic");
    }

    // ── parse_results edge cases ─────────────────────────────────────

    #[test]
    fn parse_empty_html_returns_no_results() {
        let results = parse_results("", 10);
        assert!(results.is_empty());
    }

    #[test]
    fn parse_zero_max_returns_no_results() {
        let results = parse_results(FIXTURE, 0);
        assert!(results.is_empty());
    }

    /// Today's page: direct links, `<a>` snippets, attributes in any order.
    const DIRECT_FIXTURE: &str = r#"
        <div class="result results_links results_links_deep web-result ">
          <h2 class="result__title">
            <a rel="nofollow" class="result__a" href="https://tokio.rs/tokio/topics/shutdown">Graceful <b>Shutdown</b> | Tokio</a>
          </h2>
          <a class="result__url" href="https://tokio.rs/tokio/topics/shutdown">tokio.rs/tokio/topics/shutdown</a>
          <a class="result__snippet" href="https://tokio.rs/tokio/topics/shutdown">How to <b>shut down</b> &amp; clean up.</a>
        </div>
        <div class="result results_links web-result ">
          <h2 class="result__title">
            <a href="https://docs.rs/tokio-graceful?a=1&amp;b=2"
               class="result__a" rel="nofollow">tokio-graceful</a>
          </h2>
        </div>
    "#;

    #[test]
    fn parse_direct_links() {
        let results = parse_results(DIRECT_FIXTURE, 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Graceful Shutdown | Tokio");
        assert_eq!(results[0].url, "https://tokio.rs/tokio/topics/shutdown");
        assert_eq!(results[0].snippet, "How to shut down & clean up.");
        assert_eq!(results[1].title, "tokio-graceful");
        assert_eq!(results[1].url, "https://docs.rs/tokio-graceful?a=1&b=2");
        assert_eq!(
            results[1].snippet, "",
            "a snippet never comes from another result"
        );
    }

    #[test]
    fn parse_skips_ads_and_duckduckgo_links() {
        let html = r#"
            <a class="result__a" href="//duckduckgo.com/y.js?ad_domain=example.com">Ad</a>
            <a class="result__a" href="/html/?q=more">More</a>
            <a class="result__a" href="javascript:void(0)">Script</a>
            <a class="result__a" href="https://example.org/">Real</a>
        "#;
        let results = parse_results(html, 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://example.org/");
    }

    #[test]
    fn parse_ignores_other_classes_containing_result_a() {
        let html = r#"<a class="result__about" href="https://a.example/">x</a>
            <a class="result__a-like" href="https://b.example/">y</a>"#;
        assert!(parse_results(html, 10).is_empty());
    }

    #[test]
    fn bot_check_is_detected() {
        let page = r#"<form id="challenge-form" action="//duckduckgo.com/anomaly.js?sv=html">
            <div class="anomaly-modal__modal">Select all squares containing a duck</div></form>"#;
        assert!(parse_results(page, 10).is_empty());
        assert!(is_bot_check(page));
        assert!(!is_bot_check(DIRECT_FIXTURE));
        assert!(!is_bot_check(FIXTURE));
    }

    #[test]
    fn non_ascii_titles_do_not_panic() {
        let long = "界".repeat(300);
        let html = format!(r#"<a class="result__a" href="https://example.org/">{long}"#);
        let results = parse_results(&html, 10);
        assert_eq!(results.len(), 1);
        assert!(!results[0].title.is_empty());
    }

    #[test]
    fn entities_decode_numeric_and_named() {
        assert_eq!(
            strip_inline("I&#x27;m &#39;here&#39; &amp; &quot;there&quot;"),
            "I'm 'here' & \"there\""
        );
        assert_eq!(strip_inline("a &amp;amp; b"), "a &amp; b", "decoded once");
        assert_eq!(
            strip_inline("AT&T &bogus; &#xZZ; &"),
            "AT&T &bogus; &#xZZ; &"
        );
        assert_eq!(strip_inline("&#128512;"), "\u{1F600}");
    }
}
