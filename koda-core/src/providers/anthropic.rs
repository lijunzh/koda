//! Anthropic Claude provider.
//!
//! Implements the Claude Messages API which differs from OpenAI's format:
//! - Different auth header (x-api-key instead of Bearer)
//! - Different message/tool call structure
//! - System prompt is a top-level field, not a message

use super::{
    ChatMessage, LlmProvider, LlmResponse, ModelInfo, StreamChunk, TokenUsage, ToolCall,
    ToolDefinition,
};
use crate::model_alias::{ClaudeLine, ClaudeLineIds};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Sent on every request, the Models API included. The `2026-10-21`
/// migration guide changes only the Messages API (with token counting and
/// Message Batches) and Admin API responses; `/v1/models` documents `line`,
/// `max_input_tokens` and `max_tokens` without a version note, so no bump
/// is needed to read them. Bumping this for chat would be its own
/// migration: `2026-10-21` rejects `temperature` / `top_p` / `top_k`.
const ANTHROPIC_API_VERSION: &str = "2023-06-01";
const ANTHROPIC_BETA_FEATURES: &str = "prompt-caching-2024-07-31";

/// Beta header value for 1M extended context.
/// See: https://docs.anthropic.com/en/docs/about-claude/models#extended-context
const EXTENDED_CONTEXT_BETA: &str = "context-1m-2025-08-07";

/// Virtual model name suffix that opts into 1M extended context.
const EXTENDED_CONTEXT_SUFFIX: &str = "-1m";

/// Models that need the beta header to get 1M context. Claude 4.6 and
/// later have 1M by default (see
/// [`crate::model_context::has_native_1m_context`]), so only Sonnet 4.5
/// and the dated Sonnet 4 snapshots remain here.
const EXTENDED_CONTEXT_ELIGIBLE: &[&str] = &[
    "claude-sonnet-4-5",
    "claude-sonnet-4-2", // dated variant
    "claude-sonnet-4",
];

/// Check whether a base model ID (without `-1m` suffix) takes the
/// extended-context beta header. False for models whose window is
/// already 1M, so `/model` doesn't list a pointless `-1m` variant.
fn is_extended_context_eligible(base_model: &str) -> bool {
    if crate::model_context::has_native_1m_context(base_model) {
        return false;
    }
    let m = base_model.to_lowercase();
    EXTENDED_CONTEXT_ELIGIBLE
        .iter()
        .any(|prefix| m.starts_with(prefix))
}

/// Strip the `-1m` suffix from a virtual model name, returning the
/// real API model ID and whether extended context was requested.
/// Returns an error message if the model isn't eligible for 1M.
fn resolve_model(model: &str) -> (&str, bool) {
    if let Some(base) = model.strip_suffix(EXTENDED_CONTEXT_SUFFIX) {
        if crate::model_context::has_native_1m_context(base) {
            // 1M is this model's default; nothing to opt into.
            (base, false)
        } else if is_extended_context_eligible(base) {
            (base, true)
        } else {
            tracing::warn!(
                "Model '{}' does not support 1M extended context. \
                 Using standard 200K context.",
                model
            );
            (base, false)
        }
    } else {
        (model, false)
    }
}

/// Build the `anthropic-beta` header value, appending the extended
/// context beta flag when requested.
fn beta_header(extended_context: bool) -> String {
    if extended_context {
        format!("{ANTHROPIC_BETA_FEATURES},{EXTENDED_CONTEXT_BETA}")
    } else {
        ANTHROPIC_BETA_FEATURES.to_string()
    }
}

/// Anthropic Claude API provider.
pub struct AnthropicProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl std::fmt::Debug for AnthropicProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicProvider")
            .field("base_url", &self.base_url)
            .field("api_key", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl AnthropicProvider {
    /// Build the system prompt as a cacheable content block array.
    /// The cache_control on the last block tells Anthropic to cache
    /// everything up to and including that block.
    fn build_cached_system(system_text: &str) -> serde_json::Value {
        serde_json::json!([
            {
                "type": "text",
                "text": system_text,
                "cache_control": { "type": "ephemeral" }
            }
        ])
    }

    /// Build tool definitions with cache_control on the last tool.
    /// This caches the entire tool schema prefix.
    fn build_cached_tools(tools: &[ToolDefinition]) -> Vec<serde_json::Value> {
        let len = tools.len();
        tools
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let mut tool = serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters,
                });
                // Mark the last tool as the cache breakpoint
                if i == len - 1 {
                    tool["cache_control"] = serde_json::json!({ "type": "ephemeral" });
                }
                tool
            })
            .collect()
    }

    /// Log cache hit/miss info at debug level.
    fn log_cache_stats(usage: &AnthropicUsage) {
        if usage.cache_read_input_tokens > 0 || usage.cache_creation_input_tokens > 0 {
            tracing::debug!(
                "Prompt cache: read={}tok, created={}tok, uncached={}tok",
                usage.cache_read_input_tokens,
                usage.cache_creation_input_tokens,
                usage.input_tokens,
            );
        }
    }
    /// Create a new Anthropic provider with the given API key and optional base URL.
    pub fn new(api_key: String, base_url: Option<&str>) -> Self {
        Self {
            client: super::build_http_client(base_url),
            base_url: base_url
                .unwrap_or("https://api.anthropic.com")
                .trim_end_matches('/')
                .to_string(),
            api_key,
        }
    }
}

// ── Request types ────────────────────────────────────────────

#[derive(Serialize)]
struct MessagesRequest {
    model: String,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<serde_json::Value>,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<serde_json::Value>,
}

#[derive(Serialize, Clone)]
struct AnthropicMessage {
    role: String,
    content: AnthropicContent,
}

#[derive(Serialize, Clone)]
#[serde(untagged)]
enum AnthropicContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "type")]
enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
    },
    #[serde(rename = "image")]
    Image { source: ImageSource },
    /// A block Koda doesn't read, such as `thinking` or `redacted_thinking`.
    /// One of these must not fail the whole response.
    #[serde(other, skip_serializing)]
    Unsupported,
}

#[derive(Serialize, Deserialize, Clone)]
struct ImageSource {
    #[serde(rename = "type")]
    source_type: String,
    media_type: String,
    data: String,
}

// ── Response types ───────────────────────────────────────────

#[derive(Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
    usage: AnthropicUsage,
}

#[derive(Deserialize, Debug, Clone)]
struct AnthropicUsage {
    input_tokens: i64,
    output_tokens: i64,
    /// Tokens written to cache on this request (first time penalty, then free).
    #[serde(default)]
    cache_creation_input_tokens: i64,
    /// Tokens read from cache (90% cheaper than regular input).
    #[serde(default)]
    cache_read_input_tokens: i64,
}

// ── Models API types ────────────────────────────────────────
//
// Field reference: <https://platform.claude.com/docs/en/api/models-list>
// and <https://platform.claude.com/docs/en/api/models/retrieve>. Only the
// fields Koda reads are declared; every one of them is optional on the
// wire or defaulted here, so an older response shape still parses.

