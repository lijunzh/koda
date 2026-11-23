//! WebSearch tool — search the web and return the top results.
//!
//! The tool has Claude Code's shape: a query, plus either domains to keep
//! (`allowed_domains`) or domains to leave out (`blocked_domains`).
//!
//! ## Where results come from
//!
//! - **The model provider's own search** on a model that has one (Claude):
//!   a separate request to the same model with Anthropic's search tool, as
//!   Claude Code's `WebSearch` sends it, returning the model's summary and
//!   the links ([`crate::providers::WebSearcher`]).
//! - **Brave Search** otherwise, when `BRAVE_API_KEY` is set, through the Brave Search
//!   API as OpenClaw's Brave provider calls it. Neither Codex nor Claude Code
//!   searches from the client (their searches run on OpenAI's and Anthropic's
//!   servers), so on models without a provider search the key is the
//!   dependable route.
//! - **DuckDuckGo's HTML page** otherwise, which needs no key but often
//!   answers with a bot check; that failure tells the model to suggest a
//!   Brave key.
//!
//! Domain filters go into the query as `site:` operators, which both engines
//! honour, and are checked again on the results.
//!
//! ## Usage patterns
//!
//! - Research a library or API: `"rust tokio graceful shutdown"`
//! - Find documentation: `"fastapi middleware"` with `allowed_domains: ["fastapi.tiangolo.com"]`
//! - Combine with WebFetch: search → pick best URL → fetch full page

use crate::providers::ToolDefinition;
use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

const RESULTS: usize = 5;
/// Results asked of the engine, so domain filtering still leaves `RESULTS`.
const FETCHED: usize = 10;
const TIMEOUT_SECS: u64 = 15;
const DDG_HTML_URL: &str = "https://html.duckduckgo.com/html/";
const BRAVE_URL: &str = "https://api.search.brave.com/res/v1/web/search";
/// The environment variable holding a Brave Search API key, as OpenClaw names it.
pub const BRAVE_KEY_ENV: &str = "BRAVE_API_KEY";
/// Brave rejects queries longer than this many characters.
const BRAVE_MAX_QUERY: usize = 400;

/// Added to every DuckDuckGo failure: the way to search dependably.
const BRAVE_REMINDER: &str = "For dependable search, the user can set the BRAVE_API_KEY \
    environment variable to a Brave Search API key (https://brave.com/search/api/) and \
    restart Koda.";

// ── Tool definition ────────────────────────────────────────────────────────

/// Return tool definitions for the LLM.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "WebSearch".to_string(),
        description: "Search the web and return the top results (title, URL, snippet, and \
            the date when known). Use it for current information, documentation, or facts \
            not in your training data. Results can be limited to some domains or exclude \
            others. For a full page, follow up with WebFetch on a result URL. When results \
            inform your answer, list the URLs you used as sources."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "minLength": 2,
                    "description": "The search query to use"
                },
                "allowed_domains": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Only include results from these domains"
                },
                "blocked_domains": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Never include results from these domains"
                }
            },
            "required": ["query"]
        }),
    }]
}

// ── Request ────────────────────────────────────────────────────────────────

/// A validated search: the model's query and its domain filter.
#[derive(Debug, PartialEq)]
struct SearchRequest {
    query: String,
    allowed: Vec<String>,
    blocked: Vec<String>,
}

