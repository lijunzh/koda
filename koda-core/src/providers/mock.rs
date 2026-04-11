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
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::mpsc;

static MOCK_CALL_COUNTER: AtomicU64 = AtomicU64::new(1);

/// A scripted response for the mock provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MockResponse {
    /// Stream text content back as the LLM response.
    Text(String),
    /// Stream text and signal `stop_reason = "max_tokens"` (truncated response).
    TextMaxTokens(String),
    /// Return one or more tool calls.
    ToolCalls(Vec<ToolCall>),
    /// Return tool calls via per-block `ToolCallReady` events (Anthropic pattern).
    ///
    /// Each tool call is emitted as a `ToolCallReady` chunk — the same path that
    /// enables eager execution during streaming.
    ToolCallsEager(Vec<ToolCall>),
    /// Simulate a provider error.
    Error(String),
    /// Simulate a rate limit (429) error.
    RateLimit,
    /// Simulate a context overflow error.
    ContextOverflow,
    /// Simulate a network drop mid-stream (partial text then disconnect).
    NetworkError {
        /// Text streamed before the connection drops.
        partial_text: String,
        /// Error message for the network failure.
        error: String,
    },
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

/// Returns the shared global Arc that all `from_env()` providers write to.
///
/// Tests call `MockProvider::take_env_calls()` (feature = "test-support") to
/// inspect what messages the sub-agent provider received without needing a
/// direct reference to the provider instance.
fn global_env_calls() -> Arc<Mutex<Vec<Vec<ChatMessage>>>> {
    static CALLS: OnceLock<Arc<Mutex<Vec<Vec<ChatMessage>>>>> = OnceLock::new();
    CALLS
        .get_or_init(|| Arc::new(Mutex::new(Vec::new())))
        .clone()
}

/// A mock LLM provider that returns scripted responses.
///
/// Responses are consumed in FIFO order. Graceful fallback to empty text when
/// the queue is exhausted.
///
/// Every `chat()` / `chat_stream()` call records the received messages in
/// `recorded_calls`. Providers created via `from_env()` additionally write to
/// a shared global, letting tests inspect sub-agent calls without holding a
/// direct reference to the provider.
pub struct MockProvider {
    responses: Mutex<Vec<MockResponse>>,
    /// Records every messages slice passed to `chat()` / `chat_stream()`.
    recorded_calls: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
}

impl MockProvider {
    /// Create a mock provider that returns the given responses in order.
    pub fn new(responses: Vec<MockResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
            recorded_calls: Arc::new(Mutex::new(Vec::new())),
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
        Self {
            responses: Mutex::new(responses),
            recorded_calls: global_env_calls(),
        }
    }

