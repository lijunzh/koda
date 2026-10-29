//! OpenAI-compatible LLM provider.
//!
//! Works with OpenAI, LM Studio, Groq, and any API that speaks
//! the OpenAI chat completions format.

use super::{
    ChatMessage, LlmProvider, LlmResponse, ModelInfo, StreamChunk, TokenUsage, ToolCall,
    ToolDefinition,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Client for OpenAI-compatible APIs.
pub struct OpenAiCompatProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    /// Set once the server has sent its reasoning under the plain `reasoning`
    /// key (see [`ReasoningField`]).
    reasoning_plain: Arc<AtomicBool>,
}

/// The JSON key an OpenAI-compatible server uses for a model's reasoning.
///
/// Servers disagree: llama-server, LM Studio and DeepSeek use
/// `reasoning_content`; vllm-mlx and Recursive's `rec` use `reasoning`. We read
/// both. When replaying history we send the reasoning back under exactly one
/// name: the one this server used to send it to us, `reasoning_content` until
/// we've seen otherwise.
///
/// One name, not both: a strict server may reject an unknown message field
/// (OpenAI itself does), and it's the server's own key that its chat template
/// reads. Codex has no equivalent choice to copy: its Responses API replays
/// `ResponseItem::Reasoning` items verbatim, and it no longer speaks chat
/// completions. The reasoning is only ever attached to an assistant message
/// that has some, so servers that never return it never see the field.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ReasoningField {
    #[default]
    ReasoningContent,
    Reasoning,
}

/// The non-empty reasoning in a delta or message, and the key it arrived under.
/// `reasoning_content` wins if a server sends both.
fn pick_reasoning<'a>(
    reasoning_content: &'a Option<String>,
    reasoning: &'a Option<String>,
) -> Option<(&'a str, ReasoningField)> {
    let non_empty = |o: &'a Option<String>| o.as_deref().filter(|t| !t.is_empty());
    non_empty(reasoning_content)
        .map(|t| (t, ReasoningField::ReasoningContent))
        .or_else(|| non_empty(reasoning).map(|t| (t, ReasoningField::Reasoning)))
}

impl std::fmt::Debug for OpenAiCompatProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCompatProvider")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_deref().map(|_| "[REDACTED]"))
            .finish_non_exhaustive()
    }
}

impl OpenAiCompatProvider {
    /// Create a new OpenAI-compatible provider with the given base URL and optional API key.
    pub fn new(base_url: &str, api_key: Option<String>) -> Self {
        Self {
            client: super::build_http_client(Some(base_url)),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            reasoning_plain: Arc::new(AtomicBool::new(false)),
        }
    }

    fn reasoning_field(&self) -> ReasoningField {
        if self.reasoning_plain.load(Ordering::Relaxed) {
            ReasoningField::Reasoning
        } else {
            ReasoningField::ReasoningContent
        }
    }
}

// ── Request types ────────────────────────────────────────────

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ApiMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ApiTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Serialize)]
struct ApiMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ApiToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    /// The previous turn's reasoning, under the server's own key. At most one
    /// of these two is set (see [`ReasoningField`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<String>,
}

#[derive(Serialize)]
struct ApiTool {
    r#type: String,
    function: ApiFunction,
}

#[derive(Serialize)]
struct ApiFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Serialize, Deserialize, Clone)]
struct ApiToolCall {
    id: String,
    r#type: String,
    function: ApiToolCallFunction,
}

#[derive(Serialize, Deserialize, Clone)]
struct ApiToolCallFunction {
    name: String,
    arguments: String,
}

// ── Response types ───────────────────────────────────────────

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
    usage: Option<UsageResponse>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    tool_calls: Option<Vec<ApiToolCall>>,
}

#[derive(Deserialize)]
struct UsageResponse {
    prompt_tokens: Option<i64>,
    completion_tokens: Option<i64>,
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokensDetails>,
}

#[derive(Deserialize, Default)]
struct CompletionTokensDetails {
    #[serde(default)]
    reasoning_tokens: Option<i64>,
}

// ── SSE Streaming response types ─────────────────────────────

