//! Helper functions for inference — context estimation, message assembly,
//! error classification.

use crate::providers::{ChatMessage, ToolCall};

/// Pre-flight context budget threshold (percentage).
/// If context usage exceeds this before calling the provider, auto-compact first.
pub const PREFLIGHT_COMPACT_THRESHOLD: usize = 90;

/// Estimate token count for a set of messages.
///
/// Uses the rough heuristic of `chars / 4 + 10` per message.
/// Not accurate for code or non-ASCII, but sufficient for budget checks.
pub fn estimate_tokens(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .map(|m| {
            let content_len = m.content.as_deref().map_or(0, |c| c.len());
            let tc_len = m
                .tool_calls
                .as_ref()
                .map_or(0, |tc| serde_json::to_string(tc).map_or(0, |s| s.len()));
            (content_len + tc_len) / 4 + 10
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

/// Format a token count for display: "1.2k" or "500".
pub fn format_token_count(tokens: i64) -> String {
    if tokens >= 1000 {
        format!("{:.1}k", tokens as f64 / 1000.0)
    } else {
        format!("{tokens}")
    }
}

/// Format a duration as human-readable: "5.2s", "1m 23s".
pub fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{:.1}s", d.as_secs_f64())
    } else {
        let mins = secs / 60;
        let remaining = secs % 60;
        format!("{mins}m {remaining}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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
    fn test_estimate_tokens() {
        let messages = vec![
            ChatMessage::text("system", "You are helpful."),
            ChatMessage::text("user", "Hello world"),
        ];
        let tokens = estimate_tokens(&messages);
        // "You are helpful." = 16 chars / 4 + 10 = 14
        // "Hello world" = 11 chars / 4 + 10 = 12
        assert_eq!(tokens, 14 + 12);
    }

    #[test]
    fn test_format_duration_seconds() {
        assert_eq!(format_duration(Duration::from_secs_f64(0.5)), "0.5s");
        assert_eq!(format_duration(Duration::from_secs_f64(5.23)), "5.2s");
        assert_eq!(format_duration(Duration::from_secs_f64(59.9)), "59.9s");
    }

    #[test]
    fn test_format_duration_minutes() {
        assert_eq!(format_duration(Duration::from_secs(60)), "1m 0s");
        assert_eq!(format_duration(Duration::from_secs(83)), "1m 23s");
        assert_eq!(format_duration(Duration::from_secs(125)), "2m 5s");
        assert_eq!(format_duration(Duration::from_secs(600)), "10m 0s");
    }
}