    fn next_response(&self) -> MockResponse {
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            return MockResponse::Text(String::new());
        }
        responses.remove(0)
    }

    /// All messages passed to `chat()` / `chat_stream()` on this instance.
    pub fn recorded_calls(&self) -> Vec<Vec<ChatMessage>> {
        self.recorded_calls.lock().unwrap().clone()
    }

    /// All messages received by any `from_env()` provider since the last
    /// `clear_env_calls()`. Requires the `test-support` feature.
    ///
    /// Because `execute_sub_agent` creates providers internally via
    /// `MockProvider::from_env()`, this is the only way for tests to inspect
    /// what messages the sub-agent's provider actually received.
    #[cfg(feature = "test-support")]
    pub fn take_env_calls() -> Vec<Vec<ChatMessage>> {
        std::mem::take(&mut *global_env_calls().lock().unwrap())
    }

    /// Clear the global `from_env()` call recorder.
    #[cfg(feature = "test-support")]
    pub fn clear_env_calls() {
        global_env_calls().lock().unwrap().clear();
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    async fn chat(
        &self,
        messages: &[ChatMessage],
        _tools: &[ToolDefinition],
        _settings: &ModelSettings,
    ) -> Result<LlmResponse> {
        self.recorded_calls.lock().unwrap().push(messages.to_vec());
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
            MockResponse::TextMaxTokens(text) => Ok(LlmResponse {
                content: Some(text),
                tool_calls: vec![],
                usage: TokenUsage {
                    stop_reason: "max_tokens".into(),
                    ..Default::default()
                },
            }),
            MockResponse::ToolCallsEager(calls) => Ok(LlmResponse {
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
            MockResponse::NetworkError { .. } => Err(anyhow::anyhow!("network error")),
        }
    }

    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        _tools: &[ToolDefinition],
        _settings: &ModelSettings,
    ) -> Result<super::stream_collector::SseCollector> {
        self.recorded_calls.lock().unwrap().push(messages.to_vec());
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

        let handle = tokio::spawn(async move {
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
                MockResponse::TextMaxTokens(text) => {
                    for chunk in text.as_bytes().chunks(20) {
                        let s = String::from_utf8_lossy(chunk).to_string();
                        let _ = tx.send(StreamChunk::TextDelta(s)).await;
                    }
                    let _ = tx
                        .send(StreamChunk::Done(TokenUsage {
                            prompt_tokens: 10,
                            completion_tokens: text.len() as i64 / 4,
                            stop_reason: "max_tokens".into(),
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
                MockResponse::ToolCallsEager(calls) => {
                    // Emit each tool call individually (Anthropic pattern).
                    for tc in &calls {
                        let _ = tx.send(StreamChunk::ToolCallReady(tc.clone())).await;
                    }
                    // Empty batch — all already emitted via ToolCallReady.
                    let _ = tx.send(StreamChunk::ToolCalls(vec![])).await;
                    let _ = tx
                        .send(StreamChunk::Done(TokenUsage {
                            prompt_tokens: 10,
                            completion_tokens: 5,
                            ..Default::default()
                        }))
                        .await;
                }
                MockResponse::NetworkError {
                    partial_text,
                    error,
                } => {
                    // Send partial text, then drop with a network error.
                    if !partial_text.is_empty() {
                        let _ = tx.send(StreamChunk::TextDelta(partial_text)).await;
                    }
                    let _ = tx.send(StreamChunk::NetworkError(error)).await;
                }
                MockResponse::Error(_)
                | MockResponse::RateLimit
                | MockResponse::ContextOverflow => unreachable!(),
            }
        });

        Ok(super::stream_collector::SseCollector { rx, handle })
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

    /// Serialize env-var mutations across parallel tests.
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[tokio::test]
    async fn test_text_response() {
        let provider = MockProvider::new(vec![MockResponse::Text("hello".into())]);
        let collector = provider
            .chat_stream(
                &[],
                &[],
                &ModelSettings::defaults_for("mock", &crate::config::ProviderType::LMStudio),
            )
            .await
            .unwrap();

        let chunks: Vec<_> = collect_chunks(collector).await;
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
        let collector = provider
            .chat_stream(
                &[],
                &[],
                &ModelSettings::defaults_for("mock", &crate::config::ProviderType::LMStudio),
            )
            .await
            .unwrap();

        let chunks: Vec<_> = collect_chunks(collector).await;
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

    async fn collect_chunks(
        collector: crate::providers::stream_collector::SseCollector,
    ) -> Vec<StreamChunk> {
        let mut rx = collector.rx;
        let mut chunks = Vec::new();
        while let Some(chunk) = rx.recv().await {
            chunks.push(chunk);
        }
        chunks
    }

    // ── MockResponse::tool_call builder ─────────────────────────────────

    #[test]
    fn test_tool_call_builder() {
        let tc = MockResponse::tool_call("Read", serde_json::json!({"file_path": "foo.rs"}));
        match tc {
            MockResponse::ToolCalls(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].function_name, "Read");
                let args: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
                assert_eq!(args["file_path"], "foo.rs");
            }
            other => panic!("expected ToolCalls, got {other:?}"),
        }
    }

    // ── from_env ─────────────────────────────────────────────────────

    #[test]
    fn test_from_env_no_var_gives_empty_provider() {
        let _guard = ENV_MUTEX.lock().unwrap();
        // SAFETY: serialized via ENV_MUTEX
        unsafe { std::env::remove_var("KODA_MOCK_RESPONSES") };
        let provider = MockProvider::from_env();
        let next = provider.next_response();
        assert!(matches!(next, MockResponse::Text(t) if t.is_empty()));
    }

    #[test]
    fn test_from_env_with_text_response() {
        let _guard = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var("KODA_MOCK_RESPONSES", r#"[{"text": "hello from env"}]"#);
        }
        let provider = MockProvider::from_env();
        let next = provider.next_response();
        assert!(matches!(next, MockResponse::Text(t) if t == "hello from env"));
        unsafe { std::env::remove_var("KODA_MOCK_RESPONSES") };
    }

    #[test]
    fn test_from_env_with_tool_call() {
        let _guard = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var(
                "KODA_MOCK_RESPONSES",
                r#"[{"tool": "Bash", "args": {"command": "ls"}}]"#,
            );
        }
        let provider = MockProvider::from_env();
        let next = provider.next_response();
        assert!(matches!(next, MockResponse::ToolCalls(calls) if calls[0].function_name == "Bash"));
        unsafe { std::env::remove_var("KODA_MOCK_RESPONSES") };
    }

    #[test]
    fn test_from_env_with_error() {
        let _guard = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var("KODA_MOCK_RESPONSES", r#"[{"error": "boom"}]"#);
        }
        let provider = MockProvider::from_env();
        let next = provider.next_response();
        assert!(matches!(next, MockResponse::Error(e) if e == "boom"));
        unsafe { std::env::remove_var("KODA_MOCK_RESPONSES") };
    }

    #[test]
    fn test_from_env_with_rate_limit() {
        let _guard = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var("KODA_MOCK_RESPONSES", r#"[{"rate_limit": true}]"#);
        }
        let provider = MockProvider::from_env();
        let next = provider.next_response();
        assert!(matches!(next, MockResponse::RateLimit));
        unsafe { std::env::remove_var("KODA_MOCK_RESPONSES") };
    }

    #[test]
    fn test_from_env_with_context_overflow() {
        let _guard = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var("KODA_MOCK_RESPONSES", r#"[{"context_overflow": true}]"#);
        }
        let provider = MockProvider::from_env();
        let next = provider.next_response();
        assert!(matches!(next, MockResponse::ContextOverflow));
        unsafe { std::env::remove_var("KODA_MOCK_RESPONSES") };
    }

    // ── provider metadata ─────────────────────────────────────────────

    #[test]
    fn test_provider_name() {
        let p = MockProvider::new(vec![]);
        assert_eq!(p.provider_name(), "mock");
    }

    #[tokio::test]
    async fn test_list_models_returns_mock_model() {
        let p = MockProvider::new(vec![]);
        let models = p.list_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "mock-model");
    }

    // ── non-streaming chat ───────────────────────────────────────────

    #[tokio::test]
    async fn test_chat_text_response() {
        let settings = ModelSettings::defaults_for("mock", &crate::config::ProviderType::LMStudio);
        let provider = MockProvider::new(vec![MockResponse::Text("hi there".into())]);
        let resp = provider.chat(&[], &[], &settings).await.unwrap();
        assert_eq!(resp.content.as_deref(), Some("hi there"));
        assert!(resp.tool_calls.is_empty());
    }

    #[tokio::test]
    async fn test_chat_empty_queue_returns_empty_text() {
        let settings = ModelSettings::defaults_for("mock", &crate::config::ProviderType::LMStudio);
        let provider = MockProvider::new(vec![]);
        let resp = provider.chat(&[], &[], &settings).await.unwrap();
        assert_eq!(resp.content.as_deref(), Some(""));
    }

    #[tokio::test]
    async fn test_chat_rate_limit_is_error() {
        let settings = ModelSettings::defaults_for("mock", &crate::config::ProviderType::LMStudio);
        let provider = MockProvider::new(vec![MockResponse::RateLimit]);
        let result = provider.chat(&[], &[], &settings).await;
        assert!(result.is_err());
        // Error message: "LLM API returned 429: Too Many Requests"
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("429") || msg.to_lowercase().contains("too many"),
            "unexpected rate-limit msg: {msg}"
        );
    }

    // ── remaining chat() variants ───────────────────────────────────────

    #[tokio::test]
    async fn test_chat_tool_calls_response() {
        let settings = ModelSettings::defaults_for("mock", &crate::config::ProviderType::LMStudio);
        let provider = MockProvider::new(vec![MockResponse::tool_call(
            "Bash",
            serde_json::json!({"command": "ls"}),
        )]);
        let resp = provider.chat(&[], &[], &settings).await.unwrap();
        assert!(resp.content.is_none());
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].function_name, "Bash");
    }

    #[tokio::test]
    async fn test_chat_text_max_tokens_stop_reason() {
        let settings = ModelSettings::defaults_for("mock", &crate::config::ProviderType::LMStudio);
        let provider =
            MockProvider::new(vec![MockResponse::TextMaxTokens("truncated text".into())]);
        let resp = provider.chat(&[], &[], &settings).await.unwrap();
        assert_eq!(resp.content.as_deref(), Some("truncated text"));
        assert_eq!(resp.usage.stop_reason, "max_tokens");
    }

    #[tokio::test]
    async fn test_chat_context_overflow_is_error() {
        let settings = ModelSettings::defaults_for("mock", &crate::config::ProviderType::LMStudio);
        let provider = MockProvider::new(vec![MockResponse::ContextOverflow]);
        let result = provider.chat(&[], &[], &settings).await;
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("too long"),
            "should mention context length"
        );
    }

    #[tokio::test]
    async fn test_chat_network_error() {
        let settings = ModelSettings::defaults_for("mock", &crate::config::ProviderType::LMStudio);
        let provider = MockProvider::new(vec![MockResponse::NetworkError {
            partial_text: "partial...".into(),
            error: "connection reset".into(),
        }]);
        let result = provider.chat(&[], &[], &settings).await;
        assert!(result.is_err());
    }

    // ── chat_stream() remaining variants ────────────────────────────────

    #[tokio::test]
    async fn test_stream_text_max_tokens() {
        let settings = ModelSettings::defaults_for("mock", &crate::config::ProviderType::LMStudio);
        let provider = MockProvider::new(vec![MockResponse::TextMaxTokens("hi there".into())]);
        let collector = provider.chat_stream(&[], &[], &settings).await.unwrap();
        let chunks = collect_chunks(collector).await;
        // Should end with Done with stop_reason = "max_tokens"
        let done = chunks
            .iter()
            .find(|c| matches!(c, StreamChunk::Done(u) if u.stop_reason == "max_tokens"));
        assert!(done.is_some(), "should emit max_tokens Done chunk");
    }

    #[tokio::test]
    async fn test_stream_tool_calls_eager() {
        let settings = ModelSettings::defaults_for("mock", &crate::config::ProviderType::LMStudio);
        let provider = MockProvider::new(vec![MockResponse::ToolCallsEager(vec![ToolCall {
            id: "tc1".into(),
            function_name: "Read".into(),
            arguments: "{}".into(),
            thought_signature: None,
        }])]);
        let collector = provider.chat_stream(&[], &[], &settings).await.unwrap();
        let chunks = collect_chunks(collector).await;
        // Should have a ToolCallReady chunk
        assert!(
            chunks
                .iter()
                .any(|c| matches!(c, StreamChunk::ToolCallReady(tc) if tc.function_name == "Read")),
            "expected ToolCallReady chunk"
        );
    }

    #[tokio::test]
    async fn test_stream_network_error() {
        let settings = ModelSettings::defaults_for("mock", &crate::config::ProviderType::LMStudio);
        let provider = MockProvider::new(vec![MockResponse::NetworkError {
            partial_text: "partial output".into(),
            error: "connection dropped".into(),
        }]);
        let collector = provider.chat_stream(&[], &[], &settings).await.unwrap();
        let chunks = collect_chunks(collector).await;
        assert!(
            chunks
                .iter()
                .any(|c| matches!(c, StreamChunk::TextDelta(s) if s == "partial output")),
            "should emit partial text"
        );
        assert!(
            chunks
                .iter()
                .any(|c| matches!(c, StreamChunk::NetworkError(_))),
            "should emit NetworkError chunk"
        );
    }
}