#[derive(Deserialize)]
struct StreamChatResponse {
    choices: Vec<StreamChoice>,
    usage: Option<UsageResponse>,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct StreamDelta {
    content: Option<String>,
    /// Reasoning under the `reasoning_content` key (llama-server, LM Studio, DeepSeek).
    #[serde(default)]
    reasoning_content: Option<String>,
    /// Reasoning under the `reasoning` key (vllm-mlx, rec).
    #[serde(default)]
    reasoning: Option<String>,
    tool_calls: Option<Vec<StreamToolCall>>,
}

#[derive(Deserialize)]
struct StreamToolCall {
    index: Option<usize>,
    id: Option<String>,
    function: Option<StreamToolCallFunction>,
}

#[derive(Deserialize)]
struct StreamToolCallFunction {
    name: Option<String>,
    arguments: Option<String>,
}

// ── ChunkParser implementation ──────────────────────────────────

use super::stream_collector::ChunkParser;

/// OpenAI-compatible SSE chunk parser.
///
/// Handles the OpenAI streaming format with `choices[].delta` incremental fields,
/// reasoning content (`reasoning_content` or `reasoning`), and `<think>` tag filtering for models
/// that wrap reasoning in XML tags.
pub(crate) struct OpenAiChunkParser {
    tool_calls: Vec<(String, String, String)>, // (id, name, args)
    usage: TokenUsage,
    think_filter: super::stream_tag_filter::StreamTagFilter,
    /// Told when the server reasons under the plain `reasoning` key.
    reasoning_plain: Option<Arc<AtomicBool>>,
}

impl OpenAiChunkParser {
    pub fn new() -> Self {
        Self {
            tool_calls: Vec::new(),
            usage: TokenUsage::default(),
            think_filter: super::stream_tag_filter::StreamTagFilter::new(),
            reasoning_plain: None,
        }
    }

    /// Record the reasoning key the server uses in `flag`, for replaying history.
    fn noting_reasoning_field(mut self, flag: Arc<AtomicBool>) -> Self {
        self.reasoning_plain = Some(flag);
        self
    }
}

impl ChunkParser for OpenAiChunkParser {
    fn process_line(&mut self, data: &str) -> Vec<StreamChunk> {
        let chunk = match serde_json::from_str::<StreamChatResponse>(data) {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(
                    "OpenAI-compat: failed to parse SSE chunk: {e} — raw: {truncated}",
                    truncated = &data[..data.len().min(200)]
                );
                return vec![];
            }
        };

        let mut chunks = Vec::new();

        // Capture usage if present
        if let Some(u) = &chunk.usage {
            self.usage = usage_from_response(u);
        }

        for choice in &chunk.choices {
            // Capture finish_reason
            if let Some(reason) = &choice.finish_reason {
                self.usage.stop_reason = if reason == "length" {
                    "max_tokens".to_string()
                } else {
                    reason.clone()
                };
            }

            // Native reasoning, under either key
            if let Some((reasoning, field)) =
                pick_reasoning(&choice.delta.reasoning_content, &choice.delta.reasoning)
            {
                if let Some(flag) = &self.reasoning_plain {
                    flag.store(field == ReasoningField::Reasoning, Ordering::Relaxed);
                }
                chunks.push(StreamChunk::ThinkingDelta(reasoning.to_string()));
            }

            // Text delta — run through <think> tag filter
            if let Some(content) = &choice.delta.content
                && !content.is_empty()
            {
                chunks.extend(
                    self.think_filter
                        .process(StreamChunk::TextDelta(content.clone())),
                );
            }

            // Tool call deltas — accumulate
            if let Some(tcs) = &choice.delta.tool_calls {
                for tc in tcs {
                    let idx = tc.index.unwrap_or(0);
                    while self.tool_calls.len() <= idx {
                        self.tool_calls
                            .push((String::new(), String::new(), String::new()));
                    }
                    if let Some(id) = &tc.id {
                        self.tool_calls[idx].0 = id.clone();
                    }
                    if let Some(f) = &tc.function {
                        if let Some(name) = &f.name {
                            self.tool_calls[idx].1.push_str(name);
                        }
                        if let Some(args) = &f.arguments {
                            self.tool_calls[idx].2.push_str(args);
                        }
                    }
                }
            }
        }

        chunks
    }

    fn finish(&mut self) -> Vec<StreamChunk> {
        let mut chunks = Vec::new();
        if !self.tool_calls.is_empty() {
            let tcs = self
                .tool_calls
                .drain(..)
                .map(|(id, name, args)| ToolCall {
                    id,
                    function_name: name,
                    arguments: args,
                    thought_signature: None,
                })
                .collect();
            chunks.push(StreamChunk::ToolCalls(tcs));
        }
        chunks.extend(self.think_filter.flush());
        chunks.push(StreamChunk::Done(std::mem::take(&mut self.usage)));
        chunks
    }
}

// ── Implementation ───────────────────────────────────────────────

