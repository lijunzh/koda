//! Helper functions for inference — context estimation, message assembly,
//! error classification.

use crate::providers::{ChatMessage, ToolCall};

/// Pre-flight context budget threshold (percentage).
/// If context usage exceeds this before calling the provider, auto-compact first.
pub const PREFLIGHT_COMPACT_THRESHOLD: usize = 90;

/// Characters-per-token ratio for heuristic estimation.
/// 3.5 aligns better with provider-reported counts for code-heavy sessions
/// than the naive 4.0 estimate.
pub const CHARS_PER_TOKEN: f64 = 3.5;

/// Per-message overhead in tokens (accounts for role, separators, etc.).
pub const PER_MESSAGE_OVERHEAD: usize = 10;

/// Overhead for the system prompt beyond its character content
/// (tool schemas, message framing, etc.).
pub const SYSTEM_PROMPT_OVERHEAD: usize = 100;

/// Estimate token count for a set of messages.
///
/// Uses a calibrated heuristic: `chars / CHARS_PER_TOKEN + PER_MESSAGE_OVERHEAD`.
pub fn estimate_tokens(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .map(|m| {
            let content_len = m.content.as_deref().map_or(0, |c| c.len());
            let tc_len = m
                .tool_calls
                .as_ref()
                .map_or(0, |tc| serde_json::to_string(tc).map_or(0, |s| s.len()));
            ((content_len + tc_len) as f64 / CHARS_PER_TOKEN) as usize + PER_MESSAGE_OVERHEAD
        })
        .sum()
}

/// Assemble messages from DB history into ChatMessage vec.
pub fn assemble_messages(
    system_message: &ChatMessage,
    history: &[crate::db::Message],
) -> Vec<ChatMessage> {
    let mut messages = vec![system_message.clone()];
    for msg in history {
        let tool_calls: Option<Vec<ToolCall>> = msg
            .tool_calls
            .as_deref()
            .and_then(|tc| serde_json::from_str(tc).ok());
        messages.push(ChatMessage {
            role: msg.role.clone(),
            content: msg.content.clone(),
            tool_calls,
            tool_call_id: msg.tool_call_id.clone(),
            images: None,
        });
    }
    messages
}

/// Detect if an error is a rate limit (429) from the provider.
pub fn is_rate_limit_error(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}").to_lowercase();
    msg.contains("429")
        || msg.contains("rate limit")
        || msg.contains("rate_limit")
        || msg.contains("too many requests")
        || msg.contains("quota exceeded")
}

/// Maximum number of retries for rate-limited requests.
pub const RATE_LIMIT_MAX_RETRIES: u32 = 5;

/// Compute exponential backoff delay for a retry attempt (1-indexed).
/// Returns duration in seconds: 2, 4, 8, 16, 32 (capped at 32s).
pub fn rate_limit_backoff(attempt: u32) -> std::time::Duration {
    let secs = 2u64.pow(attempt).min(32);
    std::time::Duration::from_secs(secs)
}

/// Detect if an error is a context window overflow from the provider.
///
/// Checks for common error patterns across providers:
/// - Anthropic: "prompt is too long", "input is too long"
/// - OpenAI: "maximum context length exceeded", "context_length_exceeded"
/// - Generic: HTTP 400/413 with size-related messages
pub fn is_context_overflow_error(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}").to_lowercase();
    msg.contains("too long")
        || msg.contains("context_length_exceeded")
        || msg.contains("maximum context length")
        || msg.contains("token limit")
        || msg.contains("exceeds the model")
        || msg.contains("request too large")
        || (msg.contains("413") && msg.contains("too large"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_context_overflow_error() {
        // Should match
        assert!(is_context_overflow_error(&anyhow::anyhow!(
            "Anthropic API returned 400: prompt is too long"
        )));
        assert!(is_context_overflow_error(&anyhow::anyhow!(
            "context_length_exceeded: max 200000 tokens"
        )));
        assert!(is_context_overflow_error(&anyhow::anyhow!(
            "maximum context length exceeded"
        )));
        assert!(is_context_overflow_error(&anyhow::anyhow!(
            "request exceeds the model's input limit"
        )));

        // Should NOT match
        assert!(!is_context_overflow_error(&anyhow::anyhow!(
            "rate limit exceeded"
        )));
        assert!(!is_context_overflow_error(&anyhow::anyhow!(
            "connection refused"
        )));
    }

    #[test]
    fn test_is_rate_limit_error() {
        assert!(is_rate_limit_error(&anyhow::anyhow!(
            "429 Too Many Requests"
        )));
        assert!(is_rate_limit_error(&anyhow::anyhow!("rate limit exceeded")));
        assert!(is_rate_limit_error(&anyhow::anyhow!("rate_limit_exceeded")));
        assert!(is_rate_limit_error(&anyhow::anyhow!("too many requests")));
        assert!(is_rate_limit_error(&anyhow::anyhow!("quota exceeded")));

        assert!(!is_rate_limit_error(&anyhow::anyhow!("prompt is too long")));
        assert!(!is_rate_limit_error(&anyhow::anyhow!("connection refused")));
    }

    #[test]
    fn test_rate_limit_backoff() {
        assert_eq!(rate_limit_backoff(0).as_secs(), 1);
        assert_eq!(rate_limit_backoff(1).as_secs(), 2);
        assert_eq!(rate_limit_backoff(2).as_secs(), 4);
        assert_eq!(rate_limit_backoff(3).as_secs(), 8);
        assert_eq!(rate_limit_backoff(10).as_secs(), 32); // capped
    }

    #[test]
    fn test_estimate_tokens() {
        let messages = vec![
            ChatMessage::text("system", "You are helpful."),
            ChatMessage::text("user", "Hello world"),
        ];
        let tokens = estimate_tokens(&messages);
        // "You are helpful." = 16 chars / 3.5 + 10 ≈ 14
        // "Hello world" = 11 chars / 3.5 + 10 ≈ 13
        assert!(tokens > 20 && tokens < 40, "tokens={tokens}");
    }
}
