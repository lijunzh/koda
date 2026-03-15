//! Shared SSE stream collection for LLM providers.
//!
//! All three providers (Anthropic, Gemini, OpenAI-compat) follow the same
//! structural pattern for streaming:
//!
//! 1. Read bytes from the HTTP response
//! 2. Buffer and split into SSE `data:` lines
//! 3. Parse provider-specific JSON into `StreamChunk`s
//! 4. Accumulate tool calls across chunks
//! 5. Emit `Done` with token usage when the stream ends
//!
//! This module extracts steps 1–2 and the finalization into shared code,
//! delegating steps 3–4 to provider-specific [`ChunkParser`] implementations.

use super::StreamChunk;
use tokio::sync::mpsc;

/// Provider-specific SSE chunk parsing.
///
/// Implementations maintain their own mutable state (tool call accumulators,
/// thinking buffers, tag filters, etc.) and process one SSE data line at a time.
pub trait ChunkParser: Send + 'static {
    /// Process a single SSE data line (`data: ` prefix already stripped).
    /// Returns zero or more chunks to emit immediately.
    fn process_line(&mut self, data: &str) -> Vec<StreamChunk>;

    /// Called when the stream ends (`[DONE]` received or connection closed).
    /// Return any buffered chunks (accumulated tool calls, `Done`, etc.).
    fn finish(&mut self) -> Vec<StreamChunk>;
}

/// Spawn a task that reads an SSE byte stream, parses chunks via the given
/// [`ChunkParser`], and sends [`StreamChunk`]s to the returned receiver.
///
/// Handles:
/// - Byte stream → UTF-8 buffering
/// - SSE line splitting (`\n` delimited)
/// - `data: ` prefix stripping (non-data lines are ignored)
/// - `[DONE]` sentinel (standard SSE terminator used by OpenAI)
/// - Graceful finalization when the stream closes without `[DONE]`
pub fn spawn_sse_collector(
    response: reqwest::Response,
    parser: Box<dyn ChunkParser>,
) -> mpsc::Receiver<StreamChunk> {
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(drive_sse_stream(response, parser, tx));
    rx
}

