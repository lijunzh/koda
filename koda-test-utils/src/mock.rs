//! A mock LLM provider that returns scripted responses, so in-process tests
//! can exercise the full inference loop without a real LLM. Responses are
//! consumed in FIFO order.
//!
//! It lives here, in a crate that is never published or shipped, rather
//! than in `koda-providers`: the `koda` binary can't select it, and it
//! isn't public API on crates.io. It uses only `koda-providers`' public API.
//! A test that runs the `koda` binary scripts a stub server instead
//! ([`crate::network::FakeLlmServer::mount_script`]), in the same format
//! ([`parse_script`]).

use koda_providers::stream_collector::SseCollector;
use koda_providers::{
    ChatMessage, LlmProvider, LlmResponse, ModelInfo, ModelSettings, StreamChunk, TokenUsage,
    ToolCall, ToolDefinition,
};

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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
    /// Simulate an error that isn't a provider failure.
    Error(String),
    /// Simulate a non-2xx HTTP response.
    Http {
        /// The status code.
        status: u16,
        /// The response body.
        body: String,
    },
    /// Simulate a rate limit (429) error that says to retry at once.
    RateLimit,
    /// A rate limit (429) that says to retry after this many seconds: a
    /// real wait, for a test that stops the turn during it.
    RateLimitAfter(u64),
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

/// A script of responses from its JSON form, an array:
/// `[{"text":"hello"}, {"tool":"Read","args":{"path":"f.txt"}}, {"error":"boom"},
/// {"status":503,"body":"…"}, {"rate_limit":true}, {"context_overflow":true}]`.
/// Anything else is sent back as text.
///
/// Panics if `json` is not a JSON array.
pub fn parse_script(json: &str) -> Vec<MockResponse> {
    let raw: Vec<serde_json::Value> =
        serde_json::from_str(json).expect("a mock script must be a JSON array");
    raw.into_iter()
        .map(|v| {
            if let Some(text) = v.get("text").and_then(|t| t.as_str()) {
                MockResponse::Text(text.to_string())
            } else if let Some(tool) = v.get("tool").and_then(|t| t.as_str()) {
                let args = v.get("args").cloned().unwrap_or(serde_json::json!({}));
                MockResponse::tool_call(tool, args)
            } else if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
                MockResponse::Error(err.to_string())
            } else if let Some(status) = v.get("status").and_then(|s| s.as_u64()) {
                MockResponse::Http {
                    status: status as u16,
                    body: v["body"].as_str().unwrap_or_default().to_string(),
                }
            } else if v.get("rate_limit").is_some() {
                MockResponse::RateLimit
            } else if v.get("context_overflow").is_some() {
                MockResponse::ContextOverflow
            } else {
                MockResponse::Text(v.to_string())
            }
        })
        .collect()
}

/// A mock LLM provider that returns scripted responses.
///
/// Responses are consumed in FIFO order. Graceful fallback to empty text when
/// the queue is exhausted.
///
/// Every `chat()` / `chat_stream()` call records the received messages in
/// `recorded_calls`.
pub struct MockProvider {
    responses: Mutex<Vec<MockResponse>>,
    /// Records every messages slice passed to `chat()` / `chat_stream()`.
    recorded_calls: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
    /// The `max_tokens` setting of every `chat()` call, in order.
    recorded_max_tokens: Mutex<Vec<Option<u32>>>,
}