/// One page of `GET /v1/models`.
#[derive(Deserialize)]
struct ModelsListResponse {
    data: Vec<ModelsListEntry>,
    #[serde(default)]
    has_more: bool,
}

/// One model in a `GET /v1/models` page.
#[derive(Deserialize, Debug, Clone)]
struct ModelsListEntry {
    id: String,
    /// RFC 3339 release time; the API may use the epoch when unknown.
    #[serde(default)]
    created_at: Option<String>,
    /// `haiku` / `sonnet` / `opus` / `fable` / `mythos`, or `null` for a
    /// model in no line. The docs say not to infer a line from the id.
    #[serde(default)]
    line: Option<String>,
    /// `active` / `deprecated` / `retired`.
    #[serde(default)]
    lifecycle: Option<String>,
}

/// Single model detail from `GET /v1/models/{model_id}`.
///
/// `max_input_tokens` is the context window and `max_tokens` the cap on
/// the `max_tokens` request parameter. Both are `number or null`, and the
/// docs' placeholder example shows `0`, so `model_capabilities`
/// treats a zero as unknown rather than applying it as a window.
#[derive(Deserialize)]
struct ModelDetailResponse {
    #[allow(dead_code)]
    id: String,
    #[serde(default)]
    max_input_tokens: Option<usize>,
    #[serde(default)]
    max_tokens: Option<usize>,
}

impl AnthropicProvider {
    /// One page of `GET /v1/models`. The catalogue is well under the 100
    /// entries requested, so a further page is only logged.
    async fn fetch_models(&self) -> Result<ModelsListResponse> {
        let resp = self
            .client
            .get(format!("{}/v1/models?limit=100", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_API_VERSION)
            .send()
            .await
            .context("Failed to connect to Anthropic API")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            if status.as_u16() == 401 {
                anyhow::bail!("Invalid API key (401 Unauthorized)");
            }
            anyhow::bail!("Anthropic API returned {status}: {body}");
        }

        let page: ModelsListResponse = resp
            .json()
            .await
            .context("Failed to parse Anthropic models response")?;
        if page.has_more {
            tracing::debug!(
                "Anthropic models list has more than one page; later pages are not read"
            );
        }
        Ok(page)
    }

    /// The newest model in each Claude line (`haiku` / `sonnet` / `opus`),
    /// by the Models API's `line` and `created_at`.
    ///
    /// This is what the `claude-haiku` / `claude-sonnet` / `claude-opus`
    /// aliases follow; `crate::model_alias` asks once per process and
    /// keeps its pinned ids when this fails.
    pub async fn latest_claude_ids(&self) -> Result<ClaudeLineIds> {
        let page = self.fetch_models().await?;
        Ok(latest_by_line(&page.data))
    }
}

/// Pick the newest `created_at` in each Claude line.
///
/// Entries with `line: null`, a line Koda has no alias for, or
/// `lifecycle: retired` (inference on them fails) are skipped. Timestamps
/// are compared as strings: the API emits RFC 3339 in one UTC format, so
/// that is chronological, and a missing one counts as the oldest. On a tie
/// the first listed entry stays, matching the API's newest-first order.
fn latest_by_line(entries: &[ModelsListEntry]) -> ClaudeLineIds {
    let mut newest: HashMap<ClaudeLine, &ModelsListEntry> = HashMap::new();
    for entry in entries {
        if entry.lifecycle.as_deref() == Some("retired") {
            continue;
        }
        let Some(line) = entry.line.as_deref().and_then(ClaudeLine::parse) else {
            continue;
        };
        let is_newer = newest
            .get(&line)
            .is_none_or(|best| entry.created_at.as_deref() > best.created_at.as_deref());
        if is_newer {
            newest.insert(line, entry);
        }
    }
    newest
        .into_iter()
        .map(|(line, entry)| (line, entry.id.clone()))
        .collect()
}

// ── SSE Streaming types ──────────────────────────────────────

#[derive(Deserialize)]
struct StreamEvent {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    delta: Option<StreamDelta>,
    #[serde(default)]
    content_block: Option<ContentBlock>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
    #[serde(default)]
    message: Option<StreamMessageInfo>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct StreamDelta {
    #[serde(rename = "type")]
    #[serde(default)]
    delta_type: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    partial_json: Option<String>,
    /// Present on message_delta events: "end_turn", "max_tokens", "stop_sequence"
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Deserialize)]
struct StreamMessageInfo {
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

// ── ChunkParser implementation ──────────────────────────────────

use super::stream_collector::ChunkParser;

/// Anthropic SSE chunk parser.
///
/// Processes `content_block_start/delta`, `message_start/delta/stop` events
/// and accumulates tool calls + usage across the stream.
pub(crate) struct AnthropicChunkParser {
    tool_calls: Vec<(String, String, String)>, // (id, name, args_json)
    /// Indices of tool calls already emitted via `ToolCallReady`.
    emitted_tool_indices: std::collections::HashSet<usize>,
    usage: TokenUsage,
    thinking_indices: std::collections::HashSet<usize>,
}

impl AnthropicChunkParser {
    pub fn new() -> Self {
        Self {
            tool_calls: Vec::new(),
            emitted_tool_indices: std::collections::HashSet::new(),
            usage: TokenUsage::default(),
            thinking_indices: std::collections::HashSet::new(),
        }
    }
}

impl ChunkParser for AnthropicChunkParser {
    fn process_line(&mut self, data: &str) -> Vec<StreamChunk> {
        // First, try to detect thinking blocks from raw JSON.
        // `ContentBlock` drops a thinking block's type, so we parse raw JSON
        // separately to register thinking block indices.
        if let Ok(raw) = serde_json::from_str::<serde_json::Value>(data)
            && raw.get("type").and_then(|t| t.as_str()) == Some("content_block_start")
            && let Some(idx) = raw.get("index").and_then(|i| i.as_u64())
            && let Some(cb) = raw.get("content_block")
            && cb.get("type").and_then(|t| t.as_str()) == Some("thinking")
        {
            self.thinking_indices.insert(idx as usize);
        }

        let Ok(event) = serde_json::from_str::<StreamEvent>(data) else {
            return vec![];
        };

        let mut chunks = Vec::new();

        match event.event_type.as_str() {
            "content_block_start" => {
                // Tool use block starting
                if let Some(ContentBlock::ToolUse { id, name, .. }) = event.content_block {
                    let idx = event.index.unwrap_or(self.tool_calls.len());
                    while self.tool_calls.len() <= idx {
                        self.tool_calls
                            .push((String::new(), String::new(), String::new()));
                    }
                    self.tool_calls[idx].0 = id;
                    self.tool_calls[idx].1 = name;
                }
            }
            "content_block_delta" => {
                if let Some(delta) = event.delta {
                    let idx = event.index.unwrap_or(0);
                    let is_thinking = self.thinking_indices.contains(&idx);

                    if is_thinking {
                        if let Some(text) = delta.thinking.or(delta.text)
                            && !text.is_empty()
                        {
                            chunks.push(StreamChunk::ThinkingDelta(text));
                        }
                    } else if let Some(text) = delta.text
                        && !text.is_empty()
                    {
                        chunks.push(StreamChunk::TextDelta(text));
                    }
                    // Tool use input JSON delta
                    if let Some(partial) = delta.partial_json
                        && idx < self.tool_calls.len()
                    {
                        self.tool_calls[idx].2.push_str(&partial);
                    }
                }
            }
            "message_delta" => {
                if let Some(u) = event.usage {
                    self.usage.completion_tokens = u.output_tokens;
                }
                if let Some(delta) = &event.delta
                    && let Some(reason) = &delta.stop_reason
                {
                    self.usage.stop_reason = reason.clone();
                }
            }
            "message_start" => {
                if let Some(msg) = event.message
                    && let Some(u) = msg.usage
                {
                    self.usage.prompt_tokens = u.input_tokens;
                    self.usage.cache_read_tokens = u.cache_read_input_tokens;
                    self.usage.cache_creation_tokens = u.cache_creation_input_tokens;
                }
            }
            _ => {
                // content_block_stop — emit ToolCallReady for completed tool calls
                if event.event_type == "content_block_stop"
                    && let Some(idx) = event.index
                    && idx < self.tool_calls.len()
                    && !self.tool_calls[idx].0.is_empty()
                {
                    let (id, name, args) = &self.tool_calls[idx];
                    chunks.push(StreamChunk::ToolCallReady(ToolCall {
                        id: id.clone(),
                        function_name: name.clone(),
                        arguments: args.clone(),
                        thought_signature: None,
                    }));
                    self.emitted_tool_indices.insert(idx);
                }
                // message_stop, ping, etc. — ignored
            }
        }

        chunks
    }

