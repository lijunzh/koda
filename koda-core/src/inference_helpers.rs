//! Helper functions for inference — context estimation, message assembly,
//! error classification.
//!
//! These are pure functions extracted from [`crate::inference`] to keep the
//! main inference loop readable. They handle:
//!
//! - **Context estimation** — count tokens in the conversation to decide
//!   when to compact or truncate
//! - **Message assembly** — convert tool results and progress into the
//!   format expected by each provider
//! - **Error classification** — distinguish retryable errors (rate limits,
//!   network) from fatal ones (auth, invalid model)

use crate::providers::{ChatMessage, ToolCall};

/// Context usage % at which a pre-flight auto-compact fires.
/// Matches CC's default (~85%). Hard-coded — no config knob needed.
pub const AUTO_COMPACT_THRESHOLD: usize = 85;

/// Context usage % at which a user-visible warning is emitted.
/// Sits below `AUTO_COMPACT_THRESHOLD` so users see the warning
/// 1–2 turns before compaction fires.
pub const CONTEXT_WARN_THRESHOLD: usize = 80;

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
///
/// # Examples
///
/// ```
/// use koda_core::inference_helpers::estimate_tokens;
/// use koda_core::providers::ChatMessage;
///
/// let messages = vec![
///     ChatMessage::text("system", "You are helpful."),
///     ChatMessage::text("user", "Hello world"),
/// ];
/// let tokens = estimate_tokens(&messages);
/// assert!(tokens > 20 && tokens < 40);
/// ```
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
            role: msg.role.as_str().to_string(),
            content: msg.content.clone(),
            tool_calls,
            tool_call_id: msg.tool_call_id.clone(),
            images: None,
        });
    }
    messages
}

/// Detect if an error is a server error (5xx) from the provider.
///
/// These are typically transient (LM Studio choking on malformed input,
/// Ollama OOM, etc.) and should end the turn gracefully rather than crash.
///
/// # Examples
///
/// ```
/// use koda_core::inference_helpers::is_server_error;
///
/// assert!(is_server_error(&anyhow::anyhow!("HTTP 500 from provider")));
/// assert!(is_server_error(&anyhow::anyhow!("bad gateway")));
/// assert!(!is_server_error(&anyhow::anyhow!("401 Unauthorized")));
/// ```
pub fn is_server_error(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}").to_lowercase();
    msg.contains("500")
        || msg.contains("502")
        || msg.contains("503")
        || msg.contains("internal server error")
        || msg.contains("bad gateway")
        || msg.contains("service unavailable")
}

/// Detect if an error is a rate limit or overload response from the provider.
///
/// Matches HTTP 429 (Too Many Requests) and Anthropic's HTTP 529 (overloaded),
/// plus common text patterns across providers.
///
/// # Examples
///
/// ```
/// use koda_core::inference_helpers::is_rate_limit_error;
///
/// assert!(is_rate_limit_error(&anyhow::anyhow!("429 Too Many Requests")));
/// assert!(is_rate_limit_error(&anyhow::anyhow!("quota exceeded")));
/// assert!(!is_rate_limit_error(&anyhow::anyhow!("prompt is too long")));
/// ```
pub fn is_rate_limit_error(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}").to_lowercase();
    msg.contains("429")
        || msg.contains("529")          // Anthropic: API overloaded
        || msg.contains("rate limit")
        || msg.contains("rate_limit")
        || msg.contains("too many requests")
        || msg.contains("quota exceeded")
        || msg.contains("overloaded") // Anthropic overload text
}

/// Maximum number of retries for rate-limited requests.
pub const RATE_LIMIT_MAX_RETRIES: u32 = 5;

/// Compute exponential backoff delay for a retry attempt (1-indexed).
/// Returns duration in seconds: 2, 4, 8, 16, 32 (capped at 32s).
///
/// # Examples
///
/// ```
/// use koda_core::inference_helpers::rate_limit_backoff;
/// use std::time::Duration;
///
/// assert_eq!(rate_limit_backoff(1), Duration::from_secs(2));
/// assert_eq!(rate_limit_backoff(3), Duration::from_secs(8));
/// assert_eq!(rate_limit_backoff(10), Duration::from_secs(32)); // capped
/// ```
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
///
/// # Examples
///
/// ```
/// use koda_core::inference_helpers::is_context_overflow_error;
///
/// assert!(is_context_overflow_error(&anyhow::anyhow!("prompt is too long")));
/// assert!(is_context_overflow_error(&anyhow::anyhow!("context_length_exceeded")));
/// assert!(!is_context_overflow_error(&anyhow::anyhow!("rate limit exceeded")));
/// ```
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

