//! Native Google Gemini provider.
//!
//! Uses the Gemini REST API directly (not the OpenAI-compat shim).
//! Key differences from OpenAI:
//! - Auth via the `x-goog-api-key` header (Google's REST examples use it; the
//!   `?key=` query form also exists but puts the key in URLs, logs and proxies)
//! - `contents` with `parts` (not `messages` with `content`)
//! - `system_instruction` as a top-level field
//! - `functionDeclarations` for tool definitions
//! - SSE streaming via `streamGenerateContent?alt=sse`

use super::{
    ChatMessage, LlmProvider, LlmResponse, ModelCapabilities, ModelInfo, StreamChunk, TokenUsage,
    ToolCall, ToolDefinition,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Google Gemini API provider.
pub struct GeminiProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    /// Cached content name for context caching (e.g. "cachedContents/abc123").
    /// Created on first request, reused until TTL expires.
    cached_content: std::sync::Mutex<Option<CachedContentState>>,
}

impl std::fmt::Debug for GeminiProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeminiProvider")
            .field("base_url", &self.base_url)
            .field("api_key", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

/// State for Gemini explicit context caching.
#[derive(Debug, Clone)]
struct CachedContentState {
    /// The cache resource name from the API.
    name: String,
    /// Fingerprint of what was cached (model + system prompt hash).
    fingerprint: String,
    /// When the cache expires.
    expires_at: std::time::Instant,
}

impl GeminiProvider {
    /// Create a new Gemini provider with the given API key and optional base URL.
    ///
    /// **Security:** the API key travels in the `x-goog-api-key` header,
    /// never in the URL, and HTTPS is enforced for `googleapis.com`. Custom
    /// URLs (home-network LLM servers, local proxies, etc.) are left as-is.
    pub fn new(api_key: String, base_url: Option<&str>) -> Self {
        let raw_url = base_url
            .unwrap_or("https://generativelanguage.googleapis.com")
            .trim_end_matches('/');
        let url = Self::enforce_gemini_api_https(raw_url);
        Self {
            client: super::build_http_client(base_url),
            base_url: url,
            api_key,
            cached_content: std::sync::Mutex::new(None),
        }
    }

    /// Enforce HTTPS only for the official Gemini API (`googleapis.com`).
    ///
    /// The API key goes out as a header on every request, which plain HTTP
    /// would expose. Custom URLs (home-network servers, local proxies, etc.)
    /// are trusted as-is — users running their own infrastructure know their
    /// network security posture.
    fn enforce_gemini_api_https(url: &str) -> String {
        let is_google_api = url.to_lowercase().contains("googleapis.com");
        if !is_google_api {
            return url.to_string();
        }
        if url.starts_with("https://") {
            return url.to_string();
        }
        if let Some(rest) = url.strip_prefix("http://") {
            tracing::warn!(
                "Gemini API URL uses HTTP — upgrading to HTTPS to protect API key. \
                 The Gemini API key is sent as a query parameter and MUST use HTTPS."
            );
            return format!("https://{rest}");
        }
        // No scheme at all — assume HTTPS for Google API
        format!("https://{url}")
    }

    /// Build a Gemini API URL. The key is not part of it: it goes in the
    /// `x-goog-api-key` header that [`Self::get`] and [`Self::post`] attach,
    /// so URLs can be logged and traced without leaking it. (Until 0.5.1 the
    /// key was a `?key=` query parameter, which CodeQL flagged five times.)
    fn api_url(&self, path: &str) -> String {
        format!("{}/v1beta/{}", self.base_url, path)
    }

    /// Like `api_url` but with query parameters.
    fn api_url_with_params(&self, path: &str, extra: &str) -> String {
        format!("{}/v1beta/{}?{}", self.base_url, path, extra)
    }

    /// The header every Gemini request carries the key in.
    const API_KEY_HEADER: &'static str = "x-goog-api-key";

    /// A GET with the API key header attached.
    fn get(&self, url: String) -> reqwest::RequestBuilder {
        self.client
            .get(url)
            .header(Self::API_KEY_HEADER, &self.api_key)
    }

    /// A POST with the API key header attached.
    fn post(&self, url: String) -> reqwest::RequestBuilder {
        self.client
            .post(url)
            .header(Self::API_KEY_HEADER, &self.api_key)
    }

    /// Create or reuse a cached content resource for the system prompt + tools.
    /// Returns the cache name if successful, None if caching isn't available.
    async fn ensure_cached_content(
        &self,
        model: &str,
        system_instruction: Option<&serde_json::Value>,
        tools: &[GeminiToolConfig],
    ) -> Option<String> {
        // Build a fingerprint from model + system prompt + tool count
        let sys_text = system_instruction
            .map(|s| s.to_string())
            .unwrap_or_default();
        let fingerprint = format!(
            "{}:{}:{}",
            model,
            &sys_text[..sys_text.len().min(100)],
            tools.len()
        );

        // Check if existing cache is still valid
        if let Ok(guard) = self.cached_content.lock()
            && let Some(ref state) = *guard
            && state.fingerprint == fingerprint
            && state.expires_at > std::time::Instant::now()
        {
            return Some(state.name.clone());
        }

        // Need system instruction to cache anything meaningful
        let sys = system_instruction?;

        // Create cached content via the API
        let mut cache_body = serde_json::json!({
            "model": format!("models/{model}"),
            "systemInstruction": sys,
            "ttl": "300s"  // 5 minutes, refreshed on use
        });
        if !tools.is_empty() {
            cache_body["tools"] = serde_json::to_value(tools).unwrap_or_default();
        }

        let resp = self
            .post(self.api_url("cachedContents"))
            .json(&cache_body)
            .send()
            .await
            .ok()?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            tracing::debug!("Gemini cache creation failed: {body}");
            return None;
        }

        let result: serde_json::Value = resp.json().await.ok()?;
        let cache_name = result["name"].as_str()?.to_string();

        tracing::info!("Gemini: created context cache '{cache_name}'");

        let state = CachedContentState {
            name: cache_name.clone(),
            fingerprint,
            expires_at: std::time::Instant::now() + std::time::Duration::from_secs(280),
        };

        if let Ok(mut guard) = self.cached_content.lock() {
            *guard = Some(state);
        }

        Some(cache_name)
    }
}