    fn finish(&mut self) -> Vec<StreamChunk> {
        let mut chunks = Vec::new();
        if !self.tool_calls.is_empty() {
            // Only emit tool calls NOT already sent via ToolCallReady
            let tcs: Vec<_> = self
                .tool_calls
                .iter()
                .enumerate()
                .filter(|(i, (id, _, _))| !id.is_empty() && !self.emitted_tool_indices.contains(i))
                .map(|(_, (id, name, args))| ToolCall {
                    id: id.clone(),
                    function_name: name.clone(),
                    arguments: args.clone(),
                    thought_signature: None,
                })
                .collect();
            if !tcs.is_empty() {
                chunks.push(StreamChunk::ToolCalls(tcs));
            }
        }
        chunks.push(StreamChunk::Done(std::mem::take(&mut self.usage)));
        chunks
    }
}

// ── Implementation ───────────────────────────────────────────────

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        settings: &crate::config::ModelSettings,
    ) -> Result<LlmResponse> {
        let (api_model, extended_ctx) = resolve_model(&settings.model);
        // Extract system prompt (Anthropic puts it at the top level)
        let system = messages
            .iter()
            .find(|m| m.role == "system")
            .and_then(|m| m.content.as_ref())
            .map(|text| Self::build_cached_system(text));

        // Convert messages (skip system, convert tool results)
        let api_messages = self.convert_messages(messages);
        let api_tools = Self::build_cached_tools(tools);

        let request = MessagesRequest {
            model: api_model.to_string(),
            max_tokens: settings.max_tokens.unwrap_or(16384),
            system,
            messages: api_messages,
            tools: api_tools,
        };

        let req = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_API_VERSION)
            .header("anthropic-beta", beta_header(extended_ctx))
            .json(&request);

        let msg_resp: MessagesResponse = super::http::send_and_parse_json(req, "Anthropic").await?;

        // Parse response content blocks into our unified format
        let mut content_text = String::new();
        let mut tool_calls = Vec::new();

        for block in msg_resp.content {
            match block {
                ContentBlock::Text { text } => content_text.push_str(&text),
                ContentBlock::ToolUse { id, name, input } => {
                    tool_calls.push(ToolCall {
                        id,
                        function_name: name,
                        arguments: serde_json::to_string(&input)?,
                        thought_signature: None,
                    });
                }
                _ => {}
            }
        }

        let content = if content_text.is_empty() {
            None
        } else {
            Some(content_text)
        };

        Self::log_cache_stats(&msg_resp.usage);

        Ok(LlmResponse {
            content,
            tool_calls,
            reasoning: None,
            usage: TokenUsage {
                prompt_tokens: msg_resp.usage.input_tokens,
                completion_tokens: msg_resp.usage.output_tokens,
                cache_read_tokens: msg_resp.usage.cache_read_input_tokens,
                cache_creation_tokens: msg_resp.usage.cache_creation_input_tokens,
                ..Default::default()
            },
        })
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        // Use the /v1/models endpoint to discover available models and verify the API key.
        let page = self.fetch_models().await?;

        let mut models: Vec<ModelInfo> = page
            .data
            .into_iter()
            .map(|m| ModelInfo {
                id: m.id,
                owned_by: Some("anthropic".to_string()),
            })
            .collect();

        // Append virtual "-1m" variants for eligible models
        let extended: Vec<ModelInfo> = models
            .iter()
            .filter(|m| is_extended_context_eligible(&m.id))
            .map(|m| ModelInfo {
                id: format!("{}-1m", m.id),
                owned_by: m.owned_by.clone(),
            })
            .collect();
        models.extend(extended);

        Ok(models)
    }

    fn provider_name(&self) -> &str {
        "anthropic"
    }

    /// Query the Models API for the context window and output cap.
    ///
    /// `GET /v1/models/{id}` reports them as `max_input_tokens` and
    /// `max_tokens`. A field that is missing, `null` or `0` stays `None`,
    /// so the caller keeps the `model_context.rs` table for it. The model
    /// is looked up as sent: a virtual `-1m` name gets a 404 and therefore
    /// the table's answer, which is what gives Sonnet 4.5 / 4 their 1M.
    async fn model_capabilities(&self, model: &str) -> Result<super::ModelCapabilities> {
        let resp = self
            .client
            .get(format!("{}/v1/models/{}", self.base_url, model))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_API_VERSION)
            .send()
            .await;

        let resp = match resp {
            Ok(r) if r.status().is_success() => r,
            _ => {
                tracing::debug!(
                    "Anthropic models API did not return info for {model}; \
                     using lookup table"
                );
                return Ok(super::ModelCapabilities::default());
            }
        };

        let detail: ModelDetailResponse = match resp.json().await {
            Ok(d) => d,
            Err(_) => return Ok(super::ModelCapabilities::default()),
        };

        Ok(super::ModelCapabilities {
            context_window: detail.max_input_tokens.filter(|&n| n > 0),
            max_output_tokens: detail.max_tokens.filter(|&n| n > 0),
        })
    }

    /// Real SSE streaming via Anthropic's Messages API.
    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        settings: &crate::config::ModelSettings,
    ) -> Result<super::stream_collector::SseCollector> {
        let (api_model, extended_ctx) = resolve_model(&settings.model);
        let system = messages
            .iter()
            .find(|m| m.role == "system")
            .and_then(|m| m.content.as_ref())
            .map(|text| Self::build_cached_system(text));

        let api_messages = self.convert_messages(messages);
        let api_tools = Self::build_cached_tools(tools);

        let mut max_tokens = settings.max_tokens.unwrap_or(16384);

        // Build request body with stream: true
        let mut body = serde_json::json!({
            "model": api_model,
            "max_tokens": max_tokens,
            "stream": true,
            "messages": serde_json::to_value(&api_messages)?,
        });

        if let Some(temp) = settings.temperature {
            body["temperature"] = serde_json::json!(temp);
        }

        // Extended thinking support
        if let Some(budget) = settings.thinking_budget {
            // max_tokens must be >= budget
            if max_tokens < budget {
                max_tokens = budget + 4096;
                body["max_tokens"] = serde_json::json!(max_tokens);
            }
            body["thinking"] = serde_json::json!({
                "type": "enabled",
                "budget_tokens": budget
            });
            // Temperature must not be set when thinking is enabled
            body.as_object_mut().unwrap().remove("temperature");
        }
        if let Some(sys) = system {
            body["system"] = sys;
        }
        if !api_tools.is_empty() {
            body["tools"] = serde_json::to_value(&api_tools)?;
        }

        let req = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_API_VERSION)
            .header("anthropic-beta", beta_header(extended_ctx))
            .json(&body);

        let resp = super::http::send_or_bail(req, "Anthropic").await?;

        let collector = super::stream_collector::spawn_sse_collector(
            resp,
            Box::new(AnthropicChunkParser::new()),
        );

        Ok(collector)
    }
}