impl MockProvider {
    /// Create a mock provider that returns the given responses in order.
    pub fn new(responses: Vec<MockResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
            recorded_calls: Arc::new(Mutex::new(Vec::new())),
            recorded_max_tokens: Mutex::new(Vec::new()),
        }
    }

    /// A provider playing a script in [`parse_script`]'s JSON format.
    pub fn from_json_str(json: &str) -> Self {
        Self::new(parse_script(json))
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

    /// The `max_tokens` each `chat()` call asked for, in order.
    pub fn recorded_max_tokens(&self) -> Vec<Option<u32>> {
        self.recorded_max_tokens.lock().unwrap().clone()
    }

    /// Returns a cheaply-cloneable handle to the recorded-calls buffer.
    /// Useful when callers must move the provider into a `Box<dyn LlmProvider>`
    /// (which loses access to `recorded_calls()`) but still want to inspect
    /// the recorded messages from the outside afterward.
    pub fn recorded_calls_handle(&self) -> Arc<Mutex<Vec<Vec<ChatMessage>>>> {
        Arc::clone(&self.recorded_calls)
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    async fn chat(
        &self,
        messages: &[ChatMessage],
        _tools: &[ToolDefinition],
        settings: &ModelSettings,
    ) -> Result<LlmResponse> {
        self.recorded_calls.lock().unwrap().push(messages.to_vec());
        self.recorded_max_tokens
            .lock()
            .unwrap()
            .push(settings.max_tokens);
        match self.next_response() {
            MockResponse::Text(text) => Ok(LlmResponse {
                content: Some(text),
                tool_calls: vec![],
                reasoning: None,
                usage: TokenUsage::default(),
            }),
            MockResponse::ToolCalls(calls) => Ok(LlmResponse {
                content: None,
                tool_calls: calls,
                reasoning: None,
                usage: TokenUsage::default(),
            }),
            MockResponse::TextMaxTokens(text) => Ok(LlmResponse {
                content: Some(text),
                tool_calls: vec![],
                reasoning: None,
                usage: TokenUsage {
                    stop_reason: "max_tokens".into(),
                    ..Default::default()
                },
            }),
            MockResponse::ToolCallsEager(calls) => Ok(LlmResponse {
                content: None,
                tool_calls: calls,
                reasoning: None,
                usage: TokenUsage::default(),
            }),
            MockResponse::Error(msg) => Err(anyhow::anyhow!(msg)),
            MockResponse::Http { status, body } => Err(http_error(status, &body)),
            MockResponse::RateLimit => Err(http_error(429, "Too Many Requests")),
            MockResponse::RateLimitAfter(secs) => {
                Err(http_error_after(429, "Too Many Requests", secs))
            }
            MockResponse::ContextOverflow => Err(http_error(
                400,
                "prompt is too long, maximum context length exceeded",
            )),
            MockResponse::NetworkError { .. } => Err(anyhow::anyhow!("network error")),
        }
    }

    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        _tools: &[ToolDefinition],
        _settings: &ModelSettings,
    ) -> Result<SseCollector> {
        self.recorded_calls.lock().unwrap().push(messages.to_vec());
        let response = self.next_response();

        // Error responses fail at the call site, not inside the stream.
        match &response {
            MockResponse::Error(msg) => return Err(anyhow::anyhow!("{msg}")),
            MockResponse::Http { status, body } => return Err(http_error(*status, body)),
            MockResponse::RateLimit => return Err(http_error(429, "Too Many Requests")),
            MockResponse::RateLimitAfter(secs) => {
                return Err(http_error_after(429, "Too Many Requests", *secs));
            }
            MockResponse::ContextOverflow => {
                return Err(http_error(
                    400,
                    "prompt is too long, maximum context length exceeded",
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
                | MockResponse::Http { .. }
                | MockResponse::RateLimit
                | MockResponse::RateLimitAfter(_)
                | MockResponse::ContextOverflow => unreachable!(),
            }
        });

        Ok(SseCollector { rx, handle })
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

/// The error the real HTTP path builds for this status and body.
fn http_error(status: u16, body: &str) -> anyhow::Error {
    // The server says to retry at once, so a retry test doesn't wait out
    // real backoff; the backoff itself is tested in koda-core's unit tests.
    http_error_after(status, body, 0)
}

/// [`http_error`] whose server says to retry after `secs` seconds.
fn http_error_after(status: u16, body: &str, secs: u64) -> anyhow::Error {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("retry-after", reqwest::header::HeaderValue::from(secs));
    koda_providers::error::ProviderError::from_response(
        "LLM",
        status,
        &status.to_string(),
        &headers,
        body,
    )
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_text_response() {
        let provider = MockProvider::new(vec![MockResponse::Text("hello".into())]);
        let collector = provider
            .chat_stream(
                &[],
                &[],
                &ModelSettings::defaults_for(
                    "mock",
                    &koda_providers::provider_catalog::ProviderType::LMStudio,
                ),
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_tool_call_response() {
        let provider = MockProvider::new(vec![MockResponse::tool_call(
            "Bash",
            serde_json::json!({"command": "echo hi"}),
        )]);
        let collector = provider
            .chat_stream(
                &[],
                &[],
                &ModelSettings::defaults_for(
                    "mock",
                    &koda_providers::provider_catalog::ProviderType::LMStudio,
                ),
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_error_response() {
        let provider = MockProvider::new(vec![MockResponse::Error("boom".into())]);
        let result = provider
            .chat_stream(
                &[],
                &[],
                &ModelSettings::defaults_for(
                    "mock",
                    &koda_providers::provider_catalog::ProviderType::LMStudio,
                ),
            )
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("boom"));
    }

    async fn collect_chunks(collector: SseCollector) -> Vec<StreamChunk> {
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

    // ── parse_script / from_json_str ───────────────────────────

    #[test]
    fn test_from_json_str_empty_array_gives_empty_provider() {
        let provider = MockProvider::from_json_str("[]");
        let next = provider.next_response();
        assert!(matches!(next, MockResponse::Text(t) if t.is_empty()));
    }

    #[test]
    fn test_from_json_str_with_text_response() {
        let provider = MockProvider::from_json_str(r#"[{"text": "hello from env"}]"#);
        let next = provider.next_response();
        assert!(matches!(next, MockResponse::Text(t) if t == "hello from env"));
    }

    #[test]
    fn test_from_json_str_with_tool_call() {
        let provider =
            MockProvider::from_json_str(r#"[{"tool": "Bash", "args": {"command": "ls"}}]"#);
        let next = provider.next_response();
        assert!(matches!(next, MockResponse::ToolCalls(calls) if calls[0].function_name == "Bash"));
    }

    #[test]
    fn test_from_json_str_with_error() {
        let provider = MockProvider::from_json_str(r#"[{"error": "boom"}]"#);
        let next = provider.next_response();
        assert!(matches!(next, MockResponse::Error(e) if e == "boom"));
    }

    #[test]
    fn test_from_json_str_with_rate_limit() {
        let provider = MockProvider::from_json_str(r#"[{"rate_limit": true}]"#);
        let next = provider.next_response();
        assert!(matches!(next, MockResponse::RateLimit));
    }

    #[test]
    fn test_from_json_str_with_context_overflow() {
        let provider = MockProvider::from_json_str(r#"[{"context_overflow": true}]"#);
        let next = provider.next_response();
        assert!(matches!(next, MockResponse::ContextOverflow));
    }

    // ── provider metadata ─────────────────────────────────────────────

    #[test]
    fn test_provider_name() {
        let p = MockProvider::new(vec![]);
        assert_eq!(p.provider_name(), "mock");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_list_models_returns_mock_model() {
        let p = MockProvider::new(vec![]);
        let models = p.list_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "mock-model");
    }

    // ── non-streaming chat ───────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_chat_text_response() {
        let settings = ModelSettings::defaults_for(
            "mock",
            &koda_providers::provider_catalog::ProviderType::LMStudio,
        );
        let provider = MockProvider::new(vec![MockResponse::Text("hi there".into())]);
        let resp = provider.chat(&[], &[], &settings).await.unwrap();
        assert_eq!(resp.content.as_deref(), Some("hi there"));
        assert!(resp.tool_calls.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_chat_empty_queue_returns_empty_text() {
        let settings = ModelSettings::defaults_for(
            "mock",
            &koda_providers::provider_catalog::ProviderType::LMStudio,
        );
        let provider = MockProvider::new(vec![]);
        let resp = provider.chat(&[], &[], &settings).await.unwrap();
        assert_eq!(resp.content.as_deref(), Some(""));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_chat_rate_limit_is_error() {
        let settings = ModelSettings::defaults_for(
            "mock",
            &koda_providers::provider_catalog::ProviderType::LMStudio,
        );
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_chat_tool_calls_response() {
        let settings = ModelSettings::defaults_for(
            "mock",
            &koda_providers::provider_catalog::ProviderType::LMStudio,
        );
        let provider = MockProvider::new(vec![MockResponse::tool_call(
            "Bash",
            serde_json::json!({"command": "ls"}),
        )]);
        let resp = provider.chat(&[], &[], &settings).await.unwrap();
        assert!(resp.content.is_none());
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].function_name, "Bash");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_chat_text_max_tokens_stop_reason() {
        let settings = ModelSettings::defaults_for(
            "mock",
            &koda_providers::provider_catalog::ProviderType::LMStudio,
        );
        let provider =
            MockProvider::new(vec![MockResponse::TextMaxTokens("truncated text".into())]);
        let resp = provider.chat(&[], &[], &settings).await.unwrap();
        assert_eq!(resp.content.as_deref(), Some("truncated text"));
        assert_eq!(resp.usage.stop_reason, "max_tokens");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_chat_context_overflow_is_error() {
        let settings = ModelSettings::defaults_for(
            "mock",
            &koda_providers::provider_catalog::ProviderType::LMStudio,
        );
        let provider = MockProvider::new(vec![MockResponse::ContextOverflow]);
        let result = provider.chat(&[], &[], &settings).await;
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("too long"),
            "should mention context length"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_chat_network_error() {
        let settings = ModelSettings::defaults_for(
            "mock",
            &koda_providers::provider_catalog::ProviderType::LMStudio,
        );
        let provider = MockProvider::new(vec![MockResponse::NetworkError {
            partial_text: "partial...".into(),
            error: "connection reset".into(),
        }]);
        let result = provider.chat(&[], &[], &settings).await;
        assert!(result.is_err());
    }

    // ── chat_stream() remaining variants ────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_stream_text_max_tokens() {
        let settings = ModelSettings::defaults_for(
            "mock",
            &koda_providers::provider_catalog::ProviderType::LMStudio,
        );
        let provider = MockProvider::new(vec![MockResponse::TextMaxTokens("hi there".into())]);
        let collector = provider.chat_stream(&[], &[], &settings).await.unwrap();
        let chunks = collect_chunks(collector).await;
        // Should end with Done with stop_reason = "max_tokens"
        let done = chunks
            .iter()
            .find(|c| matches!(c, StreamChunk::Done(u) if u.stop_reason == "max_tokens"));
        assert!(done.is_some(), "should emit max_tokens Done chunk");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_stream_tool_calls_eager() {
        let settings = ModelSettings::defaults_for(
            "mock",
            &koda_providers::provider_catalog::ProviderType::LMStudio,
        );
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_stream_network_error() {
        let settings = ModelSettings::defaults_for(
            "mock",
            &koda_providers::provider_catalog::ProviderType::LMStudio,
        );
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