/// Detect if an error is a provider rejection of image / vision input.
///
/// Fires when the model or API endpoint does not support multimodal input and
/// returns an explicit error rather than silently ignoring the image bytes.
/// Matches the documented rejection messages from OpenAI-compat servers
/// (LM Studio, Ollama), the OpenAI API, and Gemini.
///
/// # Examples
///
/// ```
/// use koda_core::inference_helpers::is_image_rejection_error;
///
/// assert!(is_image_rejection_error(&anyhow::anyhow!("This model does not support image input")));
/// assert!(is_image_rejection_error(&anyhow::anyhow!("Invalid image. The model does not support vision input.")));
/// assert!(is_image_rejection_error(&anyhow::anyhow!("multimodal content is not supported")));
/// assert!(!is_image_rejection_error(&anyhow::anyhow!("rate limit exceeded")));
/// assert!(!is_image_rejection_error(&anyhow::anyhow!("prompt is too long")));
/// ```
pub fn is_image_rejection_error(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}").to_lowercase();
    // "image" alone is too broad; require it alongside a support-denial word.
    (msg.contains("image") && (msg.contains("support") || msg.contains("invalid")))
        || msg.contains("vision")
        || msg.contains("multimodal")
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
        assert!(is_rate_limit_error(&anyhow::anyhow!("529 API overloaded")));
        assert!(is_rate_limit_error(&anyhow::anyhow!("rate limit exceeded")));
        assert!(is_rate_limit_error(&anyhow::anyhow!("rate_limit_exceeded")));
        assert!(is_rate_limit_error(&anyhow::anyhow!("too many requests")));
        assert!(is_rate_limit_error(&anyhow::anyhow!("quota exceeded")));
        assert!(is_rate_limit_error(&anyhow::anyhow!(
            "Anthropic API is overloaded"
        )));

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

    // ── is_server_error ──────────────────────────────────────────────

    #[test]
    fn test_is_server_error_http_codes() {
        for code in ["500", "502", "503"] {
            let err = anyhow::anyhow!("HTTP {code} from provider");
            assert!(is_server_error(&err), "{code} should be server error");
        }
    }

    #[test]
    fn test_is_server_error_text_patterns() {
        let patterns = [
            "internal server error",
            "bad gateway",
            "service unavailable",
        ];
        for text in patterns {
            let err = anyhow::anyhow!("{text}");
            assert!(is_server_error(&err), "'{text}' should be server error");
        }
    }

    #[test]
    fn test_is_server_error_case_insensitive() {
        let err = anyhow::anyhow!("Internal Server Error from upstream");
        assert!(is_server_error(&err));
    }

    #[test]
    fn test_is_not_server_error_for_rate_limit() {
        let err = anyhow::anyhow!("429 Too Many Requests");
        assert!(
            !is_server_error(&err),
            "rate limit should not be server error"
        );
    }

    #[test]
    fn test_is_not_server_error_for_auth() {
        let err = anyhow::anyhow!("401 Unauthorized");
        assert!(!is_server_error(&err));
    }

    #[test]
    fn test_is_image_rejection_error_matches() {
        // LM Studio / Ollama
        assert!(is_image_rejection_error(&anyhow::anyhow!(
            "LLM API returned 400: This model does not support image input"
        )));
        // OpenAI
        assert!(is_image_rejection_error(&anyhow::anyhow!(
            "Invalid image. The model does not support vision input."
        )));
        // Generic multimodal rejection
        assert!(is_image_rejection_error(&anyhow::anyhow!(
            "multimodal content is not supported by this endpoint"
        )));
        // Case-insensitive
        assert!(is_image_rejection_error(&anyhow::anyhow!(
            "Vision capability not available"
        )));
    }

    #[test]
    fn test_is_image_rejection_error_no_false_positives() {
        assert!(!is_image_rejection_error(&anyhow::anyhow!(
            "rate limit exceeded"
        )));
        assert!(!is_image_rejection_error(&anyhow::anyhow!(
            "prompt is too long"
        )));
        assert!(!is_image_rejection_error(&anyhow::anyhow!(
            "502 bad gateway"
        )));
        // "image" alone without support/invalid context should not match
        assert!(!is_image_rejection_error(&anyhow::anyhow!(
            "failed to load image/png from request body"
        )));
    }
}