impl OpenAiCompatProvider {
    /// Build a ChatRequest from messages, tools, model, and optional stream flag.
    fn build_request(
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        model: &str,
        stream: Option<bool>,
        settings: &crate::config::ModelSettings,
        reasoning_field: ReasoningField,
    ) -> ChatRequest {
        let api_messages: Vec<ApiMessage> = messages
            .iter()
            .map(|m| {
                // Build content: if images are attached, use multi-part array format
                let content = if let Some(images) = &m.images {
                    if !images.is_empty() {
                        let mut parts = Vec::new();
                        // Text part
                        if let Some(text) = &m.content {
                            parts.push(serde_json::json!({
                                "type": "text",
                                "text": text
                            }));
                        }
                        // Image parts
                        for img in images {
                            parts.push(serde_json::json!({
                                "type": "image_url",
                                "image_url": {
                                    "url": format!("data:{};base64,{}", img.media_type, img.base64)
                                }
                            }));
                        }
                        Some(serde_json::Value::Array(parts))
                    } else {
                        m.content
                            .as_ref()
                            .map(|c| serde_json::Value::String(c.clone()))
                    }
                } else {
                    m.content
                        .as_ref()
                        .map(|c| serde_json::Value::String(c.clone()))
                };

                // Replay the assistant's own reasoning so the server's chat
                // template can keep it in context (Flint, Qwen3.x and others
                // degrade or stop reasoning when it is dropped).
                let replayed = m
                    .reasoning
                    .clone()
                    .filter(|r| m.role == "assistant" && !r.is_empty());
                let (reasoning_content, reasoning) = match reasoning_field {
                    ReasoningField::ReasoningContent => (replayed, None),
                    ReasoningField::Reasoning => (None, replayed),
                };

                ApiMessage {
                    role: m.role.clone(),
                    content,
                    tool_calls: m.tool_calls.as_ref().map(|tcs| {
                        tcs.iter()
                            .map(|tc| ApiToolCall {
                                id: tc.id.clone(),
                                r#type: "function".to_string(),
                                function: ApiToolCallFunction {
                                    name: tc.function_name.clone(),
                                    arguments: tc.arguments.clone(),
                                },
                            })
                            .collect()
                    }),
                    tool_call_id: m.tool_call_id.clone(),
                    reasoning_content,
                    reasoning,
                }
            })
            .collect();

        let api_tools: Vec<ApiTool> = tools
            .iter()
            .map(|t| ApiTool {
                r#type: "function".to_string(),
                function: ApiFunction {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.parameters.clone(),
                },
            })
            .collect();

        ChatRequest {
            model: model.to_string(),
            messages: api_messages,
            tools: api_tools,
            stream_options: stream.map(|_| StreamOptions {
                include_usage: true,
            }),
            stream,
            max_tokens: settings.max_tokens,
            temperature: settings.temperature,
            reasoning_effort: settings.reasoning_effort.clone(),
        }
    }
}

#[async_trait]
impl LlmProvider for OpenAiCompatProvider {
    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        settings: &crate::config::ModelSettings,
    ) -> Result<LlmResponse> {
        let request = Self::build_request(
            messages,
            tools,
            &settings.model,
            None,
            settings,
            self.reasoning_field(),
        );

        let mut req = self
            .client
            .post(format!("{}/chat/completions", self.base_url));

        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }

        let resp_req = req.json(&request);
        let chat_resp: ChatResponse = super::http::send_and_parse_json(resp_req, "LLM").await?;

        let choice = chat_resp
            .choices
            .into_iter()
            .next()
            .context("LLM returned no choices")?;

        let reasoning =
            pick_reasoning(&choice.message.reasoning_content, &choice.message.reasoning).map(
                |(text, field)| {
                    self.reasoning_plain
                        .store(field == ReasoningField::Reasoning, Ordering::Relaxed);
                    text.to_string()
                },
            );

        let tool_calls = choice
            .message
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .map(|tc| ToolCall {
                id: tc.id,
                function_name: tc.function.name,
                arguments: tc.function.arguments,
                thought_signature: None,
            })
            .collect();

        // LM Studio sends empty string content with tool calls; normalize to None
        let content = choice.message.content.filter(|c| !c.is_empty());
        let mut usage = chat_resp
            .usage
            .map_or(TokenUsage::default(), |u| usage_from_response(&u));
        // Same normalization as the streaming path: callers test for "max_tokens".
        if let Some(reason) = choice.finish_reason {
            usage.stop_reason = if reason == "length" {
                "max_tokens".to_string()
            } else {
                reason
            };
        }

        Ok(LlmResponse {
            content,
            tool_calls,
            reasoning,
            usage,
        })
    }

    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        settings: &crate::config::ModelSettings,
    ) -> Result<super::stream_collector::SseCollector> {
        let request = Self::build_request(
            messages,
            tools,
            &settings.model,
            Some(true),
            settings,
            self.reasoning_field(),
        );

        let mut req = self
            .client
            .post(format!("{}/chat/completions", self.base_url));

        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }

        let resp = super::http::send_or_bail(req.json(&request), "LLM").await?;

        let collector = super::stream_collector::spawn_sse_collector(
            resp,
            Box::new(OpenAiChunkParser::new().noting_reasoning_field(self.reasoning_plain.clone())),
        );

        Ok(collector)
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let mut req = self.client.get(format!("{}/models", self.base_url));

        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }

        let resp = req.send().await.context("Failed to list models")?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Models API returned error: {body}");
        }

        let body: ModelsResponse = resp.json().await.context("Failed to parse models")?;
        Ok(body
            .data
            .into_iter()
            .map(|m| ModelInfo {
                id: m.id,
                owned_by: m.owned_by,
            })
            .collect())
    }

    fn provider_name(&self) -> &str {
        "openai-compat"
    }

    async fn model_capabilities(&self, model: &str) -> Result<super::ModelCapabilities> {
        let base = self.base_url.trim_end_matches("/v1");

        // LM Studio: GET /api/v0/models → the loaded instance's
        // loaded_context_length (or the model's max_context_length).
        if let Ok(caps) = self.query_lmstudio_capabilities(base, model).await
            && caps.context_window.is_some()
        {
            return Ok(caps);
        }

        // llama.cpp: GET /props → default_generation_settings.n_ctx.
        // A plain GET that other servers answer with a 404, so it costs
        // little before the Ollama POST.
        if let Ok(caps) = self.query_llama_cpp_capabilities(base).await
            && caps.context_window.is_some()
        {
            return Ok(caps);
        }

        // Ollama: POST /api/show → model_info.general.context_length
        if let Ok(caps) = self.query_ollama_capabilities(base, model).await
            && caps.context_window.is_some()
        {
            return Ok(caps);
        }

        Ok(super::ModelCapabilities::default())
    }
}

