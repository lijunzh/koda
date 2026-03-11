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
use tokio::sync::mpsc;

/// Client for OpenAI-compatible APIs.
pub struct OpenAiCompatProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl OpenAiCompatProvider {
    pub fn new(base_url: &str, api_key: Option<String>) -> Self {
        Self {
            client: super::build_http_client(Some(base_url)),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
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
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: Option<String>,
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
    /// Reasoning content from o1/o3/o4-mini models.
    #[serde(default)]
    reasoning_content: Option<String>,
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

// ── Implementation ───────────────────────────────────────────

impl OpenAiCompatProvider {
    /// Build a ChatRequest from messages, tools, model, and optional stream flag.
    fn build_request(
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        model: &str,
        stream: Option<bool>,
        settings: &crate::config::ModelSettings,
    ) -> ChatRequest {
        let api_messages: Vec<ApiMessage> = messages
            .iter()
            .filter(|m| m.role != "phase") // skip internal metadata roles
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
        let request = Self::build_request(messages, tools, &settings.model, None, settings);

        let mut req = self
            .client
            .post(format!("{}/chat/completions", self.base_url));

        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }

        let resp = req
            .json(&request)
            .send()
            .await
            .context("Failed to call LLM API")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("LLM API returned {status}: {body}");
        }

        let chat_resp: ChatResponse = resp.json().await.context("Failed to parse LLM response")?;

        let choice = chat_resp
            .choices
            .into_iter()
            .next()
            .context("LLM returned no choices")?;

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
        let usage = chat_resp
            .usage
            .map_or(TokenUsage::default(), |u| usage_from_response(&u));

        Ok(LlmResponse {
            content,
            tool_calls,
            usage,
        })
    }

    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        settings: &crate::config::ModelSettings,
    ) -> Result<mpsc::Receiver<StreamChunk>> {
        let request = Self::build_request(messages, tools, &settings.model, Some(true), settings);

        let mut req = self
            .client
            .post(format!("{}/chat/completions", self.base_url));

        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }

