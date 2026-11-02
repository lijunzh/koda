//! Typed provider failures.
//!
//! An HTTP failure is classified once, here, from its status, headers and
//! body. The retry and recovery code in `inference.rs` then matches on
//! `ProviderErrorKind` instead of searching the message. Overflow and
//! image rejection still come from the body text, since providers state them
//! no other way; the text matching lives only in this file.
//!
//! The message keeps the shape `"<provider> API returned <status>: <body>"`.

use reqwest::header::HeaderMap;
use std::time::Duration;

/// What went wrong, as far as the caller needs to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorKind {
    /// 429 or Anthropic's 529: the same request can work after a wait.
    RateLimited,
    /// Any other 5xx.
    ServerError,
    /// No HTTP response came back: timeout, reset, DNS, TLS.
    Network,
    /// The request is too large for the model's window.
    ContextOverflow,
    /// The model or endpoint refuses image input.
    ImageRejected,
    /// Anything else, such as auth, a bad request, or an exhausted quota.
    /// Sending the same request again won't help.
    Other,
}

impl ProviderErrorKind {
    /// Whether the same request is worth sending again after a wait.
    pub fn is_retryable(self) -> bool {
        matches!(self, Self::RateLimited | Self::ServerError | Self::Network)
    }
}

/// A failed provider request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderError {
    /// What went wrong.
    pub kind: ProviderErrorKind,
    /// The wait the server asked for, if it named one.
    pub retry_after: Option<Duration>,
    /// The model's window, if an overflow error stated it.
    pub context_limit: Option<usize>,
    message: String,
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ProviderError {}

impl ProviderError {
    /// Classify a non-2xx response. `status_text` is what the message shows,
    /// such as `503 Service Unavailable`.
    pub fn from_response(
        provider_name: &str,
        status: u16,
        status_text: &str,
        headers: &HeaderMap,
        body: &str,
    ) -> Self {
        let message = format!("{provider_name} API returned {status_text}: {body}");
        let kind = classify(status, body);
        Self {
            kind,
            retry_after: retry_after(headers),
            context_limit: (kind == ProviderErrorKind::ContextOverflow)
                .then(|| context_limit(body))
                .flatten(),
            message,
        }
    }

    /// A request that got no HTTP response.
    pub fn network(message: String) -> Self {
        Self {
            kind: ProviderErrorKind::Network,
            retry_after: None,
            context_limit: None,
            message,
        }
    }

    /// A request that could not be built, so it never left.
    pub fn request(message: String) -> Self {
        Self {
            kind: ProviderErrorKind::Other,
            retry_after: None,
            context_limit: None,
            message,
        }
    }

    /// The provider failure behind `err`, if it is one.
    pub fn of(err: &anyhow::Error) -> Option<&Self> {
        err.chain().find_map(|cause| cause.downcast_ref())
    }

    /// The kind of `err`, treating anything else as [`ProviderErrorKind::Other`].
    pub fn kind_of(err: &anyhow::Error) -> ProviderErrorKind {
        Self::of(err).map_or(ProviderErrorKind::Other, |e| e.kind)
    }
}

/// Whether a failed stream is final, so reconnecting can't help.
///
/// A stream fails with a message, not a status: a dropped connection, a
/// stream that stopped short, or the provider's in-band error. All of those
/// are worth another try except a rejection of the request itself, which
/// would come back the same. Anthropic's in-band error types and OpenAI's
/// codes are the ones named here; Codex stops on the same set.
pub fn stream_error_is_final(message: &str) -> bool {
    let msg = message.to_lowercase();
    is_overflow(&msg)
        || is_quota_exhausted(&msg)
        || msg.contains("authentication_error")
        || msg.contains("permission_error")
        || msg.contains("invalid_request_error")
        || msg.contains("not_found_error")
        || msg.contains("invalid_api_key")
        || msg.contains("model_not_found")
        || msg.contains("invalid_prompt")
}

fn classify(status: u16, body: &str) -> ProviderErrorKind {
    let body = body.to_lowercase();
    match status {
        429 | 529 if is_quota_exhausted(&body) => ProviderErrorKind::Other,
        429 | 529 => ProviderErrorKind::RateLimited,
        413 => ProviderErrorKind::ContextOverflow,
        // Not on a bare 5xx. LM Studio answers an overflow with a bare `500`
        // HTML page, and the same page for failures that aren't about size.
        // Compacting on it would throw away history for nothing. Codex and
        // Claude Code draw the same line. Koda avoids that overflow by
        // counting against the window LM Studio loaded
        // (`loaded_context_length`, see `providers::openai_compat`).
        _ if is_overflow(&body) => ProviderErrorKind::ContextOverflow,
        500..=599 => ProviderErrorKind::ServerError,
        _ if is_image_rejection(&body) => ProviderErrorKind::ImageRejected,
        _ => ProviderErrorKind::Other,
    }
}