impl OpenAiCompatProvider {
    /// Query LM Studio for the context window it enforces for `model`.
    ///
    /// The user picks the context size when loading the model (the UI, or
    /// `lms load <model> -c <n>`), so the API is the source of truth, not a
    /// lookup table. See [`lmstudio_context_length`] for which field and
    /// which entry. A non-2xx answer (another server) falls through.
    async fn query_lmstudio_capabilities(
        &self,
        base: &str,
        model: &str,
    ) -> Result<super::ModelCapabilities> {
        let resp = self
            .client
            .get(format!("{base}/api/v0/models"))
            .send()
            .await?;

        if !resp.status().is_success() {
            return Ok(super::ModelCapabilities::default());
        }

        let body: serde_json::Value = resp.json().await?;
        let ctx = lmstudio_context_length(&body, model);
        if let Some(c) = ctx {
            tracing::info!("LM Studio reports context window: {c} tokens");
        }
        Ok(super::ModelCapabilities {
            context_window: ctx,
            max_output_tokens: None,
        })
    }

    /// Query llama.cpp's `llama-server` for its context window via `/props`.
    ///
    /// The server is started with `-c <n>` and enforces that limit, so the
    /// reported `n_ctx` is the one that matters. Any other answer (404, a
    /// body without `n_ctx`) falls through to the next probe.
    async fn query_llama_cpp_capabilities(&self, base: &str) -> Result<super::ModelCapabilities> {
        let resp = self.client.get(format!("{base}/props")).send().await?;

        if !resp.status().is_success() {
            return Ok(super::ModelCapabilities::default());
        }

        let body: serde_json::Value = resp.json().await?;
        let ctx = llama_cpp_n_ctx(&body);
        if let Some(c) = ctx {
            tracing::info!("llama.cpp reports context window: {c} tokens");
        }
        Ok(super::ModelCapabilities {
            context_window: ctx,
            max_output_tokens: None,
        })
    }

    /// Query Ollama for model context window via `/api/show`.
    async fn query_ollama_capabilities(
        &self,
        base: &str,
        model: &str,
    ) -> Result<super::ModelCapabilities> {
        let resp = self
            .client
            .post(format!("{base}/api/show"))
            .json(&serde_json::json!({ "name": model }))
            .send()
            .await?;

        if !resp.status().is_success() {
            return Ok(super::ModelCapabilities::default());
        }

        let body: serde_json::Value = resp.json().await?;

        // Ollama reports context length in model_info.general.context_length
        let ctx = body["model_info"]["general.context_length"]
            .as_u64()
            .map(|v| v as usize);

        if let Some(ctx) = ctx {
            tracing::info!("Ollama reports context window: {ctx} tokens");
        }

        Ok(super::ModelCapabilities {
            context_window: ctx,
            max_output_tokens: None,
        })
    }
}

/// The context window LM Studio enforces for `model`, from a `/api/v0/models`
/// body.
///
/// LM Studio lists every downloaded model, loaded or not (it loads on demand),
/// each with `state` and the model's `max_context_length`. A loaded entry also
/// carries `loaded_context_length`, the size it was loaded with, which is the
/// limit the server enforces; `max_context_length` is only what the model
/// could take. The two differ by 4x in the 2026-10-28 eval screens (Flint:
/// 262,144 against a 65,536 load), and reading the wrong one meant Koda never
/// compacted and the server answered a bare 500.
///
/// The entry is the one whose `id` is `model`, else the loaded one (the
/// requested name may be an alias), else none: with nothing loaded and no
/// match there is no limit to report, and the caller falls through to its
/// other probes. `loaded_context_length` wins over `max_context_length`; a
/// zero or missing value counts as absent.
fn lmstudio_context_length(body: &serde_json::Value, model: &str) -> Option<usize> {
    let entries = body["data"].as_array()?;
    let entry = entries
        .iter()
        .find(|m| m["id"].as_str() == Some(model))
        .or_else(|| {
            entries
                .iter()
                .find(|m| m["state"].as_str() == Some("loaded"))
        })?;
    ["loaded_context_length", "max_context_length"]
        .into_iter()
        .find_map(|field| entry[field].as_u64().filter(|&n| n > 0))
        .map(|n| n as usize)
}