// Request types are built via serde_json::json! for flexibility.
// Only response types need Deserialize structs.

/// An id for a function call. It must be unique across the session: history
/// keeps one result per call id, so a per-response counter that restarted at 1
/// left the model with only its newest tool result (#1454).
fn new_call_id() -> String {
    format!("gemini_tc_{}", uuid::Uuid::new_v4().simple())
}

/// Stands in for a missing thought signature. The value Gemini CLI uses.
const SYNTHETIC_THOUGHT_SIGNATURE: &str = "skip_thought_signature_validator";

/// Helper for building Gemini-format parts as JSON values.
enum Part {
    InlineData {
        mime_type: String,
        data: String,
    },
    FunctionCall {
        name: String,
        args: serde_json::Value,
    },
    FunctionResponse {
        name: String,
        response: serde_json::Value,
    },
}

impl Part {
    fn to_json(&self) -> serde_json::Value {
        match self {
            Part::InlineData { mime_type, data } => serde_json::json!({
                "inlineData": { "mimeType": mime_type, "data": data }
            }),
            Part::FunctionCall { name, args } => serde_json::json!({
                "functionCall": { "name": name, "args": args }
            }),
            Part::FunctionResponse { name, response } => serde_json::json!({
                "functionResponse": { "name": name, "response": response }
            }),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiToolConfig {
    function_declarations: Vec<FunctionDeclaration>,
}

#[derive(Serialize)]
struct FunctionDeclaration {
    name: String,
    description: String,
    /// Tool parameter schema. Sent under the `parametersJsonSchema`
    /// wire field instead of `parameters` because our schemas use
    /// JSON Schema features Gemini's strict OpenAPI 3.0 `parameters`
    /// field rejects (notably `additionalProperties: false`, which
    /// the bg-task tools use to prevent hallucinated args — and
    /// which OpenAI structured outputs also require).
    ///
    /// The Gemini API exposes both fields side by side: `parameters`
    /// is OpenAPI 3.0 (strict subset), `parametersJsonSchema` is
    /// full JSON Schema. The 1st-party gemini-cli uses the
    /// JSON-Schema field for the same reason. See:
    /// https://ai.google.dev/api/caching#FunctionDeclaration
    #[serde(rename = "parametersJsonSchema")]
    parameters: serde_json::Value,
}

// ── Tool config ──────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerateResponse {
    candidates: Option<Vec<Candidate>>,
    usage_metadata: Option<UsageMetadata>,
    /// Set instead of `candidates` when the prompt itself was blocked.
    #[serde(default)]
    prompt_feedback: Option<PromptFeedback>,
}

impl GenerateResponse {
    /// Why generation stopped, lowercased, if this response says so.
    fn stop_reason(&self) -> Option<String> {
        let candidate = self.candidates.iter().flatten().rev().find_map(|c| {
            if let Some(message) = &c.finish_message {
                tracing::warn!("Gemini finish message: {message}");
            }
            c.finish_reason.as_ref()
        });
        let blocked = self.prompt_feedback.as_ref();
        candidate
            .or(blocked.and_then(|f| f.block_reason.as_ref()))
            .map(|reason| reason.to_lowercase())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PromptFeedback {
    #[serde(default)]
    block_reason: Option<String>,
}

#[derive(Deserialize)]
struct Candidate {
    content: Option<CandidateContent>,
    /// "STOP", "MAX_TOKENS", "SAFETY", "RECITATION", etc.
    #[serde(default, rename = "finishReason")]
    finish_reason: Option<String>,
    /// The API's own explanation of `finish_reason`, when it gives one.
    #[serde(default, rename = "finishMessage")]
    finish_message: Option<String>,
}

#[derive(Deserialize)]
struct CandidateContent {
    parts: Option<Vec<ResponsePart>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResponsePart {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    function_call: Option<FunctionCallResponse>,
    /// Thought signature that must be echoed back when replaying this part.
    #[serde(default)]
    thought_signature: Option<String>,
    /// When true, this part contains thinking/reasoning content (not user-visible output).
    #[serde(default)]
    thought: Option<bool>,
}

#[derive(Deserialize)]
struct FunctionCallResponse {
    name: String,
    args: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageMetadata {
    #[serde(default)]
    prompt_token_count: i64,
    #[serde(default)]
    candidates_token_count: i64,
    #[serde(default)]
    cached_content_token_count: i64,
    /// Tokens used for thinking/reasoning.
    #[serde(default)]
    thoughts_token_count: i64,
}

impl UsageMetadata {
    fn log_cache_stats(&self) {
        if self.cached_content_token_count > 0 {
            tracing::debug!(
                "Gemini cache: cached={}tok, prompt={}tok",
                self.cached_content_token_count,
                self.prompt_token_count,
            );
        }
    }
}

// ── Models list response ─────────────────────────────────────

#[derive(Deserialize)]
struct ModelsResponse {
    models: Option<Vec<GeminiModelInfo>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiModelInfo {
    name: String,
    #[serde(default)]
    supported_generation_methods: Vec<String>,
    /// Max input tokens (reported by the Gemini models API).
    #[serde(default)]
    input_token_limit: Option<usize>,
    /// Max output tokens (reported by the Gemini models API).
    #[serde(default)]
    output_token_limit: Option<usize>,
}

// ── ChunkParser implementation ──────────────────────────────────

use super::stream_collector::ChunkParser;

/// Gemini SSE chunk parser.
///
/// Gemini sends complete JSON snapshot objects per SSE event (not incremental
/// deltas like Anthropic/OpenAI). Each event contains the full candidate content
/// plus usage metadata.
pub(crate) struct GeminiChunkParser {
    tool_calls: Vec<ToolCall>,
    usage: TokenUsage,
    /// A signature on a text or thought part, for the call that follows it.
    pending_signature: Option<String>,
}

impl GeminiChunkParser {
    pub fn new() -> Self {
        Self {
            tool_calls: Vec::new(),
            usage: TokenUsage::default(),
            pending_signature: None,
        }
    }
}

impl ChunkParser for GeminiChunkParser {
    fn process_line(&mut self, data: &str) -> Vec<StreamChunk> {
        let Ok(event) = serde_json::from_str::<GenerateResponse>(data) else {
            return vec![];
        };

        let mut chunks = Vec::new();

        // Extract usage (last event has final totals)
        if let Some(usage) = &event.usage_metadata {
            self.usage.prompt_tokens = usage.prompt_token_count;
            self.usage.completion_tokens = usage.candidates_token_count;
            self.usage.cache_read_tokens = usage.cached_content_token_count;
            self.usage.thinking_tokens = usage.thoughts_token_count;
            usage.log_cache_stats();
        }

        if let Some(reason) = event.stop_reason() {
            self.usage.stop_reason = reason;
        }

        if let Some(candidates) = &event.candidates {
            for candidate in candidates {
                if let Some(content) = &candidate.content
                    && let Some(parts) = &content.parts
                {
                    for part in parts {
                        if let Some(text) = &part.text
                            && !text.is_empty()
                        {
                            if part.thought == Some(true) {
                                chunks.push(StreamChunk::ThinkingDelta(text.clone()));
                            } else {
                                chunks.push(StreamChunk::TextDelta(text.clone()));
                            }
                        }
                        if let Some(fc) = &part.function_call {
                            self.tool_calls.push(ToolCall {
                                id: new_call_id(),
                                function_name: fc.name.clone(),
                                arguments: serde_json::to_string(&fc.args).unwrap_or_default(),
                                thought_signature: part
                                    .thought_signature
                                    .clone()
                                    .or(self.pending_signature.take()),
                            });
                        } else if part.thought_signature.is_some() {
                            self.pending_signature = part.thought_signature.clone();
                        }
                    }
                }
            }
        }

        chunks
    }

    fn finish(&mut self) -> Vec<StreamChunk> {
        let mut chunks = Vec::new();
        let signature_chars: usize = self
            .tool_calls
            .iter()
            .filter_map(|tc| tc.thought_signature.as_ref())
            .map(String::len)
            .sum();
        // Each signature goes back with every later request and counts toward
        // the prompt, so a long think shows up as a jump in context.
        if signature_chars > 0 {
            tracing::info!(
                "Gemini thought signature: {signature_chars} chars, {} thinking tokens, \
                 {} prompt tokens",
                self.usage.thinking_tokens,
                self.usage.prompt_tokens,
            );
        }
        if !self.tool_calls.is_empty() {
            chunks.push(StreamChunk::ToolCalls(std::mem::take(&mut self.tool_calls)));
        }
        chunks.push(StreamChunk::Done(std::mem::take(&mut self.usage)));
        chunks
    }
}

// ── Implementation ───────────────────────────────────────────────

#[async_trait]
impl LlmProvider for GeminiProvider {
    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        settings: &crate::config::ModelSettings,
    ) -> Result<LlmResponse> {
        let model = &settings.model;
        let (contents, system_instruction) = self.convert_messages(messages);
        let api_tools = Self::build_tools(tools);

        // Try to use cached content for system prompt + tools
        let cache_name = self
            .ensure_cached_content(model, system_instruction.as_ref(), &api_tools)
            .await;

        let body = self.build_request_body_with_cache(
            &contents,
            system_instruction.as_ref(),
            &api_tools,
            Some(settings),
            cache_name.as_deref(),
        );

        let req = self
            .post(self.api_url(&format!("models/{model}:generateContent")))
            .json(&body);

        let gen_resp: GenerateResponse = super::http::send_and_parse_json(req, "Gemini").await?;

        self.parse_response(gen_resp)
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let resp = self
            .get(self.api_url("models"))
            .send()
            .await
            .context("Failed to list Gemini models")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            if status.as_u16() == 400 || status.as_u16() == 403 {
                anyhow::bail!("Invalid API key: {body}");
            }
            anyhow::bail!("Gemini API returned {status}: {body}");
        }

        let models_resp: ModelsResponse = resp.json().await?;
        let models = models_resp
            .models
            .unwrap_or_default()
            .into_iter()
            .filter(|m| {
                m.supported_generation_methods
                    .iter()
                    .any(|method| method == "generateContent")
            })
            .map(|m| {
                // "models/gemini-2.0-flash" → "gemini-2.0-flash"
                let id = m.name.strip_prefix("models/").unwrap_or(&m.name);
                ModelInfo {
                    id: id.to_string(),
                    owned_by: Some("google".to_string()),
                }
            })
            .collect();

        Ok(models)
    }

    fn provider_name(&self) -> &str {
        "gemini"
    }

    async fn model_capabilities(&self, model: &str) -> Result<ModelCapabilities> {
        let resp = self
            .get(self.api_url(&format!("models/{model}")))
            .send()
            .await
            .context("Failed to query Gemini model info")?;

        if !resp.status().is_success() {
            return Ok(ModelCapabilities::default());
        }

        let info: GeminiModelInfo = resp.json().await.unwrap_or(GeminiModelInfo {
            name: model.to_string(),
            supported_generation_methods: vec![],
            input_token_limit: None,
            output_token_limit: None,
        });

        Ok(ModelCapabilities {
            context_window: info.input_token_limit,
            max_output_tokens: info.output_token_limit,
        })
    }

    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        settings: &crate::config::ModelSettings,
    ) -> Result<super::stream_collector::SseCollector> {
        let model = &settings.model;
        let (contents, system_instruction) = self.convert_messages(messages);
        let api_tools = Self::build_tools(tools);

        let body = self.build_request_body(
            &contents,
            system_instruction.as_ref(),
            &api_tools,
            Some(settings),
        );

        let req = self
            .post(
                self.api_url_with_params(
                    &format!("models/{model}:streamGenerateContent"),
                    "alt=sse",
                ),
            )
            .json(&body);

        let resp = super::http::send_or_bail(req, "Gemini").await?;

        let collector =
            super::stream_collector::spawn_sse_collector(resp, Box::new(GeminiChunkParser::new()));

        Ok(collector)
    }
}

impl GeminiProvider {
    /// Convert ChatMessage list to Gemini's contents + system_instruction.
    fn convert_messages(
        &self,
        messages: &[ChatMessage],
    ) -> (Vec<serde_json::Value>, Option<serde_json::Value>) {
        let mut contents = Vec::new();
        let mut system_instruction = None;
        let mut function_names = std::collections::HashMap::new();
        let mut after_tool_result = false;

        for msg in messages {
            if msg.role == "system" {
                // Gemini uses system_instruction as a top-level field
                if let Some(text) = &msg.content {
                    system_instruction = Some(serde_json::json!({
                        "parts": [{ "text": text }]
                    }));
                }
                continue;
            }

            let role = match msg.role.as_str() {
                "assistant" => "model",
                other => other,
            };

            // Tool result message: a user turn holding one functionResponse
            // per call of the model's turn, named after the function, not the
            // call id. Newer models reject the old `function` role.
            if msg.role == "tool"
                && let Some(content) = &msg.content
            {
                let id = msg.tool_call_id.clone().unwrap_or_default();
                let name = function_names.get(&id).cloned().unwrap_or(id);
                let part = Part::FunctionResponse {
                    name,
                    response: serde_json::json!({ "result": content }),
                }
                .to_json();
                if after_tool_result
                    && let Some(parts) = contents
                        .last_mut()
                        .and_then(|c: &mut serde_json::Value| c["parts"].as_array_mut())
                {
                    parts.push(part);
                } else {
                    contents.push(serde_json::json!({ "role": "user", "parts": [part] }));
                }
                after_tool_result = true;
                continue;
            }
            after_tool_result = false;

            // Assistant with tool calls
            if msg.role == "assistant"
                && let Some(tcs) = &msg.tool_calls
            {
                let mut parts = Vec::new();
                if let Some(text) = &msg.content
                    && !text.is_empty()
                {
                    parts.push(serde_json::json!({ "text": text }));
                }
                for (i, tc) in tcs.iter().enumerate() {
                    function_names.insert(tc.id.clone(), tc.function_name.clone());
                    let args: serde_json::Value =
                        serde_json::from_str(&tc.arguments).unwrap_or_default();
                    let mut fc_part = Part::FunctionCall {
                        name: tc.function_name.clone(),
                        args,
                    }
                    .to_json();
                    // Echo back thought_signature if present (required by Gemini
                    // API). The first call of a model turn must have one, or the
                    // request gets a 400, so a turn that came back without one,
                    // or from another model, gets the bypass value.
                    if let Some(ref sig) = tc.thought_signature {
                        fc_part["thoughtSignature"] = serde_json::json!(sig);
                    } else if i == 0 {
                        fc_part["thoughtSignature"] =
                            serde_json::json!(SYNTHETIC_THOUGHT_SIGNATURE);
                    }
                    parts.push(fc_part);
                }
                contents.push(serde_json::json!({ "role": "model", "parts": parts }));
                continue;
            }

            // Regular user/assistant message
            let mut parts = Vec::new();

            // Images first
            if let Some(images) = &msg.images {
                for img in images {
                    parts.push(
                        Part::InlineData {
                            mime_type: img.media_type.clone(),
                            data: img.base64.clone(),
                        }
                        .to_json(),
                    );
                }
            }

            // Text
            if let Some(text) = &msg.content
                && !text.is_empty()
            {
                parts.push(serde_json::json!({ "text": text }));
            }

            if !parts.is_empty() {
                contents.push(serde_json::json!({ "role": role, "parts": parts }));
            }
        }

        (contents, system_instruction)
    }

    /// Convert tool definitions to Gemini's functionDeclarations format.
    fn build_tools(tools: &[ToolDefinition]) -> Vec<GeminiToolConfig> {
        if tools.is_empty() {
            return Vec::new();
        }
        let declarations: Vec<FunctionDeclaration> = tools
            .iter()
            .map(|t| FunctionDeclaration {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.parameters.clone(),
            })
            .collect();
        vec![GeminiToolConfig {
            function_declarations: declarations,
        }]
    }

    /// Build the full request body as JSON.
    fn build_request_body(
        &self,
        contents: &[serde_json::Value],
        system_instruction: Option<&serde_json::Value>,
        tools: &[GeminiToolConfig],
        settings: Option<&crate::config::ModelSettings>,
    ) -> serde_json::Value {
        self.build_request_body_with_cache(contents, system_instruction, tools, settings, None)
    }

    /// Build the request body, optionally referencing cached content.
    fn build_request_body_with_cache(
        &self,
        contents: &[serde_json::Value],
        system_instruction: Option<&serde_json::Value>,
        tools: &[GeminiToolConfig],
        settings: Option<&crate::config::ModelSettings>,
        cached_content_name: Option<&str>,
    ) -> serde_json::Value {
        // No cap unless one was set: thinking counts against it, so a default
        // here can leave a hard turn with nothing to say. Gemini CLI sets none.
        let mut gen_config = serde_json::json!({});
        if let Some(max_output) = settings.and_then(|s| s.max_tokens) {
            gen_config["maxOutputTokens"] = serde_json::json!(max_output);
        }
        if let Some(temp) = settings.and_then(|s| s.temperature) {
            gen_config["temperature"] = serde_json::json!(temp);
        }
        // Enable Gemini thinking/reasoning when thinking_budget is set
        if let Some(budget) = settings.and_then(|s| s.thinking_budget) {
            gen_config["thinkingConfig"] = serde_json::json!({
                "thinkingBudget": budget
            });
        }
        let mut body = serde_json::json!({
            "contents": contents,
            "generationConfig": gen_config,
        });

        // If we have a cached content reference, use it instead of
        // re-sending system instruction + tools (saves tokens + latency)
        if let Some(cache_name) = cached_content_name {
            body["cachedContent"] = serde_json::json!(cache_name);
            // Don't re-send system instruction or tools — they're in the cache
        } else {
            if let Some(sys) = system_instruction {
                body["systemInstruction"] = sys.clone();
            }
            if !tools.is_empty() {
                body["tools"] = serde_json::to_value(tools).unwrap_or_default();
            }
        }
        body
    }

    /// Parse Gemini's response into our unified format.
    fn parse_response(&self, resp: GenerateResponse) -> Result<LlmResponse> {
        let mut content_text = String::new();
        let mut tool_calls = Vec::new();
        let mut pending_signature = None;
        let stop_reason = resp.stop_reason().unwrap_or_default();

        if let Some(candidates) = resp.candidates {
            for candidate in candidates {
                if let Some(content) = candidate.content
                    && let Some(parts) = content.parts
                {
                    for part in parts {
                        if let Some(text) = part.text {
                            // Skip thinking parts in non-streaming mode (they're internal reasoning)
                            if part.thought != Some(true) {
                                content_text.push_str(&text);
                            }
                        }
                        if let Some(fc) = part.function_call {
                            tool_calls.push(ToolCall {
                                id: new_call_id(),
                                function_name: fc.name,
                                arguments: serde_json::to_string(&fc.args)?,
                                thought_signature: part
                                    .thought_signature
                                    .or(pending_signature.take()),
                            });
                        } else if part.thought_signature.is_some() {
                            pending_signature = part.thought_signature;
                        }
                    }
                }
            }
        }

        let usage = resp.usage_metadata.unwrap_or(UsageMetadata {
            prompt_token_count: 0,
            candidates_token_count: 0,
            cached_content_token_count: 0,
            thoughts_token_count: 0,
        });
        usage.log_cache_stats();

        Ok(LlmResponse {
            content: if content_text.is_empty() {
                None
            } else {
                Some(content_text)
            },
            tool_calls,
            reasoning: None,
            usage: TokenUsage {
                prompt_tokens: usage.prompt_token_count,
                completion_tokens: usage.candidates_token_count,
                cache_read_tokens: usage.cached_content_token_count,
                thinking_tokens: usage.thoughts_token_count,
                stop_reason,
                ..Default::default()
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_provider() -> GeminiProvider {
        GeminiProvider::new("fake-key".into(), None)
    }

    // ── API key placement ───────────────────────────────────

    #[test]
    fn api_urls_never_carry_the_key() {
        let p = GeminiProvider::new("sk-secret".into(), None);
        let plain = p.api_url("models/gemini-2.5-pro:generateContent");
        let with = p.api_url_with_params("models/gemini-2.5-pro:streamGenerateContent", "alt=sse");
        assert_eq!(
            plain,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:generateContent"
        );
        assert_eq!(
            with,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse"
        );
        for url in [&plain, &with] {
            assert!(!url.contains("sk-secret"), "key leaked into {url}");
            assert!(!url.contains("key="), "query key in {url}");
        }
    }

    #[test]
    fn requests_carry_the_key_in_the_header() {
        let p = GeminiProvider::new("sk-secret".into(), None);
        for req in [
            p.get(p.api_url("models")),
            p.post(p.api_url("cachedContents")),
        ] {
            let built = req.build().expect("request builds");
            assert_eq!(
                built
                    .headers()
                    .get(GeminiProvider::API_KEY_HEADER)
                    .and_then(|v| v.to_str().ok()),
                Some("sk-secret")
            );
            assert!(!built.url().as_str().contains("sk-secret"));
        }
    }

    // ── HTTPS enforcement tests ──────────────────────────────

    #[test]
    fn test_enforce_https_already_https() {
        let url =
            GeminiProvider::enforce_gemini_api_https("https://generativelanguage.googleapis.com");
        assert_eq!(url, "https://generativelanguage.googleapis.com");
    }

    #[test]
    fn test_enforce_https_upgrades_google_api_http() {
        let url =
            GeminiProvider::enforce_gemini_api_https("http://generativelanguage.googleapis.com");
        assert_eq!(url, "https://generativelanguage.googleapis.com");
    }

    #[test]
    fn test_enforce_https_allows_localhost_http() {
        let url = GeminiProvider::enforce_gemini_api_https("http://localhost:8080");
        assert_eq!(url, "http://localhost:8080");
    }

    #[test]
    fn test_enforce_https_allows_home_network_http() {
        let url = GeminiProvider::enforce_gemini_api_https("http://192.168.1.50:8080");
        assert_eq!(url, "http://192.168.1.50:8080");
    }

    #[test]
    fn test_enforce_https_allows_custom_server_http() {
        let url = GeminiProvider::enforce_gemini_api_https("http://my-llm-server.local:9000");
        assert_eq!(url, "http://my-llm-server.local:9000");
    }

    #[test]
    fn test_enforce_https_no_scheme_google_gets_https() {
        let url = GeminiProvider::enforce_gemini_api_https("generativelanguage.googleapis.com");
        assert_eq!(url, "https://generativelanguage.googleapis.com");
    }

    #[test]
    fn test_new_default_url_is_https() {
        let p = GeminiProvider::new("key".into(), None);
        assert!(p.base_url.starts_with("https://"));
    }

    #[test]
    fn test_new_upgrades_google_api_http_to_https() {
        let p = GeminiProvider::new(
            "key".into(),
            Some("http://generativelanguage.googleapis.com"),
        );
        assert!(p.base_url.starts_with("https://"));
    }

    #[test]
    fn test_new_preserves_custom_http_url() {
        let p = GeminiProvider::new("key".into(), Some("http://192.168.1.100:8080"));
        assert_eq!(p.base_url, "http://192.168.1.100:8080");
    }

    #[test]
    fn test_convert_extracts_system_instruction() {
        let p = make_provider();
        let messages = vec![
            ChatMessage::text("system", "You are a bear."),
            ChatMessage::text("user", "hello"),
        ];
        let (contents, sys) = p.convert_messages(&messages);
        assert!(sys.is_some());
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "user");
    }

    #[test]
    fn test_convert_user_message() {
        let p = make_provider();
        let messages = vec![ChatMessage::text("user", "explain this")];
        let (contents, _) = p.convert_messages(&messages);
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"][0]["text"], "explain this");
    }

    #[test]
    fn test_convert_assistant_becomes_model() {
        let p = make_provider();
        let messages = vec![ChatMessage::text("assistant", "sure!")];
        let (contents, _) = p.convert_messages(&messages);
        assert_eq!(contents[0]["role"], "model");
    }

    fn tool_result(id: &str, output: &str) -> ChatMessage {
        ChatMessage {
            role: "tool".into(),
            content: Some(output.into()),
            tool_calls: None,
            tool_call_id: Some(id.into()),
            images: None,
            reasoning: None,
        }
    }

    #[test]
    fn test_convert_tool_results_join_one_user_turn_by_function_name() {
        let p = make_provider();
        let call = |id: &str, name: &str| ToolCall {
            id: id.into(),
            function_name: name.into(),
            arguments: "{}".into(),
            thought_signature: None,
        };
        let messages = vec![
            ChatMessage {
                role: "assistant".into(),
                content: None,
                tool_calls: Some(vec![
                    call("gemini_tc_1", "Grep"),
                    call("gemini_tc_2", "Read"),
                ]),
                tool_call_id: None,
                images: None,
                reasoning: None,
            },
            tool_result("gemini_tc_1", "a match"),
            tool_result("gemini_tc_2", "file contents"),
        ];
        let (contents, _) = p.convert_messages(&messages);
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[1]["role"], "user");
        let parts = contents[1]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["functionResponse"]["name"], "Grep");
        assert_eq!(parts[1]["functionResponse"]["name"], "Read");
    }

    #[test]
    fn test_convert_tool_result_without_its_call_keeps_the_id() {
        let p = make_provider();
        let (contents, _) = p.convert_messages(&[tool_result("Read", "file contents")]);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"][0]["functionResponse"]["name"], "Read");
    }

    #[test]
    fn test_convert_assistant_with_tool_calls() {
        let p = make_provider();
        let messages = vec![ChatMessage {
            role: "assistant".into(),
            content: Some("Let me read that.".into()),
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
        let (contents, _) = p.convert_messages(&messages);
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "model");
        let parts = contents[0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2); // text + functionCall
    }

    #[test]
    fn test_convert_image_message() {
        let p = make_provider();
        let messages = vec![ChatMessage {
            role: "user".into(),
            content: Some("What is this?".into()),
            tool_calls: None,
            tool_call_id: None,
            images: Some(vec![super::super::ImageData {
                media_type: "image/png".into(),
                base64: "iVBORw0KGgo=".into(),
            }]),
            reasoning: None,
        }];
        let (contents, _) = p.convert_messages(&messages);
        assert_eq!(contents.len(), 1);
        let parts = contents[0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2); // image + text
        assert!(parts[0].get("inlineData").is_some());
        assert!(parts[1].get("text").is_some());
    }

    #[test]
    fn test_build_tools() {
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
        let gemini_tools = GeminiProvider::build_tools(&tools);
        assert_eq!(gemini_tools.len(), 1); // One tool config with two declarations
        assert_eq!(gemini_tools[0].function_declarations.len(), 2);
    }

    #[test]
    fn test_build_tools_empty() {
        let tools = GeminiProvider::build_tools(&[]);
        assert!(tools.is_empty());
    }

    /// Regression for the 400 we'd get sending JSON-Schema features
    /// (notably `additionalProperties: false`, which the bg-task
    /// tools use) under Gemini's strict OpenAPI-3.0 `parameters`
    /// field. Fix is to serialize as `parametersJsonSchema` instead
    /// — the JSON-Schema-aware sibling field. The 1st-party
    /// gemini-cli does the same. See:
    /// https://ai.google.dev/api/caching#FunctionDeclaration
    ///
    /// Without this rename the API rejects the request with:
    ///   Unknown name "additionalProperties" at
    ///   'tools[0].function_declarations[N].parameters'
    #[test]
    fn function_declaration_serializes_under_parameters_json_schema() {
        let tools = vec![ToolDefinition {
            name: "CancelTask".into(),
            description: "cancel a bg task".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" }
                },
                "required": ["task_id"],
                "additionalProperties": false
            }),
        }];
        let gemini_tools = GeminiProvider::build_tools(&tools);
        let wire = serde_json::to_value(&gemini_tools[0]).unwrap();
        let decl = &wire["functionDeclarations"][0];

        // The whole point: schema lives under `parametersJsonSchema`,
        // NOT `parameters`. Sending under `parameters` triggers the
        // "Unknown name 'additionalProperties'" 400.
        assert!(
            decl.get("parametersJsonSchema").is_some(),
            "expected `parametersJsonSchema` field in wire payload, got: {wire}"
        );
        assert!(
            decl.get("parameters").is_none(),
            "`parameters` field must NOT appear (Gemini rejects JSON Schema there): {wire}"
        );

        // The schema itself must be passed through verbatim — no
        // sanitization, no field-stripping. additionalProperties:
        // false is a feature, not a bug: it stops Gemini from
        // hallucinating extra args, exactly like OpenAI strict mode.
        assert_eq!(
            decl["parametersJsonSchema"]["additionalProperties"],
            serde_json::json!(false),
            "additionalProperties must survive intact: {wire}"
        );
    }

    #[test]
    fn test_parse_text_response() {
        let p = make_provider();
        let resp = GenerateResponse {
            prompt_feedback: None,
            candidates: Some(vec![Candidate {
                finish_reason: None,
                finish_message: None,
                content: Some(CandidateContent {
                    parts: Some(vec![ResponsePart {
                        text: Some("Hello!".into()),
                        function_call: None,
                        thought_signature: None,
                        thought: None,
                    }]),
                }),
            }]),
            usage_metadata: Some(UsageMetadata {
                prompt_token_count: 10,
                candidates_token_count: 5,
                cached_content_token_count: 0,
                thoughts_token_count: 0,
            }),
        };
        let result = p.parse_response(resp).unwrap();
        assert_eq!(result.content.as_deref(), Some("Hello!"));
        assert!(result.tool_calls.is_empty());
        assert_eq!(result.usage.prompt_tokens, 10);
        assert_eq!(result.usage.completion_tokens, 5);
    }

    #[test]
    fn test_parse_tool_call_response() {
        let p = make_provider();
        let resp = GenerateResponse {
            prompt_feedback: None,
            candidates: Some(vec![Candidate {
                finish_reason: None,
                finish_message: None,
                content: Some(CandidateContent {
                    parts: Some(vec![ResponsePart {
                        text: None,
                        function_call: Some(FunctionCallResponse {
                            name: "Read".into(),
                            args: serde_json::json!({"path": "main.rs"}),
                        }),
                        thought_signature: None,
                        thought: None,
                    }]),
                }),
            }]),
            usage_metadata: None,
        };
        let result = p.parse_response(resp).unwrap();
        assert!(result.content.is_none());
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].function_name, "Read");
    }

    #[test]
    fn test_parse_response_filters_thinking_parts() {
        let p = make_provider();
        let resp = GenerateResponse {
            prompt_feedback: None,
            candidates: Some(vec![Candidate {
                finish_reason: None,
                finish_message: None,
                content: Some(CandidateContent {
                    parts: Some(vec![
                        ResponsePart {
                            text: Some("Let me think about this...".into()),
                            function_call: None,
                            thought_signature: None,
                            thought: Some(true), // This is thinking — should be excluded
                        },
                        ResponsePart {
                            text: Some("Here's the answer.".into()),
                            function_call: None,
                            thought_signature: None,
                            thought: None, // Regular output
                        },
                    ]),
                }),
            }]),
            usage_metadata: Some(UsageMetadata {
                prompt_token_count: 10,
                candidates_token_count: 20,
                cached_content_token_count: 0,
                thoughts_token_count: 15,
            }),
        };
        let result = p.parse_response(resp).unwrap();
        // Thinking parts should be excluded from content
        assert_eq!(result.content.as_deref(), Some("Here's the answer."));
        // Thinking tokens should be tracked
        assert_eq!(result.usage.thinking_tokens, 15);
    }

    #[test]
    fn test_build_request_includes_thinking_config() {
        let p = make_provider();
        let settings = crate::config::ModelSettings {
            model: "gemini-2.5-flash".into(),
            max_tokens: Some(8192),
            temperature: None,
            thinking_budget: Some(10000),
            reasoning_effort: None,
            max_context_tokens: 32_000,
        };
        let body = p.build_request_body(&[], None, &[], Some(&settings));
        let thinking_config = &body["generationConfig"]["thinkingConfig"];
        assert_eq!(thinking_config["thinkingBudget"], 10000);
    }

    #[test]
    fn test_build_request_no_thinking_config_when_unset() {
        let p = make_provider();
        let settings = crate::config::ModelSettings {
            model: "gemini-2.0-flash".into(),
            max_tokens: Some(8192),
            temperature: None,
            thinking_budget: None,
            reasoning_effort: None,
            max_context_tokens: 32_000,
        };
        let body = p.build_request_body(&[], None, &[], Some(&settings));
        assert!(body["generationConfig"]["thinkingConfig"].is_null());
    }

    // ── GenerateResponse / Candidate / ResponsePart deserialization tests ──

    #[test]
    fn generate_response_deserializes_text_part() {
        let json = r#"{
            "candidates": [{
                "content": {
                    "parts": [{"text": "Hello, world!"}]
                }
            }]
        }"#;
        let resp: GenerateResponse = serde_json::from_str(json).unwrap();
        let candidates = resp.candidates.unwrap();
        assert_eq!(candidates.len(), 1);
        let parts = candidates[0]
            .content
            .as_ref()
            .unwrap()
            .parts
            .as_ref()
            .unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].text.as_deref(), Some("Hello, world!"));
        assert!(parts[0].thought.is_none());
    }

    #[test]
    fn generate_response_deserializes_thinking_part() {
        let json = r#"{
            "candidates": [{
                "content": {
                    "parts": [{"text": "Let me reason...", "thought": true}]
                }
            }]
        }"#;
        let resp: GenerateResponse = serde_json::from_str(json).unwrap();
        let candidates = resp.candidates.unwrap();
        let parts = candidates[0]
            .content
            .as_ref()
            .unwrap()
            .parts
            .as_ref()
            .unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].text.as_deref(), Some("Let me reason..."));
        assert_eq!(parts[0].thought, Some(true));
    }

    #[test]
    fn test_first_call_of_a_turn_always_has_a_signature() {
        let p = make_provider();
        let call = |id: &str, sig: Option<&str>| ToolCall {
            id: id.into(),
            function_name: "Read".into(),
            arguments: "{}".into(),
            thought_signature: sig.map(String::from),
        };
        let assistant = |calls| ChatMessage {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(calls),
            tool_call_id: None,
            images: None,
            reasoning: None,
        };
        let messages = vec![
            assistant(vec![call("a", None), call("b", None)]),
            tool_result("a", "x"),
            tool_result("b", "y"),
            assistant(vec![call("c", Some("real"))]),
        ];
        let (contents, _) = p.convert_messages(&messages);
        let first = &contents[0]["parts"];
        assert_eq!(first[0]["thoughtSignature"], SYNTHETIC_THOUGHT_SIGNATURE);
        assert!(first[1].get("thoughtSignature").is_none());
        assert_eq!(contents[2]["parts"][0]["thoughtSignature"], "real");
    }

    #[test]
    fn test_signature_on_an_earlier_part_reaches_the_call() {
        let p = make_provider();
        let resp: GenerateResponse = serde_json::from_value(serde_json::json!({
            "candidates": [{
                "content": { "parts": [
                    { "text": "thinking", "thought": true, "thoughtSignature": "sig" },
                    { "functionCall": { "name": "Read", "args": {} } }
                ] }
            }]
        }))
        .unwrap();
        let parsed = p.parse_response(resp).unwrap();
        assert_eq!(
            parsed.tool_calls[0].thought_signature.as_deref(),
            Some("sig")
        );
    }

    #[test]
    fn test_request_has_no_output_cap_unless_one_was_set() {
        let p = make_provider();
        let body = p.build_request_body(&[], None, &[], None);
        assert!(body["generationConfig"].get("maxOutputTokens").is_none());
    }

    #[test]
    fn test_a_blocked_prompt_reports_why() {
        let p = make_provider();
        let resp: GenerateResponse = serde_json::from_value(serde_json::json!({
            "promptFeedback": { "blockReason": "PROHIBITED_CONTENT" }
        }))
        .unwrap();
        let parsed = p.parse_response(resp).unwrap();
        assert_eq!(parsed.usage.stop_reason, "prohibited_content");
    }

    /// Compaction keys its retry on this: a summary cut off by the cap must
    /// report `max_tokens` from `chat`, like the streaming path does.
    #[test]
    fn test_parse_response_reports_max_tokens() {
        let p = make_provider();
        let resp: GenerateResponse = serde_json::from_value(serde_json::json!({
            "candidates": [{
                "content": { "parts": [{ "text": "<s" }] },
                "finishReason": "MAX_TOKENS"
            }]
        }))
        .unwrap();
        let parsed = p.parse_response(resp).unwrap();
        assert_eq!(parsed.usage.stop_reason, "max_tokens");
    }

    #[test]
    fn generate_response_deserializes_finish_reason() {
        let json = r#"{
            "candidates": [{
                "finishReason": "STOP",
                "content": {"parts": [{"text": "done"}]}
            }]
        }"#;
        let resp: GenerateResponse = serde_json::from_str(json).unwrap();
        let candidates = resp.candidates.unwrap();
        assert_eq!(candidates[0].finish_reason.as_deref(), Some("STOP"));
    }

    #[test]
    fn generate_response_with_usage_metadata() {
        let json = r#"{
            "candidates": [],
            "usageMetadata": {
                "promptTokenCount": 42,
                "candidatesTokenCount": 17,
                "cachedContentTokenCount": 5,
                "thoughtsTokenCount": 8
            }
        }"#;
        let resp: GenerateResponse = serde_json::from_str(json).unwrap();
        let usage = resp.usage_metadata.unwrap();
        assert_eq!(usage.prompt_token_count, 42);
        assert_eq!(usage.candidates_token_count, 17);
        assert_eq!(usage.cached_content_token_count, 5);
        assert_eq!(usage.thoughts_token_count, 8);
    }

    #[test]
    fn response_part_empty_text_is_valid() {
        let json = r#"{"text": ""}"#;
        let result = serde_json::from_str::<ResponsePart>(json);
        assert!(result.is_ok());
        let part = result.unwrap();
        assert_eq!(part.text.as_deref(), Some(""));
        assert!(part.function_call.is_none());
        assert!(part.thought.is_none());
    }
}
