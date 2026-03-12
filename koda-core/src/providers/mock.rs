//! Mock LLM provider for testing.
//!
//! Returns scripted responses so tests can exercise the full inference loop
//! without a real LLM. Responses are consumed in FIFO order.

use super::{
    ChatMessage, LlmProvider, LlmResponse, ModelInfo, StreamChunk, TokenUsage, ToolCall,
    ToolDefinition,
};
use crate::config::ModelSettings;

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc;

static MOCK_CALL_COUNTER: AtomicU64 = AtomicU64::new(1);

/// A scripted response for the mock provider.
#[derive(Debug, Clone)]
pub enum MockResponse {
    /// Stream text content back as the LLM response.
    Text(String),
    /// Return one or more tool calls.
    ToolCalls(Vec<ToolCall>),
    /// Simulate a provider error.
    Error(String),
    /// Simulate a rate limit (429) error.
    RateLimit,
    /// Simulate a context overflow error.
    ContextOverflow,
}

impl MockResponse {
    /// Convenience: create a single tool call response.
    pub fn tool_call(name: &str, args: serde_json::Value) -> Self {
        let id = format!(
            "mock_call_{}",
            MOCK_CALL_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        MockResponse::ToolCalls(vec![ToolCall {
            id,
            function_name: name.to_string(),
            arguments: serde_json::to_string(&args).unwrap(),
            thought_signature: None,
        }])
    }
}

/// A mock LLM provider that returns scripted responses.
///
/// Responses are consumed in FIFO order. Panics if exhausted.
pub struct MockProvider {
    responses: Mutex<Vec<MockResponse>>,
}

impl MockProvider {
    /// Create a mock provider that returns the given responses in order.
    pub fn new(responses: Vec<MockResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
        }
    }