/// `default_generation_settings.n_ctx` from a llama-server `/props` body.
///
/// Zero is treated as absent: a value of 0 would make the window useless.
fn llama_cpp_n_ctx(props: &serde_json::Value) -> Option<usize> {
    props["default_generation_settings"]["n_ctx"]
        .as_u64()
        .filter(|&n| n > 0)
        .map(|n| n as usize)
}

/// Convert OpenAI usage response to our TokenUsage, extracting reasoning_tokens.
fn usage_from_response(u: &UsageResponse) -> TokenUsage {
    TokenUsage {
        prompt_tokens: u.prompt_tokens.unwrap_or(0),
        completion_tokens: u.completion_tokens.unwrap_or(0),
        thinking_tokens: u
            .completion_tokens_details
            .as_ref()
            .and_then(|d| d.reasoning_tokens)
            .unwrap_or(0),
        ..Default::default()
    }
}

#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
    owned_by: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModelSettings;
    use crate::providers::{ChatMessage, ImageData};

    fn default_settings() -> ModelSettings {
        ModelSettings::defaults_for("gpt-4o", &crate::config::ProviderType::OpenAI)
    }

    #[test]
    fn test_build_request_plain_text() {
        let settings = default_settings();
        let messages = vec![ChatMessage::text("user", "hello")];
        let request = OpenAiCompatProvider::build_request(
            &messages,
            &[],
            "gpt-4o",
            None,
            &settings,
            ReasoningField::default(),
        );
        assert_eq!(request.messages.len(), 1);
        // Plain text should be a JSON string, not an array
        let content = request.messages[0].content.as_ref().unwrap();
        assert!(
            content.is_string(),
            "Expected string content, got: {content}"
        );
        assert_eq!(content.as_str().unwrap(), "hello");
    }

    #[test]
    fn test_build_request_with_images() {
        let messages = vec![ChatMessage {
            role: "user".into(),
            content: Some("What is this?".into()),
            tool_calls: None,
            tool_call_id: None,
            images: Some(vec![ImageData {
                media_type: "image/png".into(),
                base64: "iVBORw0KGgo=".into(),
            }]),
            reasoning: None,
        }];
        let settings = default_settings();
        let request = OpenAiCompatProvider::build_request(
            &messages,
            &[],
            "gpt-4o",
            None,
            &settings,
            ReasoningField::default(),
        );
        let content = request.messages[0].content.as_ref().unwrap();

        // Should be an array with text + image_url parts
        assert!(
            content.is_array(),
            "Expected array content for images, got: {content}"
        );
        let parts = content.as_array().unwrap();
        assert_eq!(parts.len(), 2);

        // First part: text
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "What is this?");

        // Second part: image_url with data URI
        assert_eq!(parts[1]["type"], "image_url");
        let url = parts[1]["image_url"]["url"].as_str().unwrap();
        assert!(url.starts_with("data:image/png;base64,"));
        assert!(url.contains("iVBORw0KGgo="));
    }

    #[test]
    fn test_build_request_empty_images_stays_string() {
        let messages = vec![ChatMessage {
            role: "user".into(),
            content: Some("hello".into()),
            tool_calls: None,
            tool_call_id: None,
            images: Some(vec![]), // Empty images vec
            reasoning: None,
        }];
        let settings = default_settings();
        let request = OpenAiCompatProvider::build_request(
            &messages,
            &[],
            "gpt-4o",
            None,
            &settings,
            ReasoningField::default(),
        );
        let content = request.messages[0].content.as_ref().unwrap();
        // Empty images should NOT produce an array, just a string
        assert!(
            content.is_string(),
            "Empty images should produce string content"
        );
    }

    #[test]
    fn test_build_request_tool_calls_preserved() {
        let messages = vec![ChatMessage {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(vec![crate::providers::ToolCall {
                id: "tc_1".into(),
                function_name: "Read".into(),
                arguments: r#"{"path":"main.rs"}"#.into(),
                thought_signature: None,
            }]),
            tool_call_id: None,
            images: None,
            reasoning: None,
        }];
        let settings = default_settings();
        let request = OpenAiCompatProvider::build_request(
            &messages,
            &[],
            "gpt-4o",
            None,
            &settings,
            ReasoningField::default(),
        );
        assert!(request.messages[0].tool_calls.is_some());
        let tcs = request.messages[0].tool_calls.as_ref().unwrap();
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].function.name, "Read");
    }

    // ── Reasoning: read under either key, replay under the server's own ──

    fn parse_chunks(parser: &mut OpenAiChunkParser, data: &str) -> Vec<StreamChunk> {
        parser.process_line(data)
    }

    #[test]
    fn delta_reasoning_content_becomes_thinking_delta() {
        let mut parser = OpenAiChunkParser::new();
        let chunks = parse_chunks(
            &mut parser,
            r#"{"choices":[{"delta":{"reasoning_content":"hmm"},"finish_reason":null}]}"#,
        );
        assert!(matches!(&chunks[..], [StreamChunk::ThinkingDelta(t)] if t == "hmm"));
    }

    #[test]
    fn delta_reasoning_becomes_thinking_delta() {
        // rec and vllm-mlx use `reasoning`, not `reasoning_content`.
        let flag = Arc::new(AtomicBool::new(false));
        let mut parser = OpenAiChunkParser::new().noting_reasoning_field(flag.clone());
        let chunks = parse_chunks(
            &mut parser,
            r#"{"choices":[{"delta":{"reasoning":"hmm"},"finish_reason":null}]}"#,
        );
        assert!(matches!(&chunks[..], [StreamChunk::ThinkingDelta(t)] if t == "hmm"));
        assert!(flag.load(Ordering::Relaxed), "plain key must be remembered");
    }

    #[test]
    fn delta_with_empty_reasoning_emits_nothing() {
        let mut parser = OpenAiChunkParser::new();
        let chunks = parse_chunks(
            &mut parser,
            r#"{"choices":[{"delta":{"reasoning":"","content":"hi"},"finish_reason":null}]}"#,
        );
        assert!(
            !chunks
                .iter()
                .any(|c| matches!(c, StreamChunk::ThinkingDelta(_))),
            "an empty reasoning string is not a thinking delta"
        );
    }

    #[test]
    fn final_message_reads_reasoning_under_either_key() {
        let plain: ChatResponse = serde_json::from_str(
            r#"{"choices":[{"message":{"role":"assistant","content":"ok","reasoning":"why"}}]}"#,
        )
        .unwrap();
        let m = &plain.choices[0].message;
        let (text, field) = pick_reasoning(&m.reasoning_content, &m.reasoning).unwrap();
        assert_eq!((text, field), ("why", ReasoningField::Reasoning));

        let content: ChatResponse = serde_json::from_str(
            r#"{"choices":[{"message":{"role":"assistant","content":"ok","reasoning_content":"why"}}]}"#,
        )
        .unwrap();
        let m = &content.choices[0].message;
        let (text, field) = pick_reasoning(&m.reasoning_content, &m.reasoning).unwrap();
        assert_eq!((text, field), ("why", ReasoningField::ReasoningContent));
    }

    fn assistant_with_reasoning(reasoning: Option<&str>) -> ChatMessage {
        ChatMessage {
            reasoning: reasoning.map(Into::into),
            ..ChatMessage::text("assistant", "done")
        }
    }

    #[test]
    fn request_replays_reasoning_under_reasoning_content_by_default() {
        let messages = vec![
            ChatMessage::text("user", "hi"),
            assistant_with_reasoning(Some("I should greet")),
        ];
        let request = OpenAiCompatProvider::build_request(
            &messages,
            &[],
            "m",
            None,
            &default_settings(),
            ReasoningField::default(),
        );
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["messages"][1]["reasoning_content"], "I should greet");
        assert!(json["messages"][1].get("reasoning").is_none());
        // Messages without reasoning carry neither key.
        assert!(json["messages"][0].get("reasoning_content").is_none());
        assert!(json["messages"][0].get("reasoning").is_none());
    }

    #[test]
    fn request_replays_reasoning_under_plain_key_when_server_uses_it() {
        let messages = vec![assistant_with_reasoning(Some("I should greet"))];
        let request = OpenAiCompatProvider::build_request(
            &messages,
            &[],
            "m",
            None,
            &default_settings(),
            ReasoningField::Reasoning,
        );
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["messages"][0]["reasoning"], "I should greet");
        assert!(json["messages"][0].get("reasoning_content").is_none());
    }

    #[test]
    fn request_skips_empty_and_non_assistant_reasoning() {
        let mut user = ChatMessage::text("user", "hi");
        user.reasoning = Some("not an assistant".into());
        let messages = vec![user, assistant_with_reasoning(Some("")), {
            assistant_with_reasoning(None)
        }];
        let request = OpenAiCompatProvider::build_request(
            &messages,
            &[],
            "m",
            None,
            &default_settings(),
            ReasoningField::default(),
        );
        let json = serde_json::to_value(&request).unwrap();
        for m in json["messages"].as_array().unwrap() {
            assert!(m.get("reasoning_content").is_none(), "{m}");
            assert!(m.get("reasoning").is_none(), "{m}");
        }
    }

    // ── SSE StreamChatResponse / StreamChoice / StreamDelta / StreamToolCall deserialization tests ──

    #[test]
    fn stream_response_deserializes_text_delta() {
        let json = r#"{"id":"x","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":null}]}"#;
        let resp: StreamChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices.len(), 1);
        let choice = &resp.choices[0];
        assert_eq!(choice.delta.content.as_deref(), Some("hello"));
        assert!(choice.finish_reason.is_none());
    }

    #[test]
    fn stream_response_deserializes_finish_reason() {
        let json = r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#;
        let resp: StreamChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices.len(), 1);
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn stream_response_deserializes_tool_call_delta() {
        let json = r#"{
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_abc",
                        "function": {"name": "Bash", "arguments": "{\"cmd\""}
                    }]
                },
                "finish_reason": null
            }]
        }"#;
        let resp: StreamChatResponse = serde_json::from_str(json).unwrap();
        let choice = &resp.choices[0];
        let tool_calls = choice.delta.tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls.len(), 1);
        let tc = &tool_calls[0];
        assert_eq!(tc.index, Some(0));
        assert_eq!(tc.id.as_deref(), Some("call_abc"));
        let func = tc.function.as_ref().unwrap();
        assert_eq!(func.name.as_deref(), Some("Bash"));
        assert_eq!(func.arguments.as_deref(), Some("{\"cmd\""));
    }

    #[test]
    fn stream_response_handles_null_content() {
        let json = r#"{"choices":[{"delta":{"content":null},"finish_reason":null}]}"#;
        let resp: StreamChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices.len(), 1);
        assert!(resp.choices[0].delta.content.is_none());
    }

    #[test]
    fn stream_response_handles_empty_choices() {
        let json = r#"{"choices":[]}"#;
        let result = serde_json::from_str::<StreamChatResponse>(json);
        assert!(result.is_ok());
        let resp = result.unwrap();
        assert!(resp.choices.is_empty());
    }

    #[test]
    fn llama_cpp_props_reads_n_ctx() {
        let props = serde_json::json!({
            "default_generation_settings": { "n_ctx": 32768, "params": { "temperature": 0.8 } },
            "total_slots": 1,
            "model_path": "Qwen3.8-27B-Q4_K_M.gguf"
        });
        assert_eq!(llama_cpp_n_ctx(&props), Some(32768));
    }

    /// LM Studio on 2026-10-28 with Flint loaded through
    /// `lms load recursivesi/flint -c 65536` (fields not read here left
    /// out): every downloaded model is listed, the loaded one is not first,
    /// and only it carries `loaded_context_length`.
    const LMSTUDIO_MODELS: &str = r#"{"object":"list","data":[
        {"id":"qwen/qwen3-coder-next","type":"llm","state":"not-loaded","max_context_length":262144,"capabilities":["tool_use"]},
        {"id":"recursivesi/flint","type":"llm","state":"loaded","max_context_length":262144,"loaded_context_length":65536,"capabilities":["tool_use"]},
        {"id":"mlx-community/qwen3.8-27b","type":"vlm","state":"not-loaded","max_context_length":262144,"capabilities":["tool_use","vision"]}
    ]}"#;

    #[test]
    fn lm_studio_reads_the_loaded_context_of_the_requested_model() {
        let body: serde_json::Value = serde_json::from_str(LMSTUDIO_MODELS).unwrap();
        // The requested model: its load size, not its maximum and not the
        // first entry's.
        assert_eq!(
            lmstudio_context_length(&body, "recursivesi/flint"),
            Some(65536)
        );
        // An unlisted name (an alias, say) still finds the loaded model.
        assert_eq!(lmstudio_context_length(&body, "auto-detect"), Some(65536));
        // A listed model that is not loaded has only its maximum to offer.
        assert_eq!(
            lmstudio_context_length(&body, "qwen/qwen3-coder-next"),
            Some(262144)
        );
    }

    #[test]
    fn lm_studio_with_nothing_loaded() {
        let body = serde_json::json!({ "data": [
            { "id": "a", "state": "not-loaded", "max_context_length": 131072 },
            { "id": "b", "state": "not-loaded", "max_context_length": 262144 },
        ]});
        // The requested model's maximum is the best available guess.
        assert_eq!(lmstudio_context_length(&body, "b"), Some(262144));
        // No match and nothing loaded: nothing to report, so the caller
        // falls through rather than trusting the first entry.
        assert_eq!(lmstudio_context_length(&body, "c"), None);
    }

    #[test]
    fn lm_studio_ignores_other_shapes() {
        for body in [
            serde_json::json!({}),
            serde_json::json!({ "data": [] }),
            serde_json::json!({ "data": [{ "id": "m", "state": "loaded" }] }),
            serde_json::json!({ "data": [{ "id": "m", "state": "loaded", "loaded_context_length": 0, "max_context_length": 0 }] }),
            serde_json::json!({ "data": [{ "id": "m", "max_context_length": "4096" }] }),
            serde_json::json!({ "default_generation_settings": { "n_ctx": 32768 } }),
        ] {
            assert_eq!(lmstudio_context_length(&body, "m"), None, "{body}");
        }
        // A zero load size falls back to the maximum.
        let body = serde_json::json!({ "data": [
            { "id": "m", "state": "loaded", "loaded_context_length": 0, "max_context_length": 4096 }
        ]});
        assert_eq!(lmstudio_context_length(&body, "m"), Some(4096));
    }

    #[test]
    fn llama_cpp_props_ignores_other_shapes() {
        for body in [
            serde_json::json!({}),
            serde_json::json!({ "default_generation_settings": {} }),
            serde_json::json!({ "default_generation_settings": { "n_ctx": 0 } }),
            serde_json::json!({ "default_generation_settings": { "n_ctx": "32768" } }),
            serde_json::json!({ "data": [{ "max_context_length": 4096 }] }),
        ] {
            assert_eq!(llama_cpp_n_ctx(&body), None, "{body}");
        }
    }

    /// `model_capabilities` against loopback servers, the `http.rs` test
    /// pattern. Multi-thread flavor because the server is `tokio::spawn`ed.
    mod capability_probes {
        use super::super::*;
        use axum::Router;
        use axum::routing::get;
        use tokio_util::sync::CancellationToken;

        async fn spawn(app: Router) -> (OpenAiCompatProvider, CancellationToken) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            // The `/v1` suffix is what users pass; probes strip it.
            let url = format!("http://{}/v1", listener.local_addr().unwrap());
            let ct = CancellationToken::new();
            let ct_server = ct.clone();
            tokio::spawn(async move {
                axum::serve(listener, app)
                    .with_graceful_shutdown(async move { ct_server.cancelled_owned().await })
                    .await
                    .ok();
            });
            (OpenAiCompatProvider::new(&url, None), ct)
        }

        const PROPS: &str = r#"{"default_generation_settings":{"n_ctx":32768},"total_slots":1}"#;
        const LMSTUDIO: &str = r#"{"data":[{"id":"m","max_context_length":131072}]}"#;
        // Two models listed, the requested one loaded second at a quarter
        // of its maximum.
        const LMSTUDIO_JIT: &str = r#"{"data":[
            {"id":"other","state":"not-loaded","max_context_length":262144},
            {"id":"m","state":"loaded","max_context_length":262144,"loaded_context_length":65536}
        ]}"#;

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn llama_server_context_comes_from_props() {
            // Like llama-server: /props answers, the LM Studio and Ollama
            // routes do not exist (axum answers 404).
            let app = Router::new().route("/props", get(|| async { PROPS }));
            let (provider, ct) = spawn(app).await;
            let caps = provider.model_capabilities("qwen").await.unwrap();
            assert_eq!(caps.context_window, Some(32768));
            ct.cancel();
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn lm_studio_still_wins_when_it_answers() {
            let app = Router::new()
                .route("/api/v0/models", get(|| async { LMSTUDIO }))
                .route("/props", get(|| async { PROPS }));
            let (provider, ct) = spawn(app).await;
            let caps = provider.model_capabilities("m").await.unwrap();
            assert_eq!(caps.context_window, Some(131072));
            ct.cancel();
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn lm_studio_reports_the_loaded_size_not_the_first_entry() {
            let app = Router::new().route("/api/v0/models", get(|| async { LMSTUDIO_JIT }));
            let (provider, ct) = spawn(app).await;
            let caps = provider.model_capabilities("m").await.unwrap();
            assert_eq!(caps.context_window, Some(65536));
            ct.cancel();
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn lm_studio_with_nothing_to_report_falls_through_to_props() {
            // Nothing loaded and the requested model not listed: the LM
            // Studio answer gives no window, so the next probe decides.
            let app = Router::new()
                .route(
                    "/api/v0/models",
                    get(|| async { r#"{"data":[{"id":"other","state":"not-loaded","max_context_length":262144}]}"# }),
                )
                .route("/props", get(|| async { PROPS }));
            let (provider, ct) = spawn(app).await;
            let caps = provider.model_capabilities("m").await.unwrap();
            assert_eq!(caps.context_window, Some(32768));
            ct.cancel();
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn unknown_server_falls_through_to_defaults() {
            let (provider, ct) = spawn(Router::new()).await;
            let caps = provider.model_capabilities("m").await.unwrap();
            assert_eq!(caps.context_window, None);
            ct.cancel();
        }
    }
}
