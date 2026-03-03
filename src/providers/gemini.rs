//! Native Google Gemini provider.
//!
//! Uses the Gemini REST API directly (not the OpenAI-compat shim).
//! Key differences from OpenAI:
//! - Auth via `?key=` query parameter (not Bearer header)
//! - `contents` with `parts` (not `messages` with `content`)
//! - `system_instruction` as a top-level field
//! - `functionDeclarations` for tool definitions
//! - SSE streaming via `streamGenerateContent?alt=sse`

use super::{
    ChatMessage, LlmProvider, LlmResponse, ModelInfo, StreamChunk, TokenUsage, ToolCall,
    ToolDefinition,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub struct GeminiProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl GeminiProvider {
    pub fn new(api_key: String, base_url: Option<&str>) -> Self {
        Self {
            client: super::build_http_client(),
            base_url: base_url
                .unwrap_or("https://generativelanguage.googleapis.com")
                .trim_end_matches('/')
                .to_string(),
            api_key,
        }
    }
}

// Request types are built via serde_json::json! for flexibility.
// Only response types need Deserialize structs.

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
    parameters: serde_json::Value,
}

// ── Tool config ──────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerateResponse {
    candidates: Option<Vec<Candidate>>,
    usage_metadata: Option<UsageMetadata>,
}

#[derive(Deserialize)]
struct Candidate {
    content: Option<CandidateContent>,
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
}