impl AnthropicProvider {
    /// Convert our unified ChatMessage format to Anthropic's format.
    fn convert_messages(&self, messages: &[ChatMessage]) -> Vec<AnthropicMessage> {
        let mut result = Vec::new();

        for msg in messages {
            // Skip system messages (handled separately)
            if msg.role == "system" {
                continue;
            }

            if msg.role == "tool" {
                // Tool results need to be wrapped in a content block
                let tool_use_id = msg.tool_call_id.clone().unwrap_or_default();
                let content = msg.content.clone().unwrap_or_default();
                result.push(AnthropicMessage {
                    role: "user".to_string(),
                    content: AnthropicContent::Blocks(vec![ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                    }]),
                });
                continue;
            }

            if msg.role == "assistant"
                && let Some(tcs) = &msg.tool_calls
            {
                // Assistant message with tool calls
                let mut blocks: Vec<ContentBlock> = Vec::new();
                if let Some(text) = &msg.content
                    && !text.is_empty()
                {
                    blocks.push(ContentBlock::Text { text: text.clone() });
                }
                for tc in tcs {
                    // Anthropic requires tool_use.input to be a JSON object (dict).
                    // If arguments is empty/malformed or parses to a non-object
                    // (e.g. null, string), fall back to `{}` to avoid 400 errors (#501).
                    let input: serde_json::Value = serde_json::from_str(&tc.arguments)
                        .ok()
                        .filter(|v: &serde_json::Value| v.is_object())
                        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
                    blocks.push(ContentBlock::ToolUse {
                        id: tc.id.clone(),
                        name: tc.function_name.clone(),
                        input,
                    });
                }
                result.push(AnthropicMessage {
                    role: "assistant".to_string(),
                    content: AnthropicContent::Blocks(blocks),
                });
                continue;
            }

            // Regular user or assistant text message
            // If images are attached (user messages with @image refs), use blocks
            if let Some(images) = &msg.images
                && !images.is_empty()
            {
                let mut blocks = Vec::new();
                // Images first (Anthropic recommends images before text)
                for img in images {
                    blocks.push(ContentBlock::Image {
                        source: ImageSource {
                            source_type: "base64".to_string(),
                            media_type: img.media_type.clone(),
                            data: img.base64.clone(),
                        },
                    });
                }
                // Then text
                if let Some(text) = &msg.content
                    && !text.is_empty()
                {
                    blocks.push(ContentBlock::Text { text: text.clone() });
                }
                result.push(AnthropicMessage {
                    role: msg.role.clone(),
                    content: AnthropicContent::Blocks(blocks),
                });
                continue;
            }

            result.push(AnthropicMessage {
                role: msg.role.clone(),
                content: AnthropicContent::Text(msg.content.clone().unwrap_or_default()),
            });
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_provider() -> AnthropicProvider {
        AnthropicProvider::new("fake-key".into(), None)
    }

    #[test]
    fn test_convert_skips_system_messages() {
        let p = make_provider();
        let messages = vec![
            ChatMessage::text("system", "system prompt"),
            ChatMessage::text("user", "hello"),
        ];
        let converted = p.convert_messages(&messages);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].role, "user");
    }

    #[test]
    fn test_convert_tool_result_becomes_user_message() {
        let p = make_provider();
        let messages = vec![ChatMessage {
            role: "tool".into(),
            content: Some("file contents here".into()),
            tool_calls: None,
            tool_call_id: Some("tc_123".into()),
            images: None,
            reasoning: None,
        }];
        let converted = p.convert_messages(&messages);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].role, "user");
        // Should be a Blocks content with ToolResult
        match &converted[0].content {
            AnthropicContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 1);
                match &blocks[0] {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                    } => {
                        assert_eq!(tool_use_id, "tc_123");
                        assert_eq!(content, "file contents here");
                    }
                    _ => panic!("Expected ToolResult block"),
                }
            }
            _ => panic!("Expected Blocks content"),
        }
    }

    #[test]
    fn test_convert_assistant_with_tool_calls() {
        let p = make_provider();
        let messages = vec![ChatMessage {
            role: "assistant".into(),
            content: Some("Let me check.".into()),
            tool_calls: Some(vec![ToolCall {
                id: "tc_1".into(),
                function_name: "Read".into(),
                arguments: r#"{"path":"main.rs"}"#.into(),
                thought_signature: None,
            }]),
            tool_call_id: None,
            images: None,
            reasoning: None,
        }];
        let converted = p.convert_messages(&messages);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].role, "assistant");
        match &converted[0].content {
            AnthropicContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 2); // text + tool_use
            }
            _ => panic!("Expected Blocks content for assistant with tool calls"),
        }
    }

    #[test]
    fn test_convert_plain_user_message() {
        let p = make_provider();
        let messages = vec![ChatMessage::text("user", "explain this code")];
        let converted = p.convert_messages(&messages);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].role, "user");
        match &converted[0].content {
            AnthropicContent::Text(t) => assert_eq!(t, "explain this code"),
            _ => panic!("Expected Text content"),
        }
    }

    #[test]
    fn test_convert_empty_content_becomes_empty_string() {
        let p = make_provider();
        let messages = vec![ChatMessage {
            role: "user".into(),
            content: None,
            tool_calls: None,
            tool_call_id: None,
            images: None,
            reasoning: None,
        }];
        let converted = p.convert_messages(&messages);
        assert_eq!(converted.len(), 1);
        match &converted[0].content {
            AnthropicContent::Text(t) => assert_eq!(t, ""),
            _ => panic!("Expected Text content"),
        }
    }

    #[test]
    fn test_convert_assistant_tool_calls_without_text() {
        let p = make_provider();
        let messages = vec![ChatMessage {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: "tc_2".into(),
                function_name: "Bash".into(),
                arguments: r#"{"command":"cargo test"}"#.into(),
                thought_signature: None,
            }]),
            tool_call_id: None,
            images: None,
            reasoning: None,
        }];
        let converted = p.convert_messages(&messages);
        assert_eq!(converted.len(), 1);
        match &converted[0].content {
            AnthropicContent::Blocks(blocks) => {
                // Should have only the tool_use block, no empty text block
                assert_eq!(blocks.len(), 1);
                match &blocks[0] {
                    ContentBlock::ToolUse { name, .. } => assert_eq!(name, "Bash"),
                    _ => panic!("Expected ToolUse block"),
                }
            }
            _ => panic!("Expected Blocks content"),
        }
    }

    /// Regression: tool_use.input must always be a dict, never null (#501).
    #[test]
    fn test_convert_tool_use_input_always_dict() {
        let p = make_provider();

        // Case 1: empty string arguments
        // Case 2: null JSON
        // Case 3: JSON string (not object)
        // Case 4: valid object (should pass through)
        let cases = [
            ("".to_string(), serde_json::json!({})),
            ("null".to_string(), serde_json::json!({})),
            ("\"just a string\"".to_string(), serde_json::json!({})),
            ("42".to_string(), serde_json::json!({})),
            (
                r#"{"file":"main.rs"}"#.to_string(),
                serde_json::json!({"file": "main.rs"}),
            ),
        ];

        for (i, (args, expected)) in cases.iter().enumerate() {
            let messages = vec![ChatMessage {
                role: "assistant".into(),
                content: None,
                tool_calls: Some(vec![ToolCall {
                    id: format!("tc_{i}"),
                    function_name: "TestTool".into(),
                    arguments: args.clone(),
                    thought_signature: None,
                }]),
                tool_call_id: None,
                images: None,
                reasoning: None,
            }];
            let converted = p.convert_messages(&messages);
            match &converted[0].content {
                AnthropicContent::Blocks(blocks) => match &blocks[0] {
                    ContentBlock::ToolUse { input, .. } => {
                        assert!(
                            input.is_object(),
                            "Case {i} (args={args:?}): input must be an object, got {input}"
                        );
                        assert_eq!(input, expected, "Case {i} (args={args:?})");
                    }
                    _ => panic!("Case {i}: expected ToolUse block"),
                },
                _ => panic!("Case {i}: expected Blocks content"),
            }
        }
    }

    #[test]
    fn test_convert_full_conversation_ordering() {
        let p = make_provider();
        let messages = vec![
            ChatMessage::text("system", "sys"),
            ChatMessage::text("user", "hi"),
            ChatMessage::text("assistant", "hello!"),
            ChatMessage::text("user", "bye"),
        ];
        let converted = p.convert_messages(&messages);
        // System is skipped, so 3 messages
        assert_eq!(converted.len(), 3);
        assert_eq!(converted[0].role, "user");
        assert_eq!(converted[1].role, "assistant");
        assert_eq!(converted[2].role, "user");
    }

    #[test]
    fn test_convert_user_message_with_images() {
        let p = make_provider();
        let messages = vec![ChatMessage {
            role: "user".into(),
            content: Some("What is in this image?".into()),
            tool_calls: None,
            tool_call_id: None,
            images: Some(vec![super::super::ImageData {
                media_type: "image/png".into(),
                base64: "iVBORw0KGgo=".into(),
            }]),
            reasoning: None,
        }];
        let converted = p.convert_messages(&messages);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].role, "user");
        match &converted[0].content {
            AnthropicContent::Blocks(blocks) => {
                // Image block + text block
                assert_eq!(blocks.len(), 2);
                match &blocks[0] {
                    ContentBlock::Image { source } => {
                        assert_eq!(source.source_type, "base64");
                        assert_eq!(source.media_type, "image/png");
                    }
                    _ => panic!("Expected Image block first"),
                }
                match &blocks[1] {
                    ContentBlock::Text { text } => {
                        assert_eq!(text, "What is in this image?");
                    }
                    _ => panic!("Expected Text block second"),
                }
            }
            _ => panic!("Expected Blocks content for message with images"),
        }
    }

    #[test]
    fn test_build_cached_system() {
        let result = AnthropicProvider::build_cached_system("You are a helpful assistant.");
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[0]["text"], "You are a helpful assistant.");
        assert_eq!(arr[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn test_build_cached_tools_marks_last() {
        let tools = vec![
            ToolDefinition {
                name: "Read".into(),
                description: "Read a file".into(),
                parameters: serde_json::json!({"type": "object"}),
            },
            ToolDefinition {
                name: "Write".into(),
                description: "Write a file".into(),
                parameters: serde_json::json!({"type": "object"}),
            },
        ];
        let cached = AnthropicProvider::build_cached_tools(&tools);
        assert_eq!(cached.len(), 2);

        // First tool: no cache_control
        assert!(cached[0].get("cache_control").is_none());
        assert_eq!(cached[0]["name"], "Read");

        // Last tool: has cache_control
        assert_eq!(cached[1]["cache_control"]["type"], "ephemeral");
        assert_eq!(cached[1]["name"], "Write");
    }

    #[test]
    fn test_build_cached_tools_empty() {
        let cached = AnthropicProvider::build_cached_tools(&[]);
        assert!(cached.is_empty());
    }

    #[test]
    fn test_build_cached_tools_single() {
        let tools = vec![ToolDefinition {
            name: "Bash".into(),
            description: "Run a command".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let cached = AnthropicProvider::build_cached_tools(&tools);
        assert_eq!(cached.len(), 1);
        // Single tool should have cache_control (it's both first and last)
        assert_eq!(cached[0]["cache_control"]["type"], "ephemeral");
    }

    // ── Extended context (1M) tests ────────────────────────

    #[test]
    fn test_resolve_model_standard() {
        let (model, ext) = resolve_model("claude-sonnet-5-5");
        assert_eq!(model, "claude-sonnet-5-5");
        assert!(!ext);
    }

    #[test]
    fn test_resolve_model_1m_native_is_noop() {
        // 1M is the default on these; the suffix is stripped, no header.
        let (model, ext) = resolve_model("claude-sonnet-4-6-1m");
        assert_eq!(model, "claude-sonnet-4-6");
        assert!(!ext);

        let (model, ext) = resolve_model("claude-opus-4-6-1m");
        assert_eq!(model, "claude-opus-4-6");
        assert!(!ext);

        let (model, ext) = resolve_model("claude-sonnet-5-5-1m");
        assert_eq!(model, "claude-sonnet-5-5");
        assert!(!ext);
    }

    #[test]
    fn test_resolve_model_1m_eligible() {
        let (model, ext) = resolve_model("claude-sonnet-4-5-20250929-1m");
        assert_eq!(model, "claude-sonnet-4-5-20250929");
        assert!(ext);

        let (model, ext) = resolve_model("claude-sonnet-4-20250514-1m");
        assert_eq!(model, "claude-sonnet-4-20250514");
        assert!(ext);
    }

    #[test]
    fn test_resolve_model_1m_ineligible() {
        // Opus 4.5, Haiku — not eligible, should get ext=false
        let (model, ext) = resolve_model("claude-opus-4-5-20251101-1m");
        assert_eq!(model, "claude-opus-4-5-20251101");
        assert!(!ext);

        let (model, ext) = resolve_model("claude-haiku-4-5-20251001-1m");
        assert_eq!(model, "claude-haiku-4-5-20251001");
        assert!(!ext);
    }

    #[test]
    fn test_is_eligible() {
        assert!(is_extended_context_eligible("claude-sonnet-4-5-20250929"));
        assert!(is_extended_context_eligible("claude-sonnet-4-20250514"));

        // Natively 1M: no header, no -1m variant in /model
        assert!(!is_extended_context_eligible("claude-sonnet-4-6"));
        assert!(!is_extended_context_eligible("claude-opus-4-6"));
        assert!(!is_extended_context_eligible("claude-sonnet-5-5"));
        assert!(!is_extended_context_eligible("claude-haiku-5-5"));

        assert!(!is_extended_context_eligible("claude-opus-4-5-20251101"));
        assert!(!is_extended_context_eligible("claude-haiku-4-5-20251001"));
        assert!(!is_extended_context_eligible("claude-3-opus-20240229"));
    }

    #[test]
    fn test_beta_header_standard() {
        let header = beta_header(false);
        assert_eq!(header, ANTHROPIC_BETA_FEATURES);
        assert!(!header.contains("context-1m"));
    }

    #[test]
    fn test_beta_header_extended() {
        let header = beta_header(true);
        assert!(header.contains(ANTHROPIC_BETA_FEATURES));
        assert!(header.contains(EXTENDED_CONTEXT_BETA));
    }

    // ── SSE StreamEvent deserialization tests ──────────────────

    #[test]
    fn stream_event_deserializes_content_block_delta() {
        let json = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}"#;
        let event: StreamEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type, "content_block_delta");
        assert_eq!(event.index, Some(0));
        let delta = event.delta.unwrap();
        assert_eq!(delta.delta_type.as_deref(), Some("text_delta"));
        assert_eq!(delta.text.as_deref(), Some("hello"));
    }

    #[test]
    fn stream_event_deserializes_message_start_with_usage() {
        let json =
            r#"{"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":0}}}"#;
        let event: StreamEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type, "message_start");
        let message = event.message.unwrap();
        let usage = message.usage.unwrap();
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 0);
    }

    #[test]
    fn stream_event_deserializes_message_delta_with_stop_reason() {
        let json = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":0,"output_tokens":25}}"#;
        let event: StreamEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type, "message_delta");
        let delta = event.delta.unwrap();
        assert_eq!(delta.stop_reason.as_deref(), Some("end_turn"));
        let usage = event.usage.unwrap();
        assert_eq!(usage.output_tokens, 25);
    }

    #[test]
    fn stream_event_unknown_type_deserializes_gracefully() {
        let json = r#"{"type":"ping"}"#;
        let result = serde_json::from_str::<StreamEvent>(json);
        // Should not error — unknown event types are valid in the SSE protocol
        assert!(result.is_ok());
        let event = result.unwrap();
        assert_eq!(event.event_type, "ping");
        assert!(event.delta.is_none());
        assert!(event.message.is_none());
    }

    #[test]
    fn stream_event_missing_delta_field() {
        // A content_block_delta event with no "delta" key — should parse
        // but delta will be None (all fields have #[serde(default)])
        let json = r#"{"type":"content_block_delta","index":0}"#;
        let result = serde_json::from_str::<StreamEvent>(json);
        assert!(result.is_ok());
        let event = result.unwrap();
        assert_eq!(event.event_type, "content_block_delta");
        assert!(event.delta.is_none());
    }

    // ── Models API: Claude line selection ───────────────────

    fn entry(id: &str, line: Option<&str>, created_at: Option<&str>) -> ModelsListEntry {
        ModelsListEntry {
            id: id.to_string(),
            created_at: created_at.map(str::to_string),
            line: line.map(str::to_string),
            lifecycle: Some("active".to_string()),
        }
    }

    #[test]
    fn latest_by_line_picks_newest_created_at_per_line() {
        let entries = vec![
            entry(
                "claude-haiku-4-5-20251001",
                Some("haiku"),
                Some("2025-10-15T00:00:00Z"),
            ),
            entry(
                "claude-sonnet-5-5",
                Some("sonnet"),
                Some("2026-09-21T00:00:00Z"),
            ),
            entry(
                "claude-haiku-5-5",
                Some("haiku"),
                Some("2026-09-21T00:00:00Z"),
            ),
            entry(
                "claude-sonnet-5",
                Some("sonnet"),
                Some("2026-05-12T00:00:00Z"),
            ),
            entry(
                "claude-opus-4-6",
                Some("opus"),
                Some("2026-02-05T00:00:00Z"),
            ),
            entry(
                "claude-opus-5-5",
                Some("opus"),
                Some("2026-09-21T00:00:00Z"),
            ),
        ];
        let ids = latest_by_line(&entries);
        assert_eq!(ids[&ClaudeLine::Haiku], "claude-haiku-5-5");
        assert_eq!(ids[&ClaudeLine::Sonnet], "claude-sonnet-5-5");
        assert_eq!(ids[&ClaudeLine::Opus], "claude-opus-5-5");
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn latest_by_line_ignores_null_unknown_and_retired() {
        let mut retired = entry(
            "claude-sonnet-9",
            Some("sonnet"),
            Some("2027-01-01T00:00:00Z"),
        );
        retired.lifecycle = Some("retired".to_string());
        let entries = vec![
            // A line Koda has no alias for.
            entry(
                "claude-fable-5-1",
                Some("fable"),
                Some("2026-10-01T00:00:00Z"),
            ),
            // `line: null`, even though it is the newest of all.
            entry(
                "claude-3-7-sonnet-20250219",
                None,
                Some("2026-12-01T00:00:00Z"),
            ),
            entry(
                "claude-sonnet-5-5",
                Some("sonnet"),
                Some("2026-09-21T00:00:00Z"),
            ),
            retired,
        ];
        let ids = latest_by_line(&entries);
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[&ClaudeLine::Sonnet], "claude-sonnet-5-5");
    }

    #[test]
    fn latest_by_line_tie_keeps_the_first_listed() {
        // The API lists newest first; on equal timestamps the first wins.
        let entries = vec![
            entry(
                "claude-opus-5-5",
                Some("opus"),
                Some("2026-09-21T00:00:00Z"),
            ),
            entry(
                "claude-opus-5-5-20260921",
                Some("opus"),
                Some("2026-09-21T00:00:00Z"),
            ),
        ];
        assert_eq!(
            latest_by_line(&entries)[&ClaudeLine::Opus],
            "claude-opus-5-5"
        );
    }

    #[test]
    fn latest_by_line_missing_created_at_counts_as_oldest() {
        let entries = vec![
            entry("claude-haiku-undated", Some("haiku"), None),
            entry(
                "claude-haiku-5-5",
                Some("haiku"),
                Some("2026-09-21T00:00:00Z"),
            ),
        ];
        assert_eq!(
            latest_by_line(&entries)[&ClaudeLine::Haiku],
            "claude-haiku-5-5"
        );
        // Alone, an undated entry still answers for its line.
        let only = vec![entry("claude-haiku-undated", Some("haiku"), None)];
        assert_eq!(
            latest_by_line(&only)[&ClaudeLine::Haiku],
            "claude-haiku-undated"
        );
        assert!(latest_by_line(&[]).is_empty());
    }

    #[test]
    fn models_list_entry_parses_the_documented_shape() {
        let json = r#"{"type":"model","id":"claude-haiku-5-5","display_name":"Claude Haiku 5.5","created_at":"2026-09-21T00:00:00Z","line":"haiku","lifecycle":"active","deprecated_at":null,"retires_at":null,"max_input_tokens":1000000,"max_tokens":128000,"capabilities":{"image_input":{"supported":true}}}"#;
        let e: ModelsListEntry = serde_json::from_str(json).unwrap();
        assert_eq!(e.id, "claude-haiku-5-5");
        assert_eq!(e.line.as_deref(), Some("haiku"));
        assert_eq!(e.created_at.as_deref(), Some("2026-09-21T00:00:00Z"));
        assert_eq!(e.lifecycle.as_deref(), Some("active"));

        // The pre-2026 shape without `line` / `lifecycle` still parses.
        let old = r#"{"type":"model","id":"claude-sonnet-4-6","display_name":"Claude Sonnet 4.6","created_at":"2026-02-19T00:00:00Z"}"#;
        let e: ModelsListEntry = serde_json::from_str(old).unwrap();
        assert!(e.line.is_none());
        assert!(e.lifecycle.is_none());
    }

    mod models_api {
        //! `GET /v1/models` and `GET /v1/models/{id}` against a loopback
        //! axum server, the `http.rs` test pattern: no live key, no network.
        //! Multi-thread flavor because the server is `tokio::spawn`ed.

        use super::super::*;
        use crate::model_alias::{ClaudeLine, LiveClaudeIds, pinned, resolve_with};
        use axum::Router;
        use axum::extract::{Path, State};
        use axum::http::{HeaderMap, StatusCode, header::CONTENT_TYPE};
        use axum::response::IntoResponse;
        use axum::routing::get;
        use std::sync::{Arc, Mutex};
        use tokio_util::sync::CancellationToken;

        #[derive(Clone)]
        struct Fixture {
            list: (u16, &'static str),
            detail: (u16, &'static str),
            /// Headers of every request, in order.
            seen: Arc<Mutex<Vec<HeaderMap>>>,
        }

        async fn list(State(f): State<Fixture>, headers: HeaderMap) -> impl IntoResponse {
            f.seen.lock().unwrap().push(headers);
            (
                StatusCode::from_u16(f.list.0).unwrap(),
                [(CONTENT_TYPE, "application/json")],
                f.list.1,
            )
        }

        async fn detail(
            State(f): State<Fixture>,
            Path(_id): Path<String>,
            headers: HeaderMap,
        ) -> impl IntoResponse {
            f.seen.lock().unwrap().push(headers);
            (
                StatusCode::from_u16(f.detail.0).unwrap(),
                [(CONTENT_TYPE, "application/json")],
                f.detail.1,
            )
        }

        async fn spawn(
            list_resp: (u16, &'static str),
            detail_resp: (u16, &'static str),
        ) -> (AnthropicProvider, Fixture, CancellationToken) {
            let fixture = Fixture {
                list: list_resp,
                detail: detail_resp,
                seen: Arc::new(Mutex::new(Vec::new())),
            };
            let app = Router::new()
                .route("/v1/models", get(list))
                .route("/v1/models/{id}", get(detail))
                .with_state(fixture.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let ct = CancellationToken::new();
            let ct_server = ct.clone();
            tokio::spawn(async move {
                axum::serve(listener, app)
                    .with_graceful_shutdown(async move { ct_server.cancelled_owned().await })
                    .await
                    .ok();
            });
            let provider = AnthropicProvider::new("sk-ant-test".to_string(), Some(&url));
            (provider, fixture, ct)
        }

        /// A page in the documented shape. `claude-sonnet-7` is invented so
        /// the tests can tell an API answer from the pinned table.
        const LIST: &str = r#"{"data":[
            {"type":"model","id":"claude-fable-5-1","display_name":"Claude Fable 5.1","created_at":"2026-10-01T00:00:00Z","line":"fable","lifecycle":"active","max_input_tokens":1000000,"max_tokens":128000},
            {"type":"model","id":"claude-opus-5-5","display_name":"Claude Opus 5.5","created_at":"2026-09-21T00:00:00Z","line":"opus","lifecycle":"active","max_input_tokens":1000000,"max_tokens":128000},
            {"type":"model","id":"claude-sonnet-7","display_name":"Claude Sonnet 7","created_at":"2026-09-21T00:00:00Z","line":"sonnet","lifecycle":"active","max_input_tokens":1000000,"max_tokens":128000},
            {"type":"model","id":"claude-sonnet-5-5","display_name":"Claude Sonnet 5.5","created_at":"2026-06-21T00:00:00Z","line":"sonnet","lifecycle":"active","max_input_tokens":1000000,"max_tokens":128000},
            {"type":"model","id":"claude-haiku-5-5","display_name":"Claude Haiku 5.5","created_at":"2026-09-21T00:00:00Z","line":"haiku","lifecycle":"active","max_input_tokens":1000000,"max_tokens":128000},
            {"type":"model","id":"claude-haiku-4-5-20251001","display_name":"Claude Haiku 4.5","created_at":"2025-10-15T00:00:00Z","line":"haiku","lifecycle":"active","max_input_tokens":200000,"max_tokens":64000},
            {"type":"model","id":"claude-3-7-sonnet-20250219","display_name":"Claude Sonnet 3.7","created_at":"2025-02-24T00:00:00Z","line":null,"lifecycle":"deprecated","max_input_tokens":200000,"max_tokens":64000}
        ],"has_more":false,"first_id":"claude-fable-5-1","last_id":"claude-3-7-sonnet-20250219"}"#;

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn latest_claude_ids_takes_the_newest_per_line() {
            let (provider, fixture, ct) = spawn((200, LIST), (404, "{}")).await;
            let ids = provider.latest_claude_ids().await.expect("ok");
            assert_eq!(ids[&ClaudeLine::Haiku], "claude-haiku-5-5");
            assert_eq!(ids[&ClaudeLine::Sonnet], "claude-sonnet-7");
            assert_eq!(ids[&ClaudeLine::Opus], "claude-opus-5-5");
            assert_eq!(ids.len(), 3);
            // One request, carrying the key and the version Koda sends everywhere.
            let seen = fixture.seen.lock().unwrap();
            assert_eq!(seen.len(), 1);
            let version = seen[0].get("anthropic-version").unwrap().to_str().unwrap();
            assert_eq!(version, ANTHROPIC_API_VERSION);
            let key = seen[0].get("x-api-key").unwrap().to_str().unwrap();
            assert_eq!(key, "sk-ant-test");
            drop(seen);
            ct.cancel();
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn latest_claude_ids_fails_on_http_error() {
            let (provider, _fixture, ct) = spawn((500, r#"{"error":"boom"}"#), (404, "{}")).await;
            let err = provider.latest_claude_ids().await.unwrap_err();
            assert!(format!("{err:#}").contains("500"), "{err:#}");
            ct.cancel();
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn latest_claude_ids_names_a_bad_key() {
            let (provider, _fixture, ct) = spawn(
                (401, r#"{"error":{"type":"authentication_error"}}"#),
                (404, "{}"),
            )
            .await;
            let err = provider.latest_claude_ids().await.unwrap_err();
            assert!(format!("{err:#}").contains("Invalid API key"), "{err:#}");
            ct.cancel();
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn live_ids_from_the_api_resolve_the_aliases() {
            let (provider, _fixture, ct) = spawn((200, LIST), (404, "{}")).await;
            let cache = LiveClaudeIds::new();
            let ids = cache
                .get_or_fetch(|| provider.latest_claude_ids())
                .await
                .expect("live ids");
            assert_eq!(
                resolve_with("claude-sonnet", Some(ids)).unwrap().model_id,
                "claude-sonnet-7"
            );
            assert_eq!(
                resolve_with("claude-haiku", Some(ids)).unwrap().model_id,
                "claude-haiku-5-5"
            );
            assert_eq!(
                resolve_with("claude-opus", Some(ids)).unwrap().model_id,
                "claude-opus-5-5"
            );
            ct.cancel();
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn api_failure_falls_back_to_the_pinned_ids() {
            let (provider, fixture, ct) = spawn((503, "overloaded"), (404, "{}")).await;
            let cache = LiveClaudeIds::new();
            assert!(
                cache
                    .get_or_fetch(|| provider.latest_claude_ids())
                    .await
                    .is_none()
            );
            // A second use does not ask the API again.
            assert!(
                cache
                    .get_or_fetch(|| provider.latest_claude_ids())
                    .await
                    .is_none()
            );
            assert_eq!(fixture.seen.lock().unwrap().len(), 1);
            for alias in pinned() {
                assert_eq!(
                    resolve_with(alias.alias, cache.get()).unwrap().model_id,
                    alias.model_id,
                    "{} falls back to its pinned id",
                    alias.alias
                );
            }
            ct.cancel();
        }

        // ── Models API: capabilities ────────────────────────────

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn model_capabilities_reads_max_input_tokens_and_max_tokens() {
            let detail = r#"{"type":"model","id":"claude-sonnet-5-5","display_name":"Claude Sonnet 5.5","created_at":"2026-06-21T00:00:00Z","line":"sonnet","max_input_tokens":1000000,"max_tokens":128000}"#;
            let (provider, fixture, ct) = spawn((200, "{}"), (200, detail)).await;
            let caps = provider
                .model_capabilities("claude-sonnet-5-5")
                .await
                .unwrap();
            assert_eq!(caps.context_window, Some(1_000_000));
            assert_eq!(caps.max_output_tokens, Some(128_000));
            let seen = fixture.seen.lock().unwrap();
            let version = seen[0].get("anthropic-version").unwrap().to_str().unwrap();
            assert_eq!(version, ANTHROPIC_API_VERSION);
            drop(seen);
            ct.cancel();
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn model_capabilities_treats_zero_null_and_old_names_as_unknown() {
            // `0` is the docs' placeholder, `null` is documented, and
            // `context_window` / `max_output_tokens` never existed.
            let detail = r#"{"type":"model","id":"claude-sonnet-5-5","max_input_tokens":0,"max_tokens":null,"context_window":200000,"max_output_tokens":64000}"#;
            let (provider, _fixture, ct) = spawn((200, "{}"), (200, detail)).await;
            let caps = provider
                .model_capabilities("claude-sonnet-5-5")
                .await
                .unwrap();
            assert_eq!(caps.context_window, None);
            assert_eq!(caps.max_output_tokens, None);
            ct.cancel();
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn model_capabilities_is_empty_when_the_model_is_unknown() {
            let (provider, _fixture, ct) = spawn(
                (200, "{}"),
                (404, r#"{"error":{"type":"not_found_error"}}"#),
            )
            .await;
            // A virtual `-1m` name is one the API never knows.
            let caps = provider
                .model_capabilities("claude-sonnet-4-5-20250929-1m")
                .await
                .unwrap();
            assert_eq!(caps.context_window, None);
            assert_eq!(caps.max_output_tokens, None);
            ct.cancel();
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn config_takes_the_window_and_output_cap_from_the_api() {
            // 777K differs from every table value, so the source is unambiguous.
            let detail = r#"{"type":"model","id":"claude-sonnet-5-5","max_input_tokens":777000,"max_tokens":128000}"#;
            let (provider, _fixture, ct) = spawn((200, "{}"), (200, detail)).await;
            let mut config = crate::config::KodaConfig::default_for_testing(
                crate::config::ProviderType::Anthropic,
            );
            config.model = "claude-sonnet-5-5".to_string();
            config.model_settings.model = config.model.clone();
            config.model_settings.max_tokens = None;
            config.recalculate_model_derived();
            assert_eq!(config.max_context_tokens, 1_000_000, "table value first");

            config.query_and_apply_capabilities(&provider).await;
            assert_eq!(config.max_context_tokens, 777_000);
            assert_eq!(config.model_settings.max_context_tokens, 777_000);
            assert_eq!(config.model_settings.max_tokens, Some(128_000));
            ct.cancel();
        }
    }
}
