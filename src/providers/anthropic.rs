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

/// Known Claude models (Anthropic doesn't have a /models endpoint).
const CLAUDE_MODELS: &[&str] = &[
    "claude-sonnet-4-20250514",
    "claude-3-5-haiku-20241022",
    "claude-3-5-sonnet-20241022",
    "claude-3-opus-20240229",
];

pub struct AnthropicProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl AnthropicProvider {
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
    system: Option<String>,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<AnthropicTool>,
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
}

#[derive(Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

// ── Response types ───────────────────────────────────────────

#[derive(Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
    usage: AnthropicUsage,
}

#[derive(Deserialize)]
struct AnthropicUsage {
    input_tokens: i64,
    output_tokens: i64,
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
        model: &str,
    ) -> Result<LlmResponse> {
        // Extract system prompt (Anthropic puts it at the top level)
        let system = messages
            .iter()
            .find(|m| m.role == "system")
            .and_then(|m| m.content.clone());

        // Convert messages (skip system, convert tool results)
        let api_messages = self.convert_messages(messages);

        let api_tools: Vec<AnthropicTool> = tools
            .iter()
            .map(|t| AnthropicTool {
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema: t.parameters.clone(),
            })
            .collect();

        let request = MessagesRequest {
            model: model.to_string(),
            max_tokens: 8192,
            system,
            messages: api_messages,
            tools: api_tools,
        };

        let resp = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_API_VERSION)
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

        Ok(LlmResponse {
            content,
            tool_calls,
            usage: TokenUsage {
                prompt_tokens: msg_resp.usage.input_tokens,
                completion_tokens: msg_resp.usage.output_tokens,
            },
        })
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        // Verify the API key by making a minimal request.
        // Use a 1-token chat to confirm auth works.
        let verify_req = MessagesRequest {
            model: "claude-3-5-haiku-20241022".to_string(),
            max_tokens: 1,
            system: None,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: AnthropicContent::Text("hi".to_string()),
            }],
            tools: Vec::new(),
        };

        let resp = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_API_VERSION)
            .json(&verify_req)
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

        // Key is valid, return known models
        Ok(CLAUDE_MODELS
            .iter()
            .map(|id| ModelInfo {
                id: id.to_string(),
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
        model: &str,
    ) -> Result<tokio::sync::mpsc::Receiver<StreamChunk>> {
        let system = messages
            .iter()
            .find(|m| m.role == "system")
            .and_then(|m| m.content.clone());

        let api_messages = self.convert_messages(messages);

        let api_tools: Vec<AnthropicTool> = tools
            .iter()
            .map(|t| AnthropicTool {
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema: t.parameters.clone(),
            })
            .collect();

        // Build request body with stream: true
        let mut body = serde_json::json!({
            "model": model,
            "max_tokens": 8192,
            "stream": true,
            "messages": serde_json::to_value(&api_messages)?,
        });
        if let Some(sys) = system {
            body["system"] = serde_json::Value::String(sys);
        }
        if !api_tools.is_empty() {
            body["tools"] = serde_json::to_value(&api_tools)?;
        }

        let resp = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_API_VERSION)
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
                                // Text delta
                                if let Some(text) = delta.text
                                    && !text.is_empty()
                                {
                                    let _ = tx.send(StreamChunk::TextDelta(text)).await;
                                }
                                // Tool use input JSON delta
                                if let Some(partial) = delta.partial_json {
                                    let idx = event.index.unwrap_or(0);
                                    if idx < tool_calls.len() {
                                        tool_calls[idx].2.push_str(&partial);
                                    }
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
            ChatMessage {
                role: "system".into(),
                content: Some("system prompt".into()),
                tool_calls: None,
                tool_call_id: None,
            },
            ChatMessage {
                role: "user".into(),
                content: Some("hello".into()),
                tool_calls: None,
                tool_call_id: None,
            },
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
            }]),
            tool_call_id: None,
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
        let messages = vec![ChatMessage {
            role: "user".into(),
            content: Some("explain this code".into()),
            tool_calls: None,
            tool_call_id: None,
        }];
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
            }]),
            tool_call_id: None,
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
            ChatMessage {
                role: "system".into(),
                content: Some("sys".into()),
                tool_calls: None,
                tool_call_id: None,
            },
            ChatMessage {
                role: "user".into(),
                content: Some("hi".into()),
                tool_calls: None,
                tool_call_id: None,
            },
            ChatMessage {
                role: "assistant".into(),
                content: Some("hello!".into()),
                tool_calls: None,
                tool_call_id: None,
            },
            ChatMessage {
                role: "user".into(),
                content: Some("bye".into()),
                tool_calls: None,
                tool_call_id: None,
            },
        ];
        let converted = p.convert_messages(&messages);
        // System is skipped, so 3 messages
        assert_eq!(converted.len(), 3);
        assert_eq!(converted[0].role, "user");
        assert_eq!(converted[1].role, "assistant");
        assert_eq!(converted[2].role, "user");
    }
}