// ── Implementation ───────────────────────────────────────────

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

        let body = self.build_request_body(
            &contents,
            system_instruction.as_ref(),
            &api_tools,
            Some(settings),
        );

        let resp = self
            .client
            .post(format!(
                "{}/v1beta/models/{}:generateContent?key={}",
                self.base_url, model, self.api_key
            ))
            .json(&body)
            .send()
            .await
            .context("Failed to call Gemini API")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Gemini API returned {status}: {body}");
        }

        let gen_resp: GenerateResponse = resp
            .json()
            .await
            .context("Failed to parse Gemini response")?;

        self.parse_response(gen_resp)
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let resp = self
            .client
            .get(format!(
                "{}/v1beta/models?key={}",
                self.base_url, self.api_key
            ))
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

    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        settings: &crate::config::ModelSettings,
    ) -> Result<tokio::sync::mpsc::Receiver<StreamChunk>> {
        let model = &settings.model;
        let (contents, system_instruction) = self.convert_messages(messages);
        let api_tools = Self::build_tools(tools);

        let body = self.build_request_body(
            &contents,
            system_instruction.as_ref(),
            &api_tools,
            Some(settings),
        );

        let resp = self
            .client
            .post(format!(
                "{}/v1beta/models/{}:streamGenerateContent?alt=sse&key={}",
                self.base_url, model, self.api_key
            ))
            .json(&body)
            .send()
            .await
            .context("Failed to call Gemini API (stream)")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Gemini API returned {status}: {body}");
        }

        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let mut byte_stream = resp.bytes_stream();

        tokio::spawn(async move {
            use futures_util::StreamExt;

            let mut buffer = String::new();
            let mut tool_calls: Vec<ToolCall> = Vec::new();
            let mut final_usage = TokenUsage::default();
            let mut tc_counter = 0u32;

            while let Some(chunk_result) = byte_stream.next().await {
                let Ok(bytes) = chunk_result else { break };
                buffer.push_str(&String::from_utf8_lossy(&bytes));

                while let Some(line_end) = buffer.find('\n') {
                    let line = buffer[..line_end].trim().to_string();
                    buffer.drain(..=line_end);

                    let Some(json_str) = line.strip_prefix("data: ") else {
                        continue;
                    };

                    let Ok(event) = serde_json::from_str::<GenerateResponse>(json_str) else {
                        continue;
                    };

                    // Extract usage from each chunk (last one has final totals)
                    if let Some(usage) = &event.usage_metadata {
                        final_usage.prompt_tokens = usage.prompt_token_count;
                        final_usage.completion_tokens = usage.candidates_token_count;
                        final_usage.cache_read_tokens = usage.cached_content_token_count;
                        final_usage.thinking_tokens = usage.thoughts_token_count;
                        usage.log_cache_stats();
                    }

                    // Extract content parts
                    if let Some(candidates) = &event.candidates {
                        for candidate in candidates {
                            if let Some(content) = &candidate.content
                                && let Some(parts) = &content.parts
                            {
                                for part in parts {
                                    if let Some(text) = &part.text
                                        && !text.is_empty()
                                    {
                                        // Thinking parts go to ThinkingDelta, regular text to TextDelta
                                        if part.thought == Some(true) {
                                            let _ = tx
                                                .send(StreamChunk::ThinkingDelta(text.clone()))
                                                .await;
                                        } else {
                                            let _ =
                                                tx.send(StreamChunk::TextDelta(text.clone())).await;
                                        }
                                    }
                                    if let Some(fc) = &part.function_call {
                                        tc_counter += 1;
                                        tool_calls.push(ToolCall {
                                            id: format!("gemini_tc_{tc_counter}"),
                                            function_name: fc.name.clone(),
                                            arguments: serde_json::to_string(&fc.args)
                                                .unwrap_or_default(),
                                            thought_signature: part.thought_signature.clone(),
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if !tool_calls.is_empty() {
                let _ = tx.send(StreamChunk::ToolCalls(tool_calls)).await;
            }
            let _ = tx.send(StreamChunk::Done(final_usage)).await;
        });

        Ok(rx)
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
                "tool" => "function",
                other => other,
            };

            // Tool result message
            if msg.role == "tool"
                && let Some(content) = &msg.content
            {
                let name = msg.tool_call_id.clone().unwrap_or_default();
                contents.push(serde_json::json!({
                    "role": "function",
                    "parts": [Part::FunctionResponse {
                        name,
                        response: serde_json::json!({ "result": content }),
                    }.to_json()]
                }));
                continue;
            }

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
                for tc in tcs {
                    let args: serde_json::Value =
                        serde_json::from_str(&tc.arguments).unwrap_or_default();
                    let mut fc_part = Part::FunctionCall {
                        name: tc.function_name.clone(),
                        args,
                    }
                    .to_json();
                    // Echo back thought_signature if present (required by Gemini API)
                    if let Some(ref sig) = tc.thought_signature {
                        fc_part["thoughtSignature"] = serde_json::json!(sig);
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
        let max_output = settings.and_then(|s| s.max_tokens).unwrap_or(8192);
        let mut gen_config = serde_json::json!({ "maxOutputTokens": max_output });
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
        if let Some(sys) = system_instruction {
            body["systemInstruction"] = sys.clone();
        }
        if !tools.is_empty() {
            body["tools"] = serde_json::to_value(tools).unwrap_or_default();
        }
        body
    }

    /// Parse Gemini's response into our unified format.
    fn parse_response(&self, resp: GenerateResponse) -> Result<LlmResponse> {
        let mut content_text = String::new();
        let mut tool_calls = Vec::new();
        let mut tc_counter = 0u32;

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
                            tc_counter += 1;
                            tool_calls.push(ToolCall {
                                id: format!("gemini_tc_{tc_counter}"),
                                function_name: fc.name,
                                arguments: serde_json::to_string(&fc.args)?,
                                thought_signature: part.thought_signature,
                            });
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
            usage: TokenUsage {
                prompt_tokens: usage.prompt_token_count,
                completion_tokens: usage.candidates_token_count,
                cache_read_tokens: usage.cached_content_token_count,
                thinking_tokens: usage.thoughts_token_count,
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

    #[test]
    fn test_convert_tool_result_becomes_function() {
        let p = make_provider();
        let messages = vec![ChatMessage {
            role: "tool".into(),
            content: Some("file contents".into()),
            tool_calls: None,
            tool_call_id: Some("Read".into()),
            images: None,
        }];
        let (contents, _) = p.convert_messages(&messages);
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "function");
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

    #[test]
    fn test_parse_text_response() {
        let p = make_provider();
        let resp = GenerateResponse {
            candidates: Some(vec![Candidate {
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
            candidates: Some(vec![Candidate {
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
            candidates: Some(vec![Candidate {
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
}