        let resp = req
            .json(&request)
            .send()
            .await
            .context("Failed to call LLM API (stream)")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("LLM API returned {status}: {body}");
        }

        let (tx, rx) = mpsc::channel(64);

        // Spawn a task to read SSE chunks and send them to the channel
        let mut byte_stream = resp.bytes_stream();
        tokio::spawn(async move {
            use futures_util::StreamExt;

            let mut buffer = String::new();
            let mut tool_calls: Vec<(String, String, String)> = Vec::new(); // (id, name, args)
            let mut final_usage = TokenUsage::default();
            let mut think_filter = super::stream_tag_filter::StreamTagFilter::new();

            while let Some(chunk_result) = byte_stream.next().await {
                let Ok(bytes) = chunk_result else { break };
                buffer.push_str(&String::from_utf8_lossy(&bytes));

                // Process complete SSE lines
                while let Some(line_end) = buffer.find('\n') {
                    let line = buffer[..line_end].trim().to_string();
                    buffer.drain(..=line_end);

                    if line == "data: [DONE]" {
                        // Stream complete — send tool calls if any, then Done
                        if !tool_calls.is_empty() {
                            let tcs = tool_calls
                                .drain(..)
                                .map(|(id, name, args)| ToolCall {
                                    id,
                                    function_name: name,
                                    arguments: args,
                                    thought_signature: None,
                                })
                                .collect();
                            let _ = tx.send(StreamChunk::ToolCalls(tcs)).await;
                        }
                        for filtered in think_filter.flush() {
                            let _ = tx.send(filtered).await;
                        }
                        let _ = tx.send(StreamChunk::Done(final_usage.clone())).await;
                        return;
                    }

                    let Some(json_str) = line.strip_prefix("data: ") else {
                        continue;
                    };

                    let Ok(chunk) = serde_json::from_str::<StreamChatResponse>(json_str) else {
                        continue;
                    };

                    // Capture usage if present
                    if let Some(u) = &chunk.usage {
                        final_usage = usage_from_response(u);
                    }

                    for choice in &chunk.choices {
                        // Capture finish_reason ("stop", "length", "tool_calls", etc.)
                        if let Some(reason) = &choice.finish_reason {
                            // OpenAI uses "length" for max_tokens, normalize
                            final_usage.stop_reason = if reason == "length" {
                                "max_tokens".to_string()
                            } else {
                                reason.clone()
                            };
                        }

                        // Reasoning content (o1/o3/o4-mini)
                        if let Some(reasoning) = &choice.delta.reasoning_content
                            && !reasoning.is_empty()
                        {
                            let _ = tx.send(StreamChunk::ThinkingDelta(reasoning.clone())).await;
                        }

                        // Text delta — run through <think> tag filter
                        if let Some(content) = &choice.delta.content
                            && !content.is_empty()
                        {
                            for filtered in
                                think_filter.process(StreamChunk::TextDelta(content.clone()))
                            {
                                let _ = tx.send(filtered).await;
                            }
                        }

                        // Tool call deltas — accumulate
                        if let Some(tcs) = &choice.delta.tool_calls {
                            for tc in tcs {
                                let idx = tc.index.unwrap_or(0);
                                // Grow the vec if needed
                                while tool_calls.len() <= idx {
                                    tool_calls.push((String::new(), String::new(), String::new()));
                                }
                                if let Some(id) = &tc.id {
                                    tool_calls[idx].0 = id.clone();
                                }
                                if let Some(f) = &tc.function {
                                    if let Some(name) = &f.name {
                                        tool_calls[idx].1.push_str(name);
                                    }
                                    if let Some(args) = &f.arguments {
                                        tool_calls[idx].2.push_str(args);
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Stream ended without [DONE] — send accumulated data
            if !tool_calls.is_empty() {
                let tcs = tool_calls
                    .drain(..)
                    .map(|(id, name, args)| ToolCall {
                        id,
                        function_name: name,
                        arguments: args,
                        thought_signature: None,
                    })
                    .collect();
                let _ = tx.send(StreamChunk::ToolCalls(tcs)).await;
            }
            // Flush any remaining content in the <think> tag filter
            for filtered in think_filter.flush() {
                let _ = tx.send(filtered).await;
            }
            let _ = tx.send(StreamChunk::Done(final_usage)).await;
        });

        Ok(rx)
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
        // Try LM Studio / Ollama extended endpoint first (/api/v0/models)
        // which returns max_context_length per model.
        let base = self.base_url.trim_end_matches("/v1");
        if let Ok(caps) = self.query_local_capabilities(base, model).await
            && caps.context_window.is_some()
        {
            return Ok(caps);
        }
        Ok(super::ModelCapabilities::default())
    }
}

impl OpenAiCompatProvider {
    /// Query LM Studio / Ollama extended model info for context window.
    async fn query_local_capabilities(
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
        let models = body["data"].as_array();

        if let Some(models) = models {
            for m in models {
                let id = m["id"].as_str().unwrap_or_default();
                if id == model {
                    let ctx = m["max_context_length"].as_u64().map(|v| v as usize);
                    return Ok(super::ModelCapabilities {
                        context_window: ctx,
                        max_output_tokens: None,
                    });
                }
            }
        }

        Ok(super::ModelCapabilities::default())
    }
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
        let request =
            OpenAiCompatProvider::build_request(&messages, &[], "gpt-4o", None, &settings);
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
        }];
        let settings = default_settings();
        let request =
            OpenAiCompatProvider::build_request(&messages, &[], "gpt-4o", None, &settings);
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
        }];
        let settings = default_settings();
        let request =
            OpenAiCompatProvider::build_request(&messages, &[], "gpt-4o", None, &settings);
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
        }];
        let settings = default_settings();
        let request =
            OpenAiCompatProvider::build_request(&messages, &[], "gpt-4o", None, &settings);
        assert!(request.messages[0].tool_calls.is_some());
        let tcs = request.messages[0].tool_calls.as_ref().unwrap();
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].function.name, "Read");
    }
}
