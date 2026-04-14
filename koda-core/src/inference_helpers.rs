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

/// Synthetic assistant message injected between consecutive user-side messages.
///
/// Inserted in-memory by [`assemble_messages`] — never written to the DB.
/// When Ctrl+C interrupts an inference turn the assistant message never lands,
/// leaving the history ending on a user or tool message. The next user message
/// (e.g. "continue") then produces back-to-back user-side turns that all three
/// providers reject as invalid role alternation. The sentinel bridges the gap
/// so the provider sees `user → assistant → user` regardless of where the
/// interruption occurred. Self-correcting: once the model replies for real,
/// the sentinel is no longer needed and disappears on the next load.
pub const INTERRUPTED_TURN_SENTINEL: &str = "[Turn interrupted — pick up from where you left off.]";

/// Assemble messages from DB history into ChatMessage vec.
///
/// Injects a synthetic `assistant` sentinel between any consecutive user-side
/// messages (`user` or `tool` followed by a plain `user`). This repairs broken
/// role alternation caused by an interrupted turn without touching the DB.
/// All three providers (Anthropic, Gemini, OpenAI-compat) require alternating
/// roles; Anthropic and Gemini remap `tool` → user-role internally, so the
/// `tool → user` case is equally broken without the sentinel (#875).
pub fn assemble_messages(
    system_message: &ChatMessage,
    history: &[crate::db::Message],
) -> Vec<ChatMessage> {
    let mut messages = vec![system_message.clone()];

    for msg in history {
        let role = msg.role.as_str();
        let is_plain_user = role == "user" && msg.tool_call_id.is_none();

        // If a plain user message would immediately follow another user-side
        // message, the provider will reject the request. Insert a sentinel
        // assistant message to restore valid alternation (#875).
        if is_plain_user {
            let prev_is_user_side = messages
                .last()
                .is_some_and(|p| p.role == "user" || p.role == "tool");
            if prev_is_user_side {
                messages.push(ChatMessage::text("assistant", INTERRUPTED_TURN_SENTINEL));
            }
        }

        let tool_calls: Option<Vec<ToolCall>> = msg
            .tool_calls
            .as_deref()
            .and_then(|tc| serde_json::from_str(tc).ok());
        messages.push(ChatMessage {
            role: role.to_string(),
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
        || (msg.contains("vision")
            && (msg.contains("support") || msg.contains("not") || msg.contains("unavailable")))
        || (msg.contains("multimodal")
            && (msg.contains("support") || msg.contains("not") || msg.contains("unavailable")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::{Message, Role};

    /// Build a bare `Message` for unit tests — mirrors the helper in db/tests.rs.
    fn msg(role: &str, content: Option<&str>, tool_call_id: Option<&str>) -> Message {
        Message {
            id: 0,
            session_id: String::new(),
            role: role.parse().unwrap_or(Role::User),
            content: content.map(Into::into),
            full_content: None,
            tool_calls: None,
            tool_call_id: tool_call_id.map(Into::into),
            prompt_tokens: None,
            completion_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            thinking_tokens: None,
            thinking_content: None,
            created_at: None,
        }
    }

    fn system() -> ChatMessage {
        ChatMessage::text("system", "You are helpful.")
    }

    // ── assemble_messages sentinel injection (#875) ───────────────────────────

    /// Clean conversation — no interruption, no sentinel ever injected.
    #[test]
    fn no_sentinel_for_clean_conversation() {
        let history = vec![
            msg("user", Some("hello"), None),
            msg("assistant", Some("hi!"), None),
            msg("user", Some("refactor X"), None),
            msg("assistant", Some("done"), None),
        ];
        let out = assemble_messages(&system(), &history);
        // system + 4 history messages — sentinel would make it 6
        assert_eq!(out.len(), 5, "no sentinel expected; got {out:?}");
        assert!(
            out.iter()
                .all(|m| m.content.as_deref() != Some(INTERRUPTED_TURN_SENTINEL)),
            "sentinel must not appear in clean conversation",
        );
    }

    /// Ctrl+C during streaming: last DB message is `user`, then user says
    /// "continue" — two consecutive `user` messages need the sentinel.
    #[test]
    fn sentinel_injected_for_user_after_user() {
        let history = vec![
            msg("user", Some("refactor X"), None),
            // no assistant reply (Ctrl+C filtered it out)
            msg("user", Some("continue"), None),
        ];
        let out = assemble_messages(&system(), &history);
        // system + user + sentinel + user(continue)
        assert_eq!(out.len(), 4, "expected sentinel; got {out:?}");
        assert_eq!(out[2].role, "assistant");
        assert_eq!(out[2].content.as_deref(), Some(INTERRUPTED_TURN_SENTINEL));
        assert_eq!(out[3].content.as_deref(), Some("continue"));
    }

    /// Ctrl+C during tool execution: DB ends with a `tool` result, then user
    /// says "continue". Anthropic + Gemini both remap tool→user-side, so the
    /// sentinel is required for all three providers.
    #[test]
    fn sentinel_injected_for_user_after_tool_result() {
        let history = vec![
            msg("user", Some("read the file"), None),
            msg("assistant", Some("sure"), None),
            msg("tool", Some("file contents"), Some("tc_1")),
            // assistant never processed the result (Ctrl+C)
            msg("user", Some("continue"), None),
        ];
        let out = assemble_messages(&system(), &history);
        // system + user + assistant + tool + sentinel + user(continue)
        assert_eq!(
            out.len(),
            6,
            "expected sentinel after tool result; got {out:?}"
        );
        assert_eq!(out[4].role, "assistant");
        assert_eq!(out[4].content.as_deref(), Some(INTERRUPTED_TURN_SENTINEL));
    }

    /// Tool result immediately following an assistant message is valid — no sentinel.
    #[test]
    fn no_sentinel_before_tool_result() {
        let history = vec![
            msg("user", Some("read it"), None),
            msg("assistant", Some("ok"), None),
            msg("tool", Some("contents"), Some("tc_1")),
        ];
        let out = assemble_messages(&system(), &history);
        assert_eq!(out.len(), 4);
        assert!(
            out.iter().all(|m| m.role != "assistant"
                || m.content.as_deref() != Some(INTERRUPTED_TURN_SENTINEL))
        );
    }

    /// Multiple tool results back-to-back — no sentinel between them.
    #[test]
    fn no_sentinel_between_consecutive_tool_results() {
        let history = vec![
            msg("user", Some("do stuff"), None),
            msg("assistant", Some("calling tools"), None),
            msg("tool", Some("r1"), Some("tc_1")),
            msg("tool", Some("r2"), Some("tc_2")),
        ];
        let out = assemble_messages(&system(), &history);
        assert_eq!(out.len(), 5); // system + 4 messages, no sentinel
    }

    /// First user message follows system — system is not user-side, so no sentinel.
    #[test]
    fn no_sentinel_for_first_user_message() {
        let history = vec![msg("user", Some("hello"), None)];
        let out = assemble_messages(&system(), &history);
        assert_eq!(out.len(), 2); // system + user
        assert_eq!(out[1].role, "user");
    }

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
        // Anthropic — model does not support vision (#819)
        assert!(is_image_rejection_error(&anyhow::anyhow!(
            "400 Bad Request: Images are not supported for this model"
        )));
        // Anthropic — invalid image data (base64 corruption, wrong format)
        assert!(is_image_rejection_error(&anyhow::anyhow!(
            "400 Bad Request: Invalid image: unable to decode image data"
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
        // Bare "vision" or "multimodal" without denial context → no match
        assert!(!is_image_rejection_error(&anyhow::anyhow!(
            "Invalid API key for vision endpoint"
        )));
        assert!(!is_image_rejection_error(&anyhow::anyhow!(
            "multimodal endpoint rate limit"
        )));
    }
}