impl SearchRequest {
    fn from_args(args: &Value) -> Result<Self> {
        let query = args["query"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing 'query' argument"))?
            .trim();
        if query.chars().count() < 2 {
            anyhow::bail!("'query' must be at least 2 characters");
        }
        let allowed = domains(&args["allowed_domains"], "allowed_domains")?;
        let blocked = domains(&args["blocked_domains"], "blocked_domains")?;
        if !allowed.is_empty() && !blocked.is_empty() {
            anyhow::bail!("Use allowed_domains or blocked_domains, not both");
        }
        Ok(Self {
            query: query.to_string(),
            allowed,
            blocked,
        })
    }

    /// The query sent to the engine: the model's words plus `site:` operators.
    fn engine_query(&self) -> String {
        let mut q = self.query.clone();
        if !self.allowed.is_empty() {
            let sites: Vec<String> = self.allowed.iter().map(|d| format!("site:{d}")).collect();
            if sites.len() == 1 {
                q.push_str(&format!(" {}", sites[0]));
            } else {
                q.push_str(&format!(" ({})", sites.join(" OR ")));
            }
        }
        for d in &self.blocked {
            q.push_str(&format!(" -site:{d}"));
        }
        q
    }

    /// Whether a result's URL passes the domain filter.
    fn keeps(&self, url: &str) -> bool {
        let Some(host) = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        else {
            return false;
        };
        let matches = |d: &String| host == *d || host.ends_with(&format!(".{d}"));
        (self.allowed.is_empty() || self.allowed.iter().any(matches))
            && !self.blocked.iter().any(matches)
    }
}

/// A domain list from the arguments, each reduced to a lowercase host
/// (`https://Docs.rs/tokio` → `docs.rs`).
fn domains(value: &Value, name: &str) -> Result<Vec<String>> {
    let items = match value {
        Value::Null => return Ok(Vec::new()),
        Value::Array(items) => items,
        _ => anyhow::bail!("'{name}' must be a list of domains"),
    };
    let mut out = Vec::new();
    for item in items {
        let raw = item
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'{name}' must be a list of domains"))?
            .trim();
        let host = if raw.contains("://") {
            url::Url::parse(raw)
                .ok()
                .and_then(|u| u.host_str().map(String::from))
        } else {
            raw.split('/').next().map(String::from)
        }
        .map(|h| {
            h.trim_start_matches("www.")
                .trim_matches('.')
                .to_ascii_lowercase()
        })
        .filter(|h| !h.is_empty() && !h.contains(char::is_whitespace));
        match host {
            Some(h) if !out.contains(&h) => out.push(h),
            Some(_) => {}
            None => anyhow::bail!("'{name}' has an entry that isn't a domain: {raw:?}"),
        }
    }
    Ok(out)
}

// ── Handler ────────────────────────────────────────────────────────────────

/// Execute a web search and return formatted results: with the model
/// provider's own search when `provider` is set (Claude), else Brave or
/// DuckDuckGo.
pub async fn web_search(
    args: &Value,
    provider: Option<&dyn crate::providers::WebSearcher>,
) -> Result<String> {
    let request = SearchRequest::from_args(args)?;
    Ok(search(&request, provider).await?.0)
}

/// Run a validated search: the formatted results, and what the provider's
/// search request used when it made one.
async fn search(
    request: &SearchRequest,
    provider: Option<&dyn crate::providers::WebSearcher>,
) -> Result<(String, Option<crate::engine::UsageTotals>)> {
    if let Some(searcher) = provider {
        let answer = searcher
            .search(&request.query, &request.allowed, &request.blocked)
            .await?;
        let usage = crate::engine::UsageTotals {
            context_tokens: answer.context_tokens,
            web_search_requests: answer.web_search_requests,
            ..(&answer.usage).into()
        };
        return Ok((
            format_provider_results(&request.query, &answer.parts),
            Some(usage),
        ));
    }

    static HTTP_CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let client = HTTP_CLIENT
        .get_or_init(|| crate::providers::build_http_client(None))
        .clone();

    let found = match brave_key() {
        Some(key) => brave_search(&client, BRAVE_URL, &key, &request.engine_query()).await?,
        None => duckduckgo_search(&client, DDG_HTML_URL, &request.engine_query()).await?,
    };
    let results: Vec<SearchResult> = found
        .into_iter()
        .filter(|r| request.keeps(&r.url))
        .take(RESULTS)
        .collect();
    Ok((format_results(&request.query, &results), None))
}

/// The Brave Search API key, from the environment or Koda's key store.
fn brave_key() -> Option<String> {
    crate::runtime_env::get(BRAVE_KEY_ENV)
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
}

/// A provider search's answer as Claude Code's `WebSearch` returns it: the
/// searching model's summary, each search's links as JSON, and a reminder
/// to cite them.
fn format_provider_results(query: &str, parts: &[crate::providers::SearchPart]) -> String {
    use crate::providers::SearchPart;
    let mut out = format!("Web search results for query: \"{query}\"\n\n");
    for part in parts {
        match part {
            SearchPart::Text(text) => out.push_str(&format!("{text}\n")),
            SearchPart::Links(links) if links.is_empty() => out.push_str("No links found.\n"),
            SearchPart::Links(links) => out.push_str(&format!(
                "Links: {}\n",
                serde_json::to_string(links).unwrap_or_default()
            )),
            SearchPart::Error(code) => out.push_str(&format!("Web search error: {code}\n")),
        }
    }
    if parts.is_empty() {
        out.push_str("No links found.\n");
    }
    out.push_str(
        "\nREMINDER: You MUST include the sources above in your response to the user using \
         markdown hyperlinks.",
    );
    out
}

fn format_results(query: &str, results: &[SearchResult]) -> String {
    if results.is_empty() {
        return format!(
            "No results found for: \"{query}\"\n\
             Tip: try a more specific query, or use WebFetch to read a known URL directly."
        );
    }
    let mut out = format!("Web search results for \"{query}\":\n\n");
    for (i, r) in results.iter().enumerate() {
        out.push_str(&format!("{}. {}\n   {}\n", i + 1, r.title, r.url));
        if let Some(age) = &r.age {
            out.push_str(&format!("   Published: {age}\n"));
        }
        if !r.snippet.is_empty() {
            out.push_str(&format!("   {}\n", r.snippet));
        }
        out.push('\n');
    }
    out
}

// ── Brave ──────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct BraveResponse {
    web: Option<BraveWeb>,
}