/// The billing limit, not a throttle: it resets in hours, if at all.
fn is_quota_exhausted(body: &str) -> bool {
    body.contains("insufficient_quota") || body.contains("billing")
}

/// Common patterns across providers:
/// - Anthropic: "prompt is too long", "input is too long"
/// - OpenAI: "maximum context length exceeded", "context_length_exceeded"
/// - llama.cpp: "exceeds the available context size" (`exceed_context_size_error`)
fn is_overflow(body: &str) -> bool {
    body.contains("too long")
        || body.contains("context_length_exceeded")
        || body.contains("maximum context length")
        || body.contains("token limit")
        || body.contains("exceeds the model")
        || body.contains("request too large")
        || body.contains("exceeds the available context size")
        || body.contains("exceed_context_size_error")
}

/// The documented rejection messages from OpenAI-compatible servers (LM
/// Studio, Ollama), the OpenAI API, and Gemini. "image" alone is too broad,
/// so it needs a support-denial word beside it.
fn is_image_rejection(body: &str) -> bool {
    let denied = body.contains("support") || body.contains("not") || body.contains("unavailable");
    (body.contains("image") && (body.contains("support") || body.contains("invalid")))
        || (body.contains("vision") && denied)
        || (body.contains("multimodal") && denied)
}

/// The server's context window, when its overflow error states one:
/// - llama.cpp: `{"error":{"type":"exceed_context_size_error","n_ctx":4096,…}}`
/// - OpenAI and vLLM: "This model's maximum context length is 8192 tokens."
///
/// `None` when no limit is stated (Anthropic's "prompt is too long",
/// LM Studio) or the number is zero.
fn context_limit(body: &str) -> Option<usize> {
    static PATTERNS: std::sync::LazyLock<[regex::Regex; 3]> = std::sync::LazyLock::new(|| {
        [
            regex::Regex::new(r#""n_ctx"\s*:\s*(\d+)"#).unwrap(),
            regex::Regex::new(r"maximum context length is (\d+)").unwrap(),
            regex::Regex::new(r"available context size \((\d+) tokens\)").unwrap(),
        ]
    });
    PATTERNS
        .iter()
        .find_map(|re| re.captures(body)?.get(1)?.as_str().parse::<usize>().ok())
        .filter(|&n| n > 0)
}

/// `retry-after-ms` (OpenAI), then `retry-after` in seconds. The HTTP-date
/// form of `retry-after` is ignored, and the caller's backoff applies.
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let number = |name: &str| {
        headers
            .get(name)?
            .to_str()
            .ok()?
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite() && *n >= 0.0)
    };
    number("retry-after-ms")
        .map(|ms| Duration::from_secs_f64(ms / 1000.0))
        .or_else(|| number("retry-after").map(Duration::from_secs_f64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ProviderErrorKind::*;

    fn error(status: u16, body: &str) -> ProviderError {
        ProviderError::from_response("LLM", status, &status.to_string(), &HeaderMap::new(), body)
    }

    #[test]
    fn the_status_decides_where_it_can() {
        assert_eq!(error(429, "slow down").kind, RateLimited);
        assert_eq!(error(529, "overloaded").kind, RateLimited);
        for status in [500, 502, 503, 504, 520] {
            assert_eq!(error(status, "").kind, ServerError, "{status}");
        }
        assert_eq!(error(413, "").kind, ContextOverflow);
        for status in [400, 401, 403, 404] {
            assert_eq!(error(status, "nope").kind, Other, "{status}");
        }
    }

    #[test]
    fn a_number_in_the_body_is_not_a_status() {
        // Matching text for `500` read these as server errors.
        assert_eq!(error(400, "max_tokens must be under 8500").kind, Other);
        assert_eq!(
            error(401, "request 5002 to http://127.0.0.1:14290 denied").kind,
            Other
        );
    }

    #[test]
    fn an_exhausted_quota_is_not_retried() {
        assert_eq!(error(429, r#"{"code":"insufficient_quota"}"#).kind, Other);
        assert_eq!(error(429, "check your billing details").kind, Other);
    }

    #[test]
    fn overflow_is_read_from_the_body() {
        for body in [
            "prompt is too long",
            "context_length_exceeded: max 200000 tokens",
            "maximum context length exceeded",
            "request exceeds the model's input limit",
        ] {
            assert_eq!(error(400, body).kind, ContextOverflow, "{body}");
        }
    }

    /// llama-server's body for a 6,052-token request against `-c 4096`.
    const LLAMA_CPP_OVERFLOW: &str = r#"{"error":{"code":400,"message":"request (6052 tokens) exceeds the available context size (4096 tokens), try increasing it","type":"exceed_context_size_error","n_prompt_tokens":6052,"n_ctx":4096}}"#;

    #[test]
    fn the_context_limit_comes_from_an_overflow() {
        let cases = [
            (LLAMA_CPP_OVERFLOW, Some(4096)),
            (
                "request (6052 tokens) exceeds the available context size (8192 tokens)",
                Some(8192),
            ),
            (
                "This model's maximum context length is 32768 tokens. However, you requested more",
                Some(32768),
            ),
            ("prompt is too long", None),
            ("context_length_exceeded", None),
            (r#"context_length_exceeded {"n_ctx":0}"#, None),
        ];
        for (body, want) in cases {
            let err = error(400, body);
            assert_eq!(err.kind, ContextOverflow, "{body}");
            assert_eq!(err.context_limit, want, "{body}");
        }
    }

    #[test]
    fn lm_studio_bare_500_is_a_server_error_not_an_overflow() {
        let page = "<!DOCTYPE html><html><body><pre>Internal Server Error</pre></body></html>";
        let err = error(500, page);
        assert_eq!(err.kind, ServerError);
        assert_eq!(err.context_limit, None);
    }

    #[test]
    fn an_image_rejection_is_read_from_the_body() {
        for body in [
            "This model does not support image input",
            "image input is not supported",
            "vision is not supported on this model",
            "multimodal content is not supported",
        ] {
            assert_eq!(error(400, body).kind, ImageRejected, "{body}");
        }
    }

    #[test]
    fn the_message_keeps_its_shape() {
        let err = ProviderError::from_response(
            "Gemini",
            503,
            "503 Service Unavailable",
            &HeaderMap::new(),
            "busy",
        );
        assert_eq!(
            err.to_string(),
            "Gemini API returned 503 Service Unavailable: busy"
        );
    }

    #[test]
    fn retry_after_is_read_from_the_headers() {
        let delay = |name: &'static str, value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(name, value.parse().unwrap());
            ProviderError::from_response("LLM", 429, "429", &headers, "").retry_after
        };
        assert_eq!(delay("retry-after", "7"), Some(Duration::from_secs(7)));
        assert_eq!(
            delay("retry-after", "1.5"),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(
            delay("retry-after-ms", "250"),
            Some(Duration::from_millis(250))
        );
        assert_eq!(delay("retry-after", "Wed, 21 Oct 2026 07:28:00 GMT"), None);
        assert_eq!(delay("retry-after", "-1"), None);
    }

    #[test]
    fn of_finds_the_error_through_context() {
        use anyhow::Context;
        let err: anyhow::Result<()> = Err(error(503, "busy").into());
        let err = err.context("LLM inference failed").unwrap_err();
        assert_eq!(ProviderError::kind_of(&err), ServerError);
        assert_eq!(ProviderError::kind_of(&anyhow::anyhow!("500 boom")), Other);
    }

    #[test]
    fn only_a_rejected_request_is_final_in_a_stream() {
        for message in [
            "Anthropic stream error: overloaded_error: Overloaded",
            "Anthropic stream error: api_error: boom",
            "Anthropic stream ended before message_stop",
            "OpenAI-compatible stream ended before a finish_reason or [DONE]",
            "OpenAI-compatible stream error: worker failed",
            "error decoding response body",
        ] {
            assert!(!stream_error_is_final(message), "{message}");
        }
        for message in [
            "Anthropic stream error: authentication_error: bad key",
            "Anthropic stream error: invalid_request_error: nope",
            "OpenAI-compatible stream error: context_length_exceeded",
            "OpenAI-compatible stream error: insufficient_quota",
        ] {
            assert!(stream_error_is_final(message), "{message}");
        }
    }
}
