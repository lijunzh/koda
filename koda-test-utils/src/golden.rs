//! Golden-file recording and replay for LLM provider testing.
//!
//! **RecordingProvider** wraps any real [`LlmProvider`], forwards calls to it,
//! and captures the responses as [`MockResponse`] entries.  When dropped (or
//! on explicit `save()`), the captured exchanges are written to a JSON file.
//!
//! **`replay()`** loads a golden file and returns a [`MockProvider`] that
//! replays the recorded responses — no real LLM needed.
//!
//! # Workflow
//!
//! ```text
//! 1. Record:  KODA_RECORD_GOLDEN=tests/fixtures/my_test.json cargo test my_test
//! 2. Verify:  inspect the JSON file, commit it
//! 3. Replay:  let provider = golden::replay("tests/fixtures/my_test.json");
//! ```
//!
//! # File format
//!
//! ```json
//! {
//!   "provider": "anthropic",
//!   "model": "claude-sonnet-4-6",
//!   "responses": [
//!     { "Text": "Hello! How can I help?" },
//!     { "ToolCalls": [{ "id": "call_1", "function_name": "Read", "arguments": "{}" }] },
//!     { "Text": "Done!" }
//!   ]
//! }
//! ```

use anyhow::Result;
use async_trait::async_trait;
use koda_core::config::ModelSettings;
use koda_core::providers::mock::{MockProvider, MockResponse};
use koda_core::providers::{
    ChatMessage, LlmProvider, LlmResponse, ModelCapabilities, ModelInfo, StreamChunk,
    ToolDefinition,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// On-disk format for a golden file.
#[derive(Debug, Serialize, Deserialize)]
pub struct GoldenFile {
    /// Provider name (for documentation only).
    pub provider: String,
    /// Model used during recording (for documentation only).
    pub model: String,
    /// Recorded responses in order.
    pub responses: Vec<MockResponse>,
}

/// Load a golden file and return a `MockProvider` that replays its responses.
pub fn replay(path: impl AsRef<Path>) -> MockProvider {
    let data = std::fs::read_to_string(path.as_ref())
        .unwrap_or_else(|e| panic!("failed to read golden file {:?}: {e}", path.as_ref()));
    let golden: GoldenFile = serde_json::from_str(&data)
        .unwrap_or_else(|e| panic!("failed to parse golden file {:?}: {e}", path.as_ref()));
    MockProvider::new(golden.responses)
}

/// A provider wrapper that records responses from a real provider.
///
/// Forwards all calls to the inner provider, captures the responses,
/// and writes them to a JSON file when [`save()`](Self::save) is called.
pub struct RecordingProvider<P: LlmProvider> {
    inner: P,
    output_path: PathBuf,
    responses: Arc<Mutex<Vec<MockResponse>>>,
}

impl<P: LlmProvider> RecordingProvider<P> {
    /// Wrap a real provider, recording responses to `output_path`.
    pub fn new(inner: P, output_path: impl Into<PathBuf>) -> Self {
        Self {
            inner,
            output_path: output_path.into(),
            responses: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Write the recorded responses to the golden file.
    pub fn save(&self) -> Result<()> {
        let responses = self.responses.lock().unwrap().clone();
        let golden = GoldenFile {
            provider: self.inner.provider_name().to_string(),
            model: "recorded".to_string(),
            responses,
        };
        if let Some(parent) = self.output_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&golden)?;
        std::fs::write(&self.output_path, json)?;
        Ok(())
    }

    fn push(&self, response: MockResponse) {
        self.responses.lock().unwrap().push(response);
    }
}

impl<P: LlmProvider> Drop for RecordingProvider<P> {
    fn drop(&mut self) {
        if let Err(e) = self.save() {
            eprintln!("WARNING: failed to save golden file: {e}");
        }
    }
}

#[async_trait]
impl<P: LlmProvider> LlmProvider for RecordingProvider<P> {
    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        settings: &ModelSettings,
    ) -> Result<LlmResponse> {
        let result = self.inner.chat(messages, tools, settings).await;
        match &result {
            Ok(resp) => {
                let mock = response_to_mock(resp);
                self.push(mock);
            }
            Err(e) => {
                let msg = format!("{e:#}");
                if msg.contains("429") || msg.contains("Too Many Requests") {
                    self.push(MockResponse::RateLimit);
                } else if msg.contains("context") && msg.contains("overflow") {
                    self.push(MockResponse::ContextOverflow);
                } else {
                    self.push(MockResponse::Error(msg));
                }
            }
        }
        result
    }

    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        settings: &ModelSettings,
    ) -> Result<mpsc::Receiver<StreamChunk>> {
        let result = self.inner.chat_stream(messages, tools, settings).await;
        match result {
            Ok(mut rx) => {
                // Collect the full stream, record it, then re-emit via a new channel.
                let (tx, new_rx) = mpsc::channel(256);
                let responses = self.responses.clone();
                tokio::spawn(async move {
                    let mut text = String::new();
                    let mut tool_calls = Vec::new();
                    let mut had_network_error = false;
                    let mut max_tokens = false;

                    while let Some(chunk) = rx.recv().await {
                        match &chunk {
                            StreamChunk::TextDelta(t) => text.push_str(t),
                            StreamChunk::ThinkingDelta(_) => {} // skip thinking for replay
                            StreamChunk::ToolCallReady(tc) => tool_calls.push(tc.clone()),
                            StreamChunk::ToolCalls(tcs) => tool_calls.extend(tcs.iter().cloned()),
                            StreamChunk::Done(usage) => {
                                max_tokens = usage.stop_reason == "max_tokens";
                            }
                            StreamChunk::NetworkError(_) => had_network_error = true,
                        }
                        // Forward to consumer
                        let _ = tx.send(chunk).await;
                    }

                    // Record the aggregated response
                    let mock = if had_network_error {
                        MockResponse::NetworkError {
                            partial_text: text,
                            error: "recorded network error".into(),
                        }
                    } else if !tool_calls.is_empty() {
                        MockResponse::ToolCalls(tool_calls)
                    } else if max_tokens {
                        MockResponse::TextMaxTokens(text)
                    } else {
                        MockResponse::Text(text)
                    };
                    responses.lock().unwrap().push(mock);
                });

                Ok(new_rx)
            }
            Err(e) => {
                let msg = format!("{e:#}");
                if msg.contains("429") {
                    self.push(MockResponse::RateLimit);
                } else {
                    self.push(MockResponse::Error(msg));
                }
                Err(e)
            }
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        self.inner.list_models().await
    }

    async fn model_capabilities(&self, model: &str) -> Result<ModelCapabilities> {
        self.inner.model_capabilities(model).await
    }

    fn provider_name(&self) -> &str {
        self.inner.provider_name()
    }
}

/// Convert an `LlmResponse` to the closest `MockResponse` variant.
fn response_to_mock(resp: &LlmResponse) -> MockResponse {
    if !resp.tool_calls.is_empty() {
        MockResponse::ToolCalls(resp.tool_calls.clone())
    } else if resp.usage.stop_reason == "max_tokens" {
        MockResponse::TextMaxTokens(resp.content.clone().unwrap_or_default())
    } else {
        MockResponse::Text(resp.content.clone().unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn round_trip_golden_file() {
        let responses = vec![
            MockResponse::Text("Hello!".into()),
            MockResponse::ToolCalls(vec![koda_core::providers::ToolCall {
                id: "call_1".into(),
                function_name: "Read".into(),
                arguments: r#"{"path":"foo.rs"}"#.into(),
                thought_signature: None,
            }]),
            MockResponse::Text("Done!".into()),
        ];

        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        let golden = GoldenFile {
            provider: "test".into(),
            model: "test-model".into(),
            responses: responses.clone(),
        };
        let json = serde_json::to_string_pretty(&golden).unwrap();
        std::fs::write(&path, json).unwrap();

        // Replay and verify
        let provider = replay(&path);
        // MockProvider exposes recorded_calls after use, but we can at least
        // verify it was created without panicking.
        assert_eq!(provider.provider_name(), "mock");
    }

    #[test]
    fn serde_round_trip_all_variants() {
        let variants = vec![
            MockResponse::Text("hello".into()),
            MockResponse::TextMaxTokens("trunc".into()),
            MockResponse::ToolCalls(vec![]),
            MockResponse::ToolCallsEager(vec![]),
            MockResponse::Error("boom".into()),
            MockResponse::RateLimit,
            MockResponse::ContextOverflow,
            MockResponse::NetworkError {
                partial_text: "partial".into(),
                error: "dropped".into(),
            },
        ];

        let json = serde_json::to_string(&variants).unwrap();
        let parsed: Vec<MockResponse> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), variants.len());
    }
}