#[derive(Deserialize)]
struct BraveWeb {
    #[serde(default)]
    results: Vec<BraveResult>,
}

#[derive(Deserialize)]
struct BraveResult {
    title: String,
    url: String,
    #[serde(default)]
    description: String,
    age: Option<String>,
}

/// Search with the Brave Search API at `endpoint`.
async fn brave_search(
    client: &reqwest::Client,
    endpoint: &str,
    key: &str,
    query: &str,
) -> Result<Vec<SearchResult>> {
    if query.chars().count() > BRAVE_MAX_QUERY {
        anyhow::bail!(
            "The query is too long for Brave Search (at most {BRAVE_MAX_QUERY} characters, \
             domain filters included); shorten it"
        );
    }
    let mut url = url::Url::parse(endpoint)
        .map_err(|e| anyhow::anyhow!("Bad Brave Search endpoint {endpoint}: {e}"))?;
    url.query_pairs_mut()
        .append_pair("q", query)
        .append_pair("count", &FETCHED.to_string())
        .append_pair("text_decorations", "false");
    let response = tokio::time::timeout(
        Duration::from_secs(TIMEOUT_SECS),
        client
            .get(url)
            .header("Accept", "application/json")
            .header("User-Agent", crate::tools::USER_AGENT)
            .header("X-Subscription-Token", key)
            .send(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Brave Search timed out after {TIMEOUT_SECS}s"))?
    .map_err(|e| anyhow::anyhow!("Brave Search request failed: {e}"))?;

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read the Brave Search response: {e}"))?;
    match status.as_u16() {
        200 => {}
        401 | 403 => anyhow::bail!(
            "Brave Search rejected the key in {BRAVE_KEY_ENV} (HTTP {status}). Tell the user \
             to check the key."
        ),
        429 => anyhow::bail!(
            "Brave Search refused the request (HTTP 429): too many searches at once, or the \
             account's credit is used up. Searching again right away will likely fail too; \
             tell the user, or read a known URL with WebFetch."
        ),
        _ => {
            let detail: String = body.chars().take(300).collect();
            anyhow::bail!("Brave Search returned HTTP {status}: {detail}");
        }
    }
    parse_brave(&body)
}

/// The web results in a Brave Search API response.
fn parse_brave(body: &str) -> Result<Vec<SearchResult>> {
    let parsed: BraveResponse = serde_json::from_str(body)
        .map_err(|e| anyhow::anyhow!("Brave Search sent a response Koda can't read: {e}"))?;
    Ok(parsed
        .web
        .map(|w| w.results)
        .unwrap_or_default()
        .into_iter()
        .filter(|r| url::Url::parse(&r.url).is_ok_and(|u| matches!(u.scheme(), "http" | "https")))
        .map(|r| SearchResult {
            title: strip_inline(&r.title),
            url: r.url,
            snippet: shorten(strip_inline(&r.description)),
            age: r.age.map(|a| strip_inline(&a)).filter(|a| !a.is_empty()),
        })
        .filter(|r| !r.title.is_empty())
        .collect())
}

// ── DuckDuckGo ─────────────────────────────────────────────────────────────

/// Search DuckDuckGo's HTML page at `endpoint`. Every failure ends with
/// [`BRAVE_REMINDER`], since this runs only when no Brave key is set.
async fn duckduckgo_search(
    client: &reqwest::Client,
    endpoint: &str,
    query: &str,
) -> Result<Vec<SearchResult>> {
    let form_body: String = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("q", query)
        .append_pair("kl", "wt-wt") // worldwide locale, avoids regional results
        .finish();

    let response = tokio::time::timeout(
        Duration::from_secs(TIMEOUT_SECS),
        client
            .post(endpoint)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("User-Agent", crate::tools::USER_AGENT)
            .body(form_body)
            .send(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Search timed out after {TIMEOUT_SECS}s. {BRAVE_REMINDER}"))?
    .map_err(|e| anyhow::anyhow!("Search request failed: {e}. {BRAVE_REMINDER}"))?;

    let status = response.status();
    let html = response
        .text()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read search response: {e}. {BRAVE_REMINDER}"))?;

    let results = parse_results(&html, FETCHED);
    if results.is_empty() {
        if is_bot_check(&html) {
            anyhow::bail!(
                "DuckDuckGo answered with a bot check instead of results, so this search \
                 failed; it does not mean nothing matches. Searching again soon will likely \
                 get the same check. Tell the user, or read a known URL with WebFetch. \
                 {BRAVE_REMINDER}"
            );
        }
        // A bot check comes as 202; anything but 200 without results is a failure.
        if status != reqwest::StatusCode::OK {
            anyhow::bail!("DuckDuckGo returned HTTP {status} and no results. {BRAVE_REMINDER}");
        }
    }
    Ok(results)
}

// ── HTML parsing ───────────────────────────────────────────────────────────

#[derive(Debug)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
    /// When the page was published, as the engine words it ("March 6, 2025").
    age: Option<String>,
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
            age: None,
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
    shorten(strip_inline(&text_region[..end]))
}

/// A snippet cut to about 300 bytes on a character boundary.
fn shorten(raw: String) -> String {
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
    async fn execute(&self, ctx: &ToolExecCtx<'_>, args: &serde_json::Value) -> ToolResult {
        let request = match SearchRequest::from_args(args) {
            Ok(request) => request,
            Err(e) => return crate::tools::wrap_result(Err(e)),
        };
        // Claude Code checks its budget once the input is valid and before
        // it picks how to search, so every backend draws on it.
        if let Some(budget) = ctx.web_search_budget
            && !budget.try_consume()
        {
            return crate::tools::wrap_result(Ok(budget.refusal()));
        }
        let r = search(&request, ctx.web_searcher.as_deref()).await;
        // The search request counts toward the turn and the session, as the
        // goal judge's and compaction's calls do.
        if let (Ok((_, Some(usage))), Some((sink, _))) = (&r, ctx.sink) {
            sink.emit(crate::engine::EngineEvent::ModelUsage {
                usage: usage.clone(),
            });
        }
        crate::tools::wrap_result(
            r.map(|(results, _)| crate::untrusted::wrap("WebSearch", &results)),
        )
    }
}

// ── Session budget (Claude Code's per-session web search cap) ──────────────

/// `WebSearch` calls a session may make when `max_web_searches_per_session`
/// is unset, as Claude Code's `CLAUDE_CODE_MAX_WEB_SEARCHES_PER_SESSION`
/// defaults.
pub const DEFAULT_MAX_WEB_SEARCHES_PER_SESSION: u32 = 200;
/// Calls the budget gets back per hour in a session a person attends when
/// `web_search_refills_per_hour` is unset, as in Claude Code's interactive
/// sessions. A session nobody attends (`koda exec`) gets none back, as
/// `claude -p` gets none.
pub const DEFAULT_WEB_SEARCH_REFILLS_PER_HOUR: u32 = 100;
const HOUR: Duration = Duration::from_secs(3600);

/// How many `WebSearch` calls a session has left, shared by every agent in
/// it, as Claude Code's session budget is: up to `max` calls, with one
/// coming back every hour / `refills_per_hour` since the budget was last
/// used, and all of them once it has refilled to empty. A new session
/// (`/new`, `/clear`) starts a new budget.
#[derive(Debug)]
pub struct WebSearchBudget {
    max: u32,
    refills_per_hour: u32,
    state: std::sync::Mutex<BudgetState>,
}

#[derive(Debug)]
struct BudgetState {
    /// Calls used and not yet refilled.
    used: u32,
    /// Time accrued toward the next refill, times `refills_per_hour`.
    accrued: u128,
    /// When the budget was last brought up to date.
    last: std::time::Instant,
}

impl Default for WebSearchBudget {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_WEB_SEARCHES_PER_SESSION, 0)
    }
}

impl WebSearchBudget {
    /// A budget of `max` calls that refills `refills_per_hour` an hour.
    pub fn new(max: u32, refills_per_hour: u32) -> Self {
        Self {
            max,
            refills_per_hour,
            state: std::sync::Mutex::new(BudgetState {
                used: 0,
                accrued: 0,
                last: std::time::Instant::now(),
            }),
        }
    }

    /// Take one call from the budget; `false`, taking nothing, when it's
    /// used up.
    pub fn try_consume(&self) -> bool {
        self.try_consume_at(std::time::Instant::now())
    }

    fn try_consume_at(&self, now: std::time::Instant) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.refill(&mut state, now);
        if state.used >= self.max {
            return false;
        }
        state.used += 1;
        true
    }

    /// Give back the calls the time since the last update has earned,
    /// as Claude Code's budget does.
    fn refill(&self, state: &mut BudgetState, now: std::time::Instant) {
        let elapsed = now.saturating_duration_since(state.last);
        state.last = now;
        if state.used == 0 {
            state.accrued = 0;
            return;
        }
        state.accrued += elapsed.as_millis() * u128::from(self.refills_per_hour);
        let back = state.accrued / HOUR.as_millis();
        if back >= u128::from(state.used) {
            state.used = 0;
            state.accrued = 0;
            return;
        }
        state.used -= back as u32;
        state.accrued -= back * HOUR.as_millis();
    }

    /// What the tool answers once the budget is used up, in Claude Code's
    /// words, naming Koda's setting where Claude Code names its variable.
    pub fn refusal(&self) -> String {
        let calls = |n: u32| if n == 1 { "call" } else { "calls" };
        let limit = format!("{} WebSearch {}", self.max, calls(self.max));
        let (limit, next) = if self.refills_per_hour > 0 {
            (
                format!(
                    "limit: {limit} shared by every agent in this session; the budget refills \
                     at about {} {} per hour, so searching becomes possible again later in the \
                     session",
                    self.refills_per_hour,
                    calls(self.refills_per_hour)
                ),
                "Do not pause or wait for the refill: continue with the information already \
                 gathered and say so in your answer",
            )
        } else {
            (
                format!(
                    "limit: {limit}, shared by every agent in this session; it does not refill"
                ),
                "Continue with the information already gathered instead of searching again, \
                 and say so in your answer",
            )
        };
        format!(
            "Web search was not performed: this session's web search budget is used up \
             ({limit}). {next}; do not work around the limit by querying search engines with \
             curl or wget from Bash, or by searching or fetching through a third-party reader, \
             proxy or archive service. If more searches are genuinely needed, ask the user to \
             raise max_web_searches_per_session in Koda's config.toml."
        )
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missing_query_is_error() {
        let result = web_search(&json!({}), None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("query"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_query_is_error() {
        let result = web_search(&json!({"query": "   "}), None).await;
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
        assert_eq!(required, ["query"]);
    }

    #[test]
    fn definitions_have_claude_codes_shape() {
        let props = &definitions()[0].parameters["properties"];
        let mut names: Vec<&str> = props
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        names.sort_unstable();
        assert_eq!(names, ["allowed_domains", "blocked_domains", "query"]);
        assert_eq!(props["query"]["minLength"], 2);
        assert!(!definitions()[0].description.contains("DuckDuckGo"));
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
    // ── Request and domain filters ───────────────────────────────────

    #[test]
    fn short_query_is_error() {
        let err = SearchRequest::from_args(&json!({"query": " a "})).unwrap_err();
        assert!(err.to_string().contains("at least 2"), "{err}");
    }

    #[test]
    fn domains_are_reduced_to_hosts() {
        let r = SearchRequest::from_args(&json!({
            "query": "tokio",
            "allowed_domains": ["https://Docs.rs/tokio", "www.tokio.rs", "docs.rs", "crates.io/"]
        }))
        .unwrap();
        assert_eq!(r.allowed, ["docs.rs", "tokio.rs", "crates.io"]);
    }

    #[test]
    fn both_domain_lists_is_error() {
        let err = SearchRequest::from_args(&json!({
            "query": "tokio", "allowed_domains": ["a.com"], "blocked_domains": ["b.com"]
        }))
        .unwrap_err();
        assert!(err.to_string().contains("not both"), "{err}");
    }

    #[test]
    fn bad_domain_entries_are_errors() {
        for bad in [
            json!("docs.rs"),
            json!([3]),
            json!(["two words"]),
            json!([""]),
        ] {
            let args = json!({"query": "tokio", "blocked_domains": bad});
            assert!(SearchRequest::from_args(&args).is_err(), "{args}");
        }
    }

    #[test]
    fn engine_query_adds_site_operators() {
        let one = SearchRequest::from_args(&json!({"query": "q x", "allowed_domains": ["a.com"]}));
        assert_eq!(one.unwrap().engine_query(), "q x site:a.com");
        let two = SearchRequest::from_args(
            &json!({"query": "qq", "allowed_domains": ["a.com", "b.org"]}),
        );
        assert_eq!(two.unwrap().engine_query(), "qq (site:a.com OR site:b.org)");
        let blocked = SearchRequest::from_args(
            &json!({"query": "qq", "blocked_domains": ["a.com", "b.org"]}),
        );
        assert_eq!(
            blocked.unwrap().engine_query(),
            "qq -site:a.com -site:b.org"
        );
        let none = SearchRequest::from_args(&json!({"query": "plain"}));
        assert_eq!(none.unwrap().engine_query(), "plain");
    }

    #[test]
    fn filter_keeps_subdomains_and_drops_lookalikes() {
        let allowed =
            SearchRequest::from_args(&json!({"query": "qq", "allowed_domains": ["docs.rs"]}))
                .unwrap();
        assert!(allowed.keeps("https://docs.rs/tokio"));
        assert!(allowed.keeps("https://api.Docs.rs/x"));
        assert!(!allowed.keeps("https://notdocs.rs/x"));
        assert!(!allowed.keeps("https://docs.rs.evil.com/x"));
        assert!(!allowed.keeps("not a url"));
        let blocked =
            SearchRequest::from_args(&json!({"query": "qq", "blocked_domains": ["github.com"]}))
                .unwrap();
        assert!(!blocked.keeps("https://gist.github.com/x"));
        assert!(blocked.keeps("https://gitlab.com/x"));
    }

    // ── Brave ─────────────────────────────────────────────────────────

    const BRAVE_FIXTURE: &str = r#"{
        "type": "search",
        "web": {"results": [
            {"title": "Graceful Shutdown | Tokio", "url": "https://tokio.rs/tokio/topics/shutdown",
             "description": "How to <strong>shut down</strong> &amp; clean up.", "age": "March 6, 2025"},
            {"title": "tokio-graceful", "url": "https://docs.rs/tokio-graceful"},
            {"title": "Not the web", "url": "ftp://example.org/file"}
        ]}
    }"#;

    #[test]
    fn parse_brave_results() {
        let results = parse_brave(BRAVE_FIXTURE).unwrap();
        assert_eq!(results.len(), 2, "the ftp result is dropped");
        assert_eq!(results[0].title, "Graceful Shutdown | Tokio");
        assert_eq!(results[0].snippet, "How to shut down & clean up.");
        assert_eq!(results[0].age.as_deref(), Some("March 6, 2025"));
        assert_eq!(results[1].snippet, "");
        assert_eq!(results[1].age, None);
        assert!(parse_brave(r#"{"type":"search"}"#).unwrap().is_empty());
        assert!(parse_brave("not json").is_err());
    }

    #[test]
    fn format_shows_the_date_when_known() {
        let out = format_results("tokio", &parse_brave(BRAVE_FIXTURE).unwrap());
        assert!(out.contains("1. Graceful Shutdown | Tokio\n   https://tokio.rs/tokio/topics/shutdown\n   Published: March 6, 2025\n"), "{out}");
        assert!(format_results("x", &[]).starts_with("No results found"));
    }

    /// What the stub server saw of one request.
    #[derive(Clone, Default)]
    struct Seen {
        query: Option<String>,
        body: String,
        headers: axum::http::HeaderMap,
    }

    /// A server answering every request with `status` and `body`, keeping
    /// the last request it saw.
    async fn stub(
        status: u16,
        body: &'static str,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Seen>>) {
        use axum::{
            extract::State,
            http::{HeaderMap, StatusCode, Uri},
            routing::any,
        };
        type Shared = std::sync::Arc<std::sync::Mutex<Seen>>;
        let seen: Shared = Default::default();
        async fn handle(
            State((seen, status, body)): State<(Shared, u16, &'static str)>,
            uri: Uri,
            headers: HeaderMap,
            text: String,
        ) -> (StatusCode, &'static str) {
            *seen.lock().unwrap() = Seen {
                query: uri.query().map(String::from),
                body: text,
                headers,
            };
            (StatusCode::from_u16(status).unwrap(), body)
        }
        let app = axum::Router::new()
            .route("/{*path}", any(handle))
            .with_state((seen.clone(), status, body));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/search", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.ok() });
        (url, seen)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn brave_sends_key_query_and_honest_agent() {
        let (url, seen) = stub(200, BRAVE_FIXTURE).await;
        let results = brave_search(&reqwest::Client::new(), &url, "k3y", "tokio site:tokio.rs")
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.headers["x-subscription-token"], "k3y");
        let agent = seen.headers["user-agent"].to_str().unwrap();
        assert!(agent.starts_with("Koda/"), "{agent}");
        assert!(!agent.contains("Mozilla"), "{agent}");
        let query = seen.query.unwrap();
        assert!(query.contains("q=tokio+site%3Atokio.rs"), "{query}");
        assert!(query.contains("count=10"), "{query}");
        assert!(query.contains("text_decorations=false"), "{query}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn brave_errors_say_what_to_do() {
        for (status, expect) in [(401, "check the key"), (429, "credit"), (500, "HTTP 500")] {
            let (url, _) = stub(status, r#"{"error":"x"}"#).await;
            let err = brave_search(&reqwest::Client::new(), &url, "SECRET-KEY-123", "tokio")
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains(expect), "{status}: {err}");
            assert!(
                !err.contains("SECRET-KEY-123"),
                "the key is never echoed: {err}"
            );
        }
        let long = "x".repeat(BRAVE_MAX_QUERY + 1);
        let err = brave_search(&reqwest::Client::new(), "http://127.0.0.1:9", "k", &long).await;
        assert!(err.unwrap_err().to_string().contains("too long"));
    }

    // ── DuckDuckGo ────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn duckduckgo_failures_suggest_a_brave_key() {
        let page =
            r#"<form id="challenge-form"><div class="anomaly-modal__modal">duck</div></form>"#;
        for (status, body, expect) in [(202, page, "bot check"), (503, "busy", "HTTP 503")] {
            let (url, _) = stub(status, body).await;
            let err = duckduckgo_search(&reqwest::Client::new(), &url, "tokio")
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains(expect), "{err}");
            assert!(err.contains("BRAVE_API_KEY"), "{err}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn duckduckgo_sends_honest_agent() {
        let (url, seen) = stub(200, DIRECT_FIXTURE).await;
        let results = duckduckgo_search(&reqwest::Client::new(), &url, "tokio shutdown")
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        let seen = seen.lock().unwrap().clone();
        let agent = seen.headers["user-agent"].to_str().unwrap();
        assert!(
            agent.starts_with("Koda/") && !agent.contains("Firefox"),
            "{agent}"
        );
        assert!(seen.body.contains("q=tokio+shutdown"), "{}", seen.body);
    }
    // ── Provider search (Claude) ──────────────────────────────────────

    /// A query with its allowed and blocked domains.
    type Asked = (String, Vec<String>, Vec<String>);

    /// A searcher that records what it was asked and answers `parts`.
    struct FakeSearcher {
        parts: Vec<crate::providers::SearchPart>,
        asked: std::sync::Mutex<Vec<Asked>>,
    }

    #[async_trait]
    impl crate::providers::WebSearcher for FakeSearcher {
        async fn search(
            &self,
            query: &str,
            allowed: &[String],
            blocked: &[String],
        ) -> Result<crate::providers::SearchAnswer> {
            self.asked
                .lock()
                .unwrap()
                .push((query.into(), allowed.to_vec(), blocked.to_vec()));
            Ok(crate::providers::SearchAnswer {
                parts: self.parts.clone(),
                usage: crate::providers::TokenUsage {
                    prompt_tokens: 2000,
                    completion_tokens: 300,
                    cache_read_tokens: 50,
                    ..Default::default()
                },
                context_tokens: 2050,
                web_search_requests: 2,
            })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn provider_search_is_used_with_domains_as_given() {
        use crate::providers::{SearchLink, SearchPart};
        let fake = FakeSearcher {
            parts: vec![
                SearchPart::Links(vec![SearchLink {
                    title: "Announcing Rust 1.85.0".into(),
                    url: "https://blog.rust-lang.org/2025/02/20/Rust-1.85.0/".into(),
                }]),
                SearchPart::Text("Stable on February 20, 2025.".into()),
            ],
            asked: Default::default(),
        };
        let out = web_search(
            &json!({"query": "rust 2024 edition", "allowed_domains": ["https://blog.rust-lang.org/x"]}),
            Some(&fake),
        )
        .await
        .unwrap();
        assert_eq!(
            fake.asked.lock().unwrap()[0],
            (
                "rust 2024 edition".into(),
                vec!["blog.rust-lang.org".into()],
                vec![]
            ),
            "the query goes without site: operators; the API filters domains itself"
        );
        assert_eq!(
            out,
            "Web search results for query: \"rust 2024 edition\"\n\n\
             Links: [{\"title\":\"Announcing Rust 1.85.0\",\"url\":\"https://blog.rust-lang.org/2025/02/20/Rust-1.85.0/\"}]\n\
             Stable on February 20, 2025.\n\n\
             REMINDER: You MUST include the sources above in your response to the user using markdown hyperlinks."
        );
    }

    #[test]
    fn provider_results_name_errors_and_empty_searches() {
        use crate::providers::SearchPart;
        let out = format_provider_results(
            "q",
            &[
                SearchPart::Error("too_many_requests".into()),
                SearchPart::Links(vec![]),
            ],
        );
        assert!(
            out.contains("Web search error: too_many_requests\nNo links found.\n"),
            "{out}"
        );
        assert!(format_provider_results("q", &[]).contains("No links found."));
    }

    // ── Usage and the session budget ──────────────────────────────────

    /// A sink that keeps every event.
    #[derive(Default)]
    struct Kept(std::sync::Mutex<Vec<crate::engine::EngineEvent>>);

    impl crate::engine::EngineSink for Kept {
        fn emit(&self, event: crate::engine::EngineEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    impl Kept {
        fn usage(&self) -> Vec<crate::engine::UsageTotals> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter_map(|e| match e {
                    crate::engine::EngineEvent::ModelUsage { usage } => Some(usage.clone()),
                    _ => None,
                })
                .collect()
        }
    }

    /// A registry whose `WebSearch` searches with a fake provider search,
    /// against `budget`.
    fn registry_with(
        fake: std::sync::Arc<FakeSearcher>,
        budget: std::sync::Arc<WebSearchBudget>,
    ) -> crate::tools::ToolRegistry {
        let registry = crate::tools::ToolRegistry::new(std::env::temp_dir(), 100_000)
            .with_web_search_budget(budget);
        registry.set_web_searcher(Some(fake));
        registry
    }

    fn fake() -> std::sync::Arc<FakeSearcher> {
        std::sync::Arc::new(FakeSearcher {
            parts: vec![crate::providers::SearchPart::Text("found".into())],
            asked: Default::default(),
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_provider_search_reports_its_usage() {
        let registry = registry_with(fake(), Default::default());
        let sink = Kept::default();
        let r = registry
            .execute(
                "WebSearch",
                r#"{"query":"rust 2024"}"#,
                Some((&sink, "call-1")),
                None,
            )
            .await;
        assert!(r.success, "{}", r.output);
        assert_eq!(
            sink.usage(),
            vec![crate::engine::UsageTotals {
                prompt_tokens: 2000,
                completion_tokens: 300,
                cache_read_tokens: 50,
                api_calls: 1,
                context_tokens: 2050,
                web_search_requests: 2,
                ..Default::default()
            }]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_usage_reaches_the_turns_meter_and_budget() {
        let registry = registry_with(fake(), Default::default());
        let inner = Kept::default();
        let meter = crate::engine::sink::TurnMeter::with_budget(&inner, Some(20_000));
        let r = registry
            .execute(
                "WebSearch",
                r#"{"query":"rust 2024"}"#,
                Some((&meter, "call-1")),
                None,
            )
            .await;
        assert!(r.success, "{}", r.output);
        let usage = meter.usage();
        assert_eq!(usage.api_calls, 1);
        assert_eq!(usage.web_search_requests, 2);
        assert_eq!(usage.completion_tokens, 300);
        assert_eq!(inner.usage().len(), 1, "passed on to the client");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_spent_budget_answers_without_searching() {
        let searcher = fake();
        let budget = std::sync::Arc::new(WebSearchBudget::new(2, 0));
        let registry = registry_with(searcher.clone(), budget.clone());
        let sink = Kept::default();
        let search =
            |args: &'static str| registry.execute("WebSearch", args, Some((&sink, "c")), None);

        // An invalid call takes nothing from the budget, as Claude Code
        // checks its input first.
        assert!(!search(r#"{"query":"x"}"#).await.success);
        assert!(search(r#"{"query":"one"}"#).await.success);
        assert!(search(r#"{"query":"two"}"#).await.success);
        let refused = search(r#"{"query":"three"}"#).await;
        assert!(
            refused.success,
            "an answer, not an error, as in Claude Code"
        );
        assert_eq!(refused.output, budget.refusal());
        assert_eq!(
            searcher.asked.lock().unwrap().len(),
            2,
            "the third never searched"
        );
        assert_eq!(sink.usage().len(), 2, "a refused call uses nothing");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_budget_covers_every_backend() {
        // No provider search: the call would go to Brave or DuckDuckGo, but
        // the spent budget stops it first, as Claude Code checks it before
        // picking how to search.
        let budget = std::sync::Arc::new(WebSearchBudget::new(0, 0));
        let registry = crate::tools::ToolRegistry::new(std::env::temp_dir(), 100_000)
            .with_web_search_budget(budget.clone());
        let r = registry
            .execute("WebSearch", r#"{"query":"rust 2024"}"#, None, None)
            .await;
        assert_eq!(r.output, budget.refusal());
    }

    #[test]
    fn the_budget_refills_as_claude_codes_does() {
        use std::time::Instant;
        // 100 an hour: one call back every 36 seconds.
        let budget = WebSearchBudget::new(3, 100);
        let start = Instant::now();
        let at = |secs: u64| start + Duration::from_secs(secs);
        budget.state.lock().unwrap().last = start;
        assert!(budget.try_consume_at(at(0)));
        assert!(budget.try_consume_at(at(0)));
        assert!(budget.try_consume_at(at(0)));
        assert!(!budget.try_consume_at(at(35)), "nothing back yet");
        assert!(budget.try_consume_at(at(36)), "one back after 36s");
        assert!(!budget.try_consume_at(at(37)));
        // Time accrued toward the next refill carries over: 35s here plus
        // 1s from before.
        assert!(budget.try_consume_at(at(72)));
        // Long enough to refill everything: the whole budget is back.
        let later = at(72 + 3600);
        assert!(budget.try_consume_at(later));
        assert!(budget.try_consume_at(later));
        assert!(budget.try_consume_at(later));
        assert!(!budget.try_consume_at(later));
    }

    #[test]
    fn a_budget_without_refills_stays_spent() {
        let budget = WebSearchBudget::new(1, 0);
        let start = std::time::Instant::now();
        budget.state.lock().unwrap().last = start;
        assert!(budget.try_consume_at(start));
        assert!(!budget.try_consume_at(start + Duration::from_secs(10 * 3600)));
    }

    #[test]
    fn the_refusal_has_claude_codes_words() {
        assert_eq!(
            WebSearchBudget::new(200, 0).refusal(),
            "Web search was not performed: this session's web search budget is used up \
             (limit: 200 WebSearch calls, shared by every agent in this session; it does not \
             refill). Continue with the information already gathered instead of searching \
             again, and say so in your answer; do not work around the limit by querying search \
             engines with curl or wget from Bash, or by searching or fetching through a \
             third-party reader, proxy or archive service. If more searches are genuinely \
             needed, ask the user to raise max_web_searches_per_session in Koda's config.toml."
        );
        assert_eq!(
            WebSearchBudget::new(1, 1).refusal(),
            "Web search was not performed: this session's web search budget is used up \
             (limit: 1 WebSearch call shared by every agent in this session; the budget \
             refills at about 1 call per hour, so searching becomes possible again later in \
             the session). Do not pause or wait for the refill: continue with the information \
             already gathered and say so in your answer; do not work around the limit by \
             querying search engines with curl or wget from Bash, or by searching or fetching \
             through a third-party reader, proxy or archive service. If more searches are \
             genuinely needed, ask the user to raise max_web_searches_per_session in Koda's \
             config.toml."
        );
    }
}
