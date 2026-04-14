//! IPC-backed LLM provider for sandboxed headless workers.
//!
//! When a worker process runs with `KODA_SUPERVISOR_SOCKET` set and the
//! network is isolated (seccomp/Seatbelt), this provider proxies every
//! LLM API call through the supervisor's Unix socket rather than opening
//! TCP connections directly.
//!
//! ## Type bridge
//!
//! `koda-ipc` cannot depend on `koda-core` (circular deps), so IPC types
//! mirror koda-core types 1:1. Conversions are done field-by-field here.
//!
//! ## Streaming
//!
//! The supervisor calls the real provider synchronously and returns the
//! complete `LlmResponse`. This provider wraps that in a fake stream that
//! emits all tokens in one `TextDelta` burst followed by `Done`. For
//! headless/background tasks this is indistinguishable from real streaming
//! from the session's perspective.

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::sync::mpsc;

use koda_ipc::llm::{
    IpcChatMessage, IpcImageData, IpcLlmRequest, IpcModelSettings, IpcToolCall, IpcToolDefinition,
};

use crate::config::ModelSettings;
use crate::providers::{
    ChatMessage, ImageData, LlmProvider, LlmResponse, ModelCapabilities, ModelInfo, StreamChunk,
    TokenUsage, ToolCall, ToolDefinition, stream_collector::SseCollector,
};

/// LLM provider that delegates every call to the supervisor over IPC.
pub struct IpcLlmProvider {
    socket_path: String,
    model: String,
}

impl IpcLlmProvider {
    /// Create an IPC provider that routes all LLM calls to the supervisor
    /// at the given Unix socket path.
    pub fn new(socket_path: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            socket_path: socket_path.into(),
            model: model.into(),
        }
    }
}

#[async_trait]
impl LlmProvider for IpcLlmProvider {
    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        settings: &ModelSettings,
    ) -> Result<LlmResponse> {
        let req = IpcLlmRequest {
            messages: messages.iter().map(to_ipc_message).collect(),
            tools: tools.iter().map(to_ipc_tool).collect(),
            settings: to_ipc_settings(settings),
        };
        let resp = koda_ipc::client::llm_chat(&self.socket_path, req)
            .await
            .context("IPC llm_chat")?;
        Ok(LlmResponse {
            content: resp.content,
            tool_calls: resp
                .tool_calls
                .into_iter()
                .map(from_ipc_tool_call)
                .collect(),
            usage: TokenUsage {
                prompt_tokens: resp.usage.prompt_tokens,
                completion_tokens: resp.usage.completion_tokens,
                cache_read_tokens: resp.usage.cache_read_tokens,
                cache_creation_tokens: resp.usage.cache_creation_tokens,
                thinking_tokens: resp.usage.thinking_tokens,
                stop_reason: resp.usage.stop_reason,
            },
        })
    }

    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        settings: &ModelSettings,
    ) -> Result<SseCollector> {
        // Collect the full response synchronously then emit it as a fake stream.
        // For headless tasks this is fine — no one is watching token-by-token.
        let response = self.chat(messages, tools, settings).await?;

        let (tx, rx) = mpsc::channel(32);
        let handle = tokio::spawn(async move {
            if let Some(text) = response.content {
                let _ = tx.send(StreamChunk::TextDelta(text)).await;
            }
            if !response.tool_calls.is_empty() {
                let _ = tx.send(StreamChunk::ToolCalls(response.tool_calls)).await;
            }
            let _ = tx.send(StreamChunk::Done(response.usage)).await;
        });

        Ok(SseCollector { rx, handle })
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        // Workers don't need to enumerate models.
        Ok(vec![ModelInfo {
            id: self.model.clone(),
            owned_by: None,
        }])
    }

    fn provider_name(&self) -> &str {
        "ipc-supervisor"
    }

    async fn model_capabilities(&self, _model: &str) -> Result<ModelCapabilities> {
        Ok(ModelCapabilities::default())
    }
}

// ── Conversion helpers — koda-core → koda-ipc ────────────────────────────────

fn to_ipc_message(m: &ChatMessage) -> IpcChatMessage {
    IpcChatMessage {
        role: m.role.clone(),
        content: m.content.clone(),
        tool_calls: m
            .tool_calls
            .as_ref()
            .map(|tc| tc.iter().map(to_ipc_tool_call).collect()),
        tool_call_id: m.tool_call_id.clone(),
        images: m
            .images
            .as_ref()
            .map(|imgs| imgs.iter().map(to_ipc_image).collect()),
    }
}

fn to_ipc_tool_call(tc: &ToolCall) -> IpcToolCall {
    IpcToolCall {
        id: tc.id.clone(),
        function_name: tc.function_name.clone(),
        arguments: tc.arguments.clone(),
        thought_signature: tc.thought_signature.clone(),
    }
}

fn to_ipc_image(img: &ImageData) -> IpcImageData {
    IpcImageData {
        media_type: img.media_type.clone(),
        base64: img.base64.clone(),
    }
}

fn to_ipc_tool(t: &ToolDefinition) -> IpcToolDefinition {
    IpcToolDefinition {
        name: t.name.clone(),
        description: t.description.clone(),
        parameters: t.parameters.clone(),
    }
}

fn to_ipc_settings(s: &ModelSettings) -> IpcModelSettings {
    IpcModelSettings {
        model: s.model.clone(),
        max_tokens: s.max_tokens,
        temperature: s.temperature,
        thinking_budget: s.thinking_budget,
        reasoning_effort: s.reasoning_effort.clone(),
        max_context_tokens: s.max_context_tokens,
    }
}

fn from_ipc_tool_call(tc: IpcToolCall) -> ToolCall {
    ToolCall {
        id: tc.id,
        function_name: tc.function_name,
        arguments: tc.arguments,
        thought_signature: tc.thought_signature,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderType;

    fn sample_settings() -> ModelSettings {
        ModelSettings::defaults_for("claude-opus-4-5", &ProviderType::Anthropic)
    }

    #[test]
    fn round_trip_settings() {
        let s = sample_settings();
        let ipc = to_ipc_settings(&s);
        assert_eq!(ipc.model, s.model);
        assert_eq!(ipc.max_tokens, s.max_tokens);
        assert_eq!(ipc.max_context_tokens, s.max_context_tokens);
    }

    #[test]
    fn round_trip_tool_call() {
        let tc = ToolCall {
            id: "call-1".into(),
            function_name: "Read".into(),
            arguments: r#"{"path":"src/main.rs"}"#.into(),
            thought_signature: None,
        };
        let ipc = to_ipc_tool_call(&tc);
        let back = from_ipc_tool_call(ipc);
        assert_eq!(back.id, tc.id);
        assert_eq!(back.function_name, tc.function_name);
        assert_eq!(back.arguments, tc.arguments);
    }

    #[test]
    fn round_trip_message_with_images() {
        let msg = ChatMessage {
            role: "user".into(),
            content: Some("look at this".into()),
            tool_calls: None,
            tool_call_id: None,
            images: Some(vec![ImageData {
                media_type: "image/png".into(),
                base64: "abc123".into(),
            }]),
        };
        let ipc = to_ipc_message(&msg);
        assert_eq!(ipc.role, "user");
        let imgs = ipc.images.unwrap();
        assert_eq!(imgs[0].media_type, "image/png");
        assert_eq!(imgs[0].base64, "abc123");
    }
}
