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
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

const ANTHROPIC_API_VERSION: &str = "2023-06-01";
const ANTHROPIC_BETA_FEATURES: &str = "prompt-caching-2024-07-31";

pub struct AnthropicProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
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
    pub fn new(api_key: String, base_url: Option<&str>) -> Self {
        Self {
            client: super::build_http_client(),
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

// ── Models list types ───────────────────────────────────────

#[derive(Deserialize)]
struct ModelsListResponse {
    data: Vec<ModelsListEntry>,
}

#[derive(Deserialize)]
struct ModelsListEntry {
    id: String,
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
}

#[derive(Deserialize)]
struct StreamMessageInfo {
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

// ── Implementation ───────────────────────────────────────────

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        settings: &crate::config::ModelSettings,
    ) -> Result<LlmResponse> {
        let model = &settings.model;
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
            model: model.to_string(),
            max_tokens: settings.max_tokens.unwrap_or(16384),
            system,
            messages: api_messages,
            tools: api_tools,
        };

        let resp = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_API_VERSION)
            .header("anthropic-beta", ANTHROPIC_BETA_FEATURES)
            .json(&request)
            .send()
            .await
            .context("Failed to call Anthropic API")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Anthropic API returned {status}: {body}");
        }

        let msg_resp: MessagesResponse = resp
            .json()
            .await
            .context("Failed to parse Anthropic response")?;

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

        let list_resp: ModelsListResponse = resp
            .json()
            .await
            .context("Failed to parse Anthropic models response")?;

        Ok(list_resp
            .data
            .into_iter()
            .map(|m| ModelInfo {
                id: m.id,
                owned_by: Some("anthropic".to_string()),
            })
            .collect())
    }

    fn provider_name(&self) -> &str {
        "anthropic"
    }

    /// Real SSE streaming via Anthropic's Messages API.
    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        settings: &crate::config::ModelSettings,
    ) -> Result<tokio::sync::mpsc::Receiver<StreamChunk>> {
        let model = &settings.model;
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
            "model": model,
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

        let resp = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_API_VERSION)
            .header("anthropic-beta", ANTHROPIC_BETA_FEATURES)
            .json(&body)
            .send()
            .await
            .context("Failed to call Anthropic API (stream)")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Anthropic API returned {status}: {body}");
        }

        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let mut byte_stream = resp.bytes_stream();

        tokio::spawn(async move {
            use futures_util::StreamExt;

            let mut buffer = String::new();
            let mut tool_calls: Vec<(String, String, String)> = Vec::new(); // (id, name, args_json)
            let mut final_usage = TokenUsage::default();
            let mut thinking_indices: std::collections::HashSet<usize> =
                std::collections::HashSet::new();

            while let Some(chunk_result) = byte_stream.next().await {
                let Ok(bytes) = chunk_result else { break };
                buffer.push_str(&String::from_utf8_lossy(&bytes));

                while let Some(line_end) = buffer.find('\n') {
                    let line = buffer[..line_end].trim().to_string();
                    buffer.drain(..=line_end);

                    // Skip empty lines and event type lines
                    let Some(json_str) = line.strip_prefix("data: ") else {
                        continue;
                    };

                    // End of stream
                    if json_str.trim() == "[DONE]" {
                        continue;
                    }

                    let Ok(event) = serde_json::from_str::<StreamEvent>(json_str) else {
                        continue;
                    };

                    match event.event_type.as_str() {
                        "content_block_start" => {
                            // Detect thinking blocks by checking the raw JSON
                            if let Some(idx) = event.index
                                && let Ok(raw) = serde_json::from_str::<serde_json::Value>(json_str)
                                && let Some(cb) = raw.get("content_block")
                                && cb.get("type").and_then(|t| t.as_str()) == Some("thinking")
                            {
                                thinking_indices.insert(idx);
                            }
                            // A new content block is starting — could be text or tool_use
                            if let Some(ContentBlock::ToolUse { id, name, .. }) =
                                event.content_block
                            {
                                let idx = event.index.unwrap_or(tool_calls.len());
                                while tool_calls.len() <= idx {
                                    tool_calls.push((String::new(), String::new(), String::new()));
                                }
                                tool_calls[idx].0 = id;
                                tool_calls[idx].1 = name;
                            }
                        }
                        "content_block_delta" => {
                            if let Some(delta) = event.delta {
                                let idx = event.index.unwrap_or(0);
                                let is_thinking = thinking_indices.contains(&idx);

                                // Thinking delta (Anthropic sends "thinking" field)
                                if is_thinking {
                                    if let Some(text) = delta.thinking.or(delta.text)
                                        && !text.is_empty()
                                    {
                                        let _ = tx.send(StreamChunk::ThinkingDelta(text)).await;
                                    }
                                } else {
                                    // Text delta
                                    if let Some(text) = delta.text
                                        && !text.is_empty()
                                    {
                                        let _ = tx.send(StreamChunk::TextDelta(text)).await;
                                    }
                                }
                                // Tool use input JSON delta
                                if let Some(partial) = delta.partial_json
                                    && idx < tool_calls.len()
                                {
                                    tool_calls[idx].2.push_str(&partial);
                                }
                            }
                        }
                        "message_delta" => {
                            // Final usage info
                            if let Some(u) = event.usage {
                                final_usage.completion_tokens = u.output_tokens;
                            }
                        }
                        "message_start" => {
                            // Capture input token usage
                            if let Some(msg) = event.message
                                && let Some(u) = msg.usage
                            {
                                final_usage.prompt_tokens = u.input_tokens;
                                final_usage.cache_read_tokens = u.cache_read_input_tokens;
                                final_usage.cache_creation_tokens = u.cache_creation_input_tokens;
                            }
                        }
                        "message_stop" => {
                            // Stream complete
                        }
                        _ => {} // content_block_stop, ping, etc.
                    }
                }
            }

            // Send accumulated tool calls if any
            if !tool_calls.is_empty() {
                let tcs = tool_calls
                    .drain(..)
                    .filter(|(id, _, _)| !id.is_empty())
                    .map(|(id, name, args)| ToolCall {
                        id,
                        function_name: name,
                        arguments: args,
                        thought_signature: None,
                    })
                    .collect();
                let _ = tx.send(StreamChunk::ToolCalls(tcs)).await;
            }
            let _ = tx.send(StreamChunk::Done(final_usage)).await;
        });

        Ok(rx)
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
                    let input: serde_json::Value =
                        serde_json::from_str(&tc.arguments).unwrap_or_default();
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
}
