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
            client: super::build_http_client(),
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
    #[allow(dead_code)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct StreamDelta {
    content: Option<String>,
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
                        m.content.as_ref().map(|c| serde_json::Value::String(c.clone()))
                    }
                } else {
                    m.content.as_ref().map(|c| serde_json::Value::String(c.clone()))
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
        }
    }
}

#[async_trait]
impl LlmProvider for OpenAiCompatProvider {
    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        model: &str,
    ) -> Result<LlmResponse> {
        let request = Self::build_request(messages, tools, model, None);

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
            })
            .collect();

        // LM Studio sends empty string content with tool calls; normalize to None
        let content = choice.message.content.filter(|c| !c.is_empty());
        let usage = chat_resp
            .usage
            .map_or(TokenUsage::default(), |u| TokenUsage {
                prompt_tokens: u.prompt_tokens.unwrap_or(0),
                completion_tokens: u.completion_tokens.unwrap_or(0),
            });

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
        model: &str,
    ) -> Result<mpsc::Receiver<StreamChunk>> {
        let request = Self::build_request(messages, tools, model, Some(true));

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
                                })
                                .collect();
                            let _ = tx.send(StreamChunk::ToolCalls(tcs)).await;
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
                        final_usage = TokenUsage {
                            prompt_tokens: u.prompt_tokens.unwrap_or(0),
                            completion_tokens: u.completion_tokens.unwrap_or(0),
                        };
                    }

                    for choice in &chunk.choices {
                        // Text delta
                        if let Some(content) = &choice.delta.content
                            && !content.is_empty()
                        {
                            let _ = tx.send(StreamChunk::TextDelta(content.clone())).await;
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
                    })
                    .collect();
                let _ = tx.send(StreamChunk::ToolCalls(tcs)).await;
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