/// Inner driver: read SSE lines from a byte stream and dispatch to the parser.
///
/// Separated from [`spawn_sse_collector`] so the same logic can be tested
/// with any `Stream<Item = Result<Bytes, _>>` without needing a real HTTP response.
async fn drive_sse_stream(
    response: reqwest::Response,
    mut parser: Box<dyn ChunkParser>,
    tx: mpsc::Sender<StreamChunk>,
) {
    use futures_util::StreamExt;

    let mut byte_stream = response.bytes_stream();
    let mut buffer = String::new();

    while let Some(chunk_result) = byte_stream.next().await {
        let Ok(bytes) = chunk_result else { break };
        buffer.push_str(&String::from_utf8_lossy(&bytes));

        while let Some(line_end) = buffer.find('\n') {
            let line = buffer[..line_end].trim().to_string();
            buffer.drain(..=line_end);

            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };

            // Standard SSE terminator (OpenAI convention)
            if data.trim() == "[DONE]" {
                for chunk in parser.finish() {
                    let _ = tx.send(chunk).await;
                }
                return;
            }

            for chunk in parser.process_line(data) {
                let _ = tx.send(chunk).await;
            }
        }
    }

    // Stream ended without [DONE] (Anthropic, Gemini) — flush remaining
    for chunk in parser.finish() {
        let _ = tx.send(chunk).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::TokenUsage;

    /// Trivial parser that echoes each line as a TextDelta.
    struct EchoParser {
        line_count: usize,
    }

    impl EchoParser {
        fn new() -> Self {
            Self { line_count: 0 }
        }
    }

    impl ChunkParser for EchoParser {
        fn process_line(&mut self, data: &str) -> Vec<StreamChunk> {
            self.line_count += 1;
            vec![StreamChunk::TextDelta(data.to_string())]
        }

        fn finish(&mut self) -> Vec<StreamChunk> {
            vec![StreamChunk::Done(TokenUsage {
                completion_tokens: self.line_count as i64,
                ..Default::default()
            })]
        }
    }

    /// Drive a parser through raw SSE text without an HTTP response.
    /// Used for unit-testing parsers in isolation.
    async fn drive_parser(
        parser: Box<dyn ChunkParser>,
        sse_text: &str,
    ) -> Vec<StreamChunk> {
        let mut parser = parser;
        let mut chunks = Vec::new();

        for line in sse_text.lines() {
            let trimmed = line.trim();
            let Some(data) = trimmed.strip_prefix("data: ") else {
                continue;
            };
            if data.trim() == "[DONE]" {
                chunks.extend(parser.finish());
                return chunks;
            }
            chunks.extend(parser.process_line(data));
        }
        chunks.extend(parser.finish());
        chunks
    }

    #[tokio::test]
    async fn test_basic_sse_parsing() {
        let sse = "data: hello\ndata: world\n";
        let chunks = drive_parser(Box::new(EchoParser::new()), sse).await;

        assert_eq!(chunks.len(), 3); // 2 deltas + Done
        assert!(matches!(&chunks[0], StreamChunk::TextDelta(t) if t == "hello"));
        assert!(matches!(&chunks[1], StreamChunk::TextDelta(t) if t == "world"));
        assert!(matches!(&chunks[2], StreamChunk::Done(u) if u.completion_tokens == 2));
    }

    #[tokio::test]
    async fn test_done_sentinel_triggers_early_finish() {
        let sse = "data: first\ndata: [DONE]\ndata: should-not-appear\n";
        let chunks = drive_parser(Box::new(EchoParser::new()), sse).await;

        assert_eq!(chunks.len(), 2); // 1 delta + Done
        assert!(matches!(&chunks[0], StreamChunk::TextDelta(t) if t == "first"));
        assert!(matches!(&chunks[1], StreamChunk::Done(u) if u.completion_tokens == 1));
    }

    #[tokio::test]
    async fn test_non_data_lines_are_ignored() {
        let sse = "event: message_start\ndata: payload\n: comment\nretry: 5000\n";
        let chunks = drive_parser(Box::new(EchoParser::new()), sse).await;

        assert_eq!(chunks.len(), 2); // 1 delta + Done
        assert!(matches!(&chunks[0], StreamChunk::TextDelta(t) if t == "payload"));
    }

    // ── Anthropic parser fixture tests ────────────────────────────

    use crate::providers::anthropic::AnthropicChunkParser;

    #[tokio::test]
    async fn test_anthropic_text_stream() {
        let sse = r#"data: {"type":"message_start","message":{"usage":{"input_tokens":100,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":50}}}
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" world"}}
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":0,"output_tokens":42,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}
data: {"type":"message_stop"}
"#;
        let chunks = drive_parser(Box::new(AnthropicChunkParser::new()), sse).await;

        // Should produce: TextDelta("Hello"), TextDelta(" world"), Done
        assert!(matches!(&chunks[0], StreamChunk::TextDelta(t) if t == "Hello"));
        assert!(matches!(&chunks[1], StreamChunk::TextDelta(t) if t == " world"));
        match &chunks[2] {
            StreamChunk::Done(u) => {
                assert_eq!(u.prompt_tokens, 100);
                assert_eq!(u.completion_tokens, 42);
                assert_eq!(u.cache_read_tokens, 50);
                assert_eq!(u.stop_reason, "end_turn");
            }
            other => panic!("Expected Done, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_anthropic_thinking_stream() {
        let sse = r#"data: {"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}
data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me think..."}}
data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}
data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Answer"}}
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":0,"output_tokens":20,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}
"#;
        let chunks = drive_parser(Box::new(AnthropicChunkParser::new()), sse).await;

        assert!(matches!(&chunks[0], StreamChunk::ThinkingDelta(t) if t == "Let me think..."));
        assert!(matches!(&chunks[1], StreamChunk::TextDelta(t) if t == "Answer"));
        assert!(matches!(&chunks[2], StreamChunk::Done(_)));
    }

    #[tokio::test]
    async fn test_anthropic_tool_use_stream() {
        let sse = r#"data: {"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}
data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tc_1","name":"read_file","input":{}}}
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"main.rs\"}"}}
data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"input_tokens":0,"output_tokens":15,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}
"#;
        let chunks = drive_parser(Box::new(AnthropicChunkParser::new()), sse).await;

        match &chunks[0] {
            StreamChunk::ToolCalls(tcs) => {
                assert_eq!(tcs.len(), 1);
                assert_eq!(tcs[0].id, "tc_1");
                assert_eq!(tcs[0].function_name, "read_file");
                assert_eq!(tcs[0].arguments, r#"{"path":"main.rs"}"#);
            }
            other => panic!("Expected ToolCalls, got {:?}", other),
        }
        assert!(matches!(&chunks[1], StreamChunk::Done(u) if u.stop_reason == "tool_use"));
    }

    // ── Gemini parser fixture tests ──────────────────────────────

    use crate::providers::gemini::GeminiChunkParser;

    #[tokio::test]
    async fn test_gemini_text_stream() {
        let sse = r#"data: {"candidates":[{"content":{"parts":[{"text":"Hello"}]},"finishReason":null}],"usageMetadata":{"promptTokenCount":50,"candidatesTokenCount":5,"cachedContentTokenCount":0,"thoughtsTokenCount":0}}
data: {"candidates":[{"content":{"parts":[{"text":" world"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":50,"candidatesTokenCount":10,"cachedContentTokenCount":0,"thoughtsTokenCount":0}}
"#;
        let chunks = drive_parser(Box::new(GeminiChunkParser::new()), sse).await;

        assert!(matches!(&chunks[0], StreamChunk::TextDelta(t) if t == "Hello"));
        assert!(matches!(&chunks[1], StreamChunk::TextDelta(t) if t == " world"));
        match &chunks[2] {
            StreamChunk::Done(u) => {
                assert_eq!(u.prompt_tokens, 50);
                assert_eq!(u.completion_tokens, 10);
                assert_eq!(u.stop_reason, "stop"); // normalized to lowercase
            }
            other => panic!("Expected Done, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_gemini_thinking_stream() {
        let sse = r#"data: {"candidates":[{"content":{"parts":[{"text":"Reasoning...","thought":true}]}}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"cachedContentTokenCount":0,"thoughtsTokenCount":5}}
data: {"candidates":[{"content":{"parts":[{"text":"Answer"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":10,"cachedContentTokenCount":0,"thoughtsTokenCount":5}}
"#;
        let chunks = drive_parser(Box::new(GeminiChunkParser::new()), sse).await;

        assert!(matches!(&chunks[0], StreamChunk::ThinkingDelta(t) if t == "Reasoning..."));
        assert!(matches!(&chunks[1], StreamChunk::TextDelta(t) if t == "Answer"));
        match &chunks[2] {
            StreamChunk::Done(u) => {
                assert_eq!(u.thinking_tokens, 5);
            }
            other => panic!("Expected Done, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_gemini_tool_call_stream() {
        let sse = r#"data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"list_files","args":{"dir":"."}}},{"functionCall":{"name":"read_file","args":{"path":"x"}}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"cachedContentTokenCount":0,"thoughtsTokenCount":0}}
"#;
        let chunks = drive_parser(Box::new(GeminiChunkParser::new()), sse).await;

        match &chunks[0] {
            StreamChunk::ToolCalls(tcs) => {
                assert_eq!(tcs.len(), 2);
                assert_eq!(tcs[0].function_name, "list_files");
                assert_eq!(tcs[1].function_name, "read_file");
            }
            other => panic!("Expected ToolCalls, got {:?}", other),
        }
    }

    // ── OpenAI parser fixture tests ─────────────────────────────

    use crate::providers::openai_compat::OpenAiChunkParser;

    #[tokio::test]
    async fn test_openai_text_stream() {
        // Note: StreamTagFilter holds back up to MAX_TAG_LEN (16) bytes to detect
        // <think> tags spanning chunks. Short deltas get buffered until flush.
        let long_text = "This is a long enough text to exceed the tag buffer threshold!!";
        let sse = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{long_text}\"}},\"finish_reason\":null}}],\"usage\":null}}\n\
             data: {{\"choices\":[{{\"delta\":{{\"content\":\" end\"}},\"finish_reason\":\"stop\"}}],\"usage\":{{\"prompt_tokens\":50,\"completion_tokens\":10}}}}\n\
             data: [DONE]\n"
        );
        let chunks = drive_parser(Box::new(OpenAiChunkParser::new()), &sse).await;

        // Collect all text deltas
        let text: String = chunks
            .iter()
            .filter_map(|c| match c {
                StreamChunk::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert!(text.contains(long_text));
        assert!(text.contains(" end"));

        match chunks.last().unwrap() {
            StreamChunk::Done(u) => {
                assert_eq!(u.prompt_tokens, 50);
                assert_eq!(u.completion_tokens, 10);
                assert_eq!(u.stop_reason, "stop");
            }
            other => panic!("Expected Done, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_openai_reasoning_stream() {
        let sse = r#"data: {"choices":[{"delta":{"reasoning_content":"Let me think..."},"finish_reason":null}],"usage":null}
data: {"choices":[{"delta":{"content":"Answer"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"completion_tokens_details":{"reasoning_tokens":3}}}
data: [DONE]
"#;
        let chunks = drive_parser(Box::new(OpenAiChunkParser::new()), sse).await;

        assert!(matches!(&chunks[0], StreamChunk::ThinkingDelta(t) if t == "Let me think..."));
        assert!(matches!(&chunks[1], StreamChunk::TextDelta(t) if t == "Answer"));
        match &chunks[2] {
            StreamChunk::Done(u) => {
                assert_eq!(u.thinking_tokens, 3);
            }
            other => panic!("Expected Done, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_openai_tool_call_stream() {
        let sse = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read","arguments":""}}]},"finish_reason":null}],"usage":null}
data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"f\":\"a\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}
data: [DONE]
"#;
        let chunks = drive_parser(Box::new(OpenAiChunkParser::new()), sse).await;

        match &chunks[0] {
            StreamChunk::ToolCalls(tcs) => {
                assert_eq!(tcs.len(), 1);
                assert_eq!(tcs[0].id, "call_1");
                assert_eq!(tcs[0].function_name, "read");
                assert_eq!(tcs[0].arguments, r#"{"f":"a"}"#);
            }
            other => panic!("Expected ToolCalls, got {:?}", other),
        }
        assert!(matches!(&chunks[1], StreamChunk::Done(u) if u.stop_reason == "tool_calls"));
    }

    #[tokio::test]
    async fn test_openai_length_normalized_to_max_tokens() {
        let sse = r#"data: {"choices":[{"delta":{"content":"x"},"finish_reason":"length"}],"usage":null}
data: [DONE]
"#;
        let chunks = drive_parser(Box::new(OpenAiChunkParser::new()), sse).await;

        match &chunks.last().unwrap() {
            StreamChunk::Done(u) => {
                assert_eq!(u.stop_reason, "max_tokens");
            }
            other => panic!("Expected Done, got {:?}", other),
        }
    }
}