    /// Create from `KODA_MOCK_RESPONSES` env var (JSON array).
    ///
    /// Format: `[{"text":"hello"}, {"tool":"Read","args":{"path":"f.txt"}}, {"error":"boom"}]`
    pub fn from_env() -> Self {
        let json = std::env::var("KODA_MOCK_RESPONSES").unwrap_or_else(|_| "[]".into());
        let raw: Vec<serde_json::Value> =
            serde_json::from_str(&json).expect("KODA_MOCK_RESPONSES must be a JSON array");
        let responses = raw
            .into_iter()
            .map(|v| {
                if let Some(text) = v.get("text").and_then(|t| t.as_str()) {
                    MockResponse::Text(text.to_string())
                } else if let Some(tool) = v.get("tool").and_then(|t| t.as_str()) {
                    let args = v.get("args").cloned().unwrap_or(serde_json::json!({}));
                    MockResponse::tool_call(tool, args)
                } else if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
                    MockResponse::Error(err.to_string())
                } else if v.get("rate_limit").is_some() {
                    MockResponse::RateLimit
                } else if v.get("context_overflow").is_some() {
                    MockResponse::ContextOverflow
                } else {
                    MockResponse::Text(v.to_string())
                }
            })
            .collect();
        Self::new(responses)
    }

    fn next_response(&self) -> MockResponse {
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            // Graceful fallback: return empty text (model is "done").
            return MockResponse::Text(String::new());
        }
        responses.remove(0)
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    async fn chat(
        &self,
        _messages: &[ChatMessage],
        _tools: &[ToolDefinition],
        _settings: &ModelSettings,
    ) -> Result<LlmResponse> {
        match self.next_response() {
            MockResponse::Text(text) => Ok(LlmResponse {
                content: Some(text),
                tool_calls: vec![],
                usage: TokenUsage::default(),
            }),
            MockResponse::ToolCalls(calls) => Ok(LlmResponse {
                content: None,
                tool_calls: calls,
                usage: TokenUsage::default(),
            }),
            MockResponse::Error(msg) => Err(anyhow::anyhow!(msg)),
            MockResponse::RateLimit => {
                Err(anyhow::anyhow!("LLM API returned 429: Too Many Requests"))
            }
            MockResponse::ContextOverflow => Err(anyhow::anyhow!(
                "LLM API returned 400: prompt is too long, maximum context length exceeded"
            )),
        }
    }

    async fn chat_stream(
        &self,
        _messages: &[ChatMessage],
        _tools: &[ToolDefinition],
        _settings: &ModelSettings,
    ) -> Result<mpsc::Receiver<StreamChunk>> {
        let response = self.next_response();

        // Error responses fail at the call site, not inside the stream.
        match &response {
            MockResponse::Error(msg) => return Err(anyhow::anyhow!("{msg}")),
            MockResponse::RateLimit => {
                return Err(anyhow::anyhow!("LLM API returned 429: Too Many Requests"));
            }
            MockResponse::ContextOverflow => {
                return Err(anyhow::anyhow!(
                    "LLM API returned 400: prompt is too long, maximum context length exceeded"
                ));
            }
            _ => {}
        }

        let (tx, rx) = mpsc::channel(32);

        tokio::spawn(async move {
            match response {
                MockResponse::Text(text) => {
                    // Stream in small chunks to simulate real streaming.
                    for chunk in text.as_bytes().chunks(20) {
                        let s = String::from_utf8_lossy(chunk).to_string();
                        let _ = tx.send(StreamChunk::TextDelta(s)).await;
                    }
                    let _ = tx
                        .send(StreamChunk::Done(TokenUsage {
                            prompt_tokens: 10,
                            completion_tokens: text.len() as i64 / 4,
                            ..Default::default()
                        }))
                        .await;
                }
                MockResponse::ToolCalls(calls) => {
                    let _ = tx.send(StreamChunk::ToolCalls(calls)).await;
                    let _ = tx
                        .send(StreamChunk::Done(TokenUsage {
                            prompt_tokens: 10,
                            completion_tokens: 5,
                            ..Default::default()
                        }))
                        .await;
                }
                MockResponse::Error(_)
                | MockResponse::RateLimit
                | MockResponse::ContextOverflow => unreachable!(),
            }
        });

        Ok(rx)
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        Ok(vec![ModelInfo {
            id: "mock-model".to_string(),
            owned_by: Some("test".to_string()),
        }])
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_text_response() {
        let provider = MockProvider::new(vec![MockResponse::Text("hello".into())]);
        let rx = provider
            .chat_stream(
                &[],
                &[],
                &ModelSettings::defaults_for("mock", &crate::config::ProviderType::LMStudio),
            )
            .await
            .unwrap();

        let chunks: Vec<_> = collect_chunks(rx).await;
        assert!(
            chunks
                .iter()
                .any(|c| matches!(c, StreamChunk::TextDelta(_)))
        );
        assert!(chunks.iter().any(|c| matches!(c, StreamChunk::Done(_))));
    }

    #[tokio::test]
    async fn test_tool_call_response() {
        let provider = MockProvider::new(vec![MockResponse::tool_call(
            "Bash",
            serde_json::json!({"command": "echo hi"}),
        )]);
        let rx = provider
            .chat_stream(
                &[],
                &[],
                &ModelSettings::defaults_for("mock", &crate::config::ProviderType::LMStudio),
            )
            .await
            .unwrap();

        let chunks: Vec<_> = collect_chunks(rx).await;
        assert!(
            chunks
                .iter()
                .any(|c| matches!(c, StreamChunk::ToolCalls(_)))
        );
    }

    #[tokio::test]
    async fn test_error_response() {
        let provider = MockProvider::new(vec![MockResponse::Error("boom".into())]);
        let result = provider
            .chat_stream(
                &[],
                &[],
                &ModelSettings::defaults_for("mock", &crate::config::ProviderType::LMStudio),
            )
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("boom"));
    }

    async fn collect_chunks(mut rx: mpsc::Receiver<StreamChunk>) -> Vec<StreamChunk> {
        let mut chunks = Vec::new();
        while let Some(chunk) = rx.recv().await {
            chunks.push(chunk);
        }
        chunks
    }
}
