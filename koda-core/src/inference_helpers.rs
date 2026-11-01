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
            // Reasoning is replayed on assistant turns (OpenAI-compatible
            // providers), so it fills the window like any other content. The
            // Anthropic and Gemini paths don't send it; counting it there
            // errs on the safe, over-estimating side.
            let reasoning_len = m.reasoning.as_deref().map_or(0, |r| r.len());
            ((content_len + tc_len + reasoning_len) as f64 / CHARS_PER_TOKEN) as usize
                + PER_MESSAGE_OVERHEAD
        })
        .sum()
}

/// What a request is made of, in characters. `estimate_tokens` gives the
/// total; this says which part of it moved.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ContextShape {
    pub system_chars: usize,
    pub user_chars: usize,
    pub assistant_chars: usize,
    pub tool_chars: usize,
    /// `tool_calls` as JSON. Gemini's thought signatures live here.
    pub tool_calls_chars: usize,
    pub reasoning_chars: usize,
    pub images: usize,
    /// Index, role and size of the message with the most characters.
    pub largest: Option<(usize, String, usize)>,
}

impl ContextShape {
    pub(crate) fn of(messages: &[ChatMessage]) -> Self {
        let mut shape = Self::default();
        for (index, message) in messages.iter().enumerate() {
            let content = message.content.as_deref().map_or(0, str::len);
            let calls = message
                .tool_calls
                .as_ref()
                .map_or(0, |tc| serde_json::to_string(tc).map_or(0, |s| s.len()));
            let reasoning = message.reasoning.as_deref().map_or(0, str::len);
            match message.role.as_str() {
                "system" => shape.system_chars += content,
                "user" => shape.user_chars += content,
                "tool" => shape.tool_chars += content,
                _ => shape.assistant_chars += content,
            }
            shape.tool_calls_chars += calls;
            shape.reasoning_chars += reasoning;
            shape.images += message.images.as_ref().map_or(0, Vec::len);
            let chars = content + calls + reasoning;
            if shape.largest.as_ref().is_none_or(|l| chars > l.2) {
                shape.largest = Some((index, message.role.clone(), chars));
            }
        }
        shape
    }
}

/// Pre-flight context-budget breakdown for a single sub-agent invocation.
///
/// **#1232 §3a**: prior to this check, sub-agent dispatch could fire an
/// LLM call that the model rejected with a raw `400 "Context size has been
/// exceeded"` from upstream — leaving the user with no actionable hint.
/// This pre-flight estimate runs *before* the first provider call and lets
/// the dispatcher bail with a useful breakdown instead.
///
/// Token counts are heuristic estimates using the same `chars / 3.5 + overhead`
/// model the live token gauge uses (see [`estimate_tokens`]); calibrated to
/// over-estimate slightly, which is the safe direction for a pre-flight gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreflightTokenBudget {
    /// Estimated tokens for the rendered system prompt.
    pub system_prompt_tokens: usize,
    /// Estimated tokens for the serialized tool definitions array.
    pub tool_defs_tokens: usize,
    /// Estimated tokens for the user-supplied prompt (the body of the
    /// `InvokeAgent` call).
    pub user_prompt_tokens: usize,
    /// `system + tools + user`. Rough lower bound on what the first turn
    /// will send to the provider.
    pub total_tokens: usize,
    /// `max_context_tokens` from the sub-agent's resolved config.
    pub limit_tokens: usize,
}

impl PreflightTokenBudget {
    /// Whether the estimated total exceeds the configured budget.
    pub fn is_over_budget(&self) -> bool {
        self.total_tokens > self.limit_tokens
    }

    /// Render the breakdown as a single-line, human-readable summary
    /// suitable for both error messages and debug logging. Numbers are
    /// rounded to 0.1k for readability — the underlying estimates are
    /// heuristic, so additional precision would be false confidence.
    pub fn summary(&self) -> String {
        format!(
            "system={}k + tools={}k + prompt={}k = {}k / limit {}k",
            (self.system_prompt_tokens as f64 / 1000.0).round() as usize,
            (self.tool_defs_tokens as f64 / 1000.0).round() as usize,
            (self.user_prompt_tokens as f64 / 1000.0).round() as usize,
            (self.total_tokens as f64 / 1000.0).round() as usize,
            (self.limit_tokens as f64 / 1000.0).round() as usize,
        )
    }
}

/// Estimate the first-turn token cost of dispatching a sub-agent before
/// the LLM call is made.
///
/// Inputs mirror what `sub_agent_dispatch::execute_sub_agent` already has
/// in scope:
///   * `system_prompt` — the result of `build_system_prompt(...)`.
///   * `tool_defs` — the filtered tool list from `tools.get_definitions(...)`.
///   * `user_prompt` — the `prompt` argument from the `InvokeAgent` call.
///   * `limit_tokens` — `sub_config.max_context_tokens`.
///
/// The breakdown intentionally does NOT account for the (empty) initial
/// transcript, persisted history (sub-agents start fresh), or the response
/// budget. The goal is a fast, conservative pre-flight signal — not a
/// perfect simulation of the wire payload.
pub fn estimate_subagent_preflight(
    system_prompt: &str,
    tool_defs: &[crate::providers::ToolDefinition],
    user_prompt: &str,
    limit_tokens: usize,
) -> PreflightTokenBudget {
    let system_prompt_tokens = (system_prompt.len() as f64 / CHARS_PER_TOKEN) as usize
        + PER_MESSAGE_OVERHEAD
        + SYSTEM_PROMPT_OVERHEAD;

    // Tool defs travel as JSON; the serialized form is the wire-cost proxy.
    // serde_json::to_string failures are vanishingly unlikely for our owned
    // types, but treat any failure as zero rather than panicking — a
    // pre-flight that crashes is strictly worse than a pre-flight that
    // under-estimates by a few k tokens.
    let tool_defs_chars = serde_json::to_string(tool_defs)
        .map(|s| s.len())
        .unwrap_or(0);
    let tool_defs_tokens = (tool_defs_chars as f64 / CHARS_PER_TOKEN) as usize;

    let user_prompt_tokens =
        (user_prompt.len() as f64 / CHARS_PER_TOKEN) as usize + PER_MESSAGE_OVERHEAD;

    let total_tokens = system_prompt_tokens + tool_defs_tokens + user_prompt_tokens;

    PreflightTokenBudget {
        system_prompt_tokens,
        tool_defs_tokens,
        user_prompt_tokens,
        total_tokens,
        limit_tokens,
    }
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

/// Follows the compaction summary, which reaches the model as a user message.
const COMPACTION_CONTINUATION_HINT: &str = "Your context was compacted. The text above summarizes \
    our earlier conversation. Do not mention the summary or that compaction occurred. Continue \
    the conversation naturally based on the summarized context.";

/// The user message that carries a session's compaction summaries, if any.
///
/// Summaries are the only `system` rows in the history.
fn compaction_message(history: &[crate::db::Message]) -> Option<String> {
    let summaries: Vec<&str> = history
        .iter()
        .filter(|m| m.role == crate::persistence::Role::System)
        .filter_map(|m| m.content.as_deref())
        .collect();
    (!summaries.is_empty()).then(|| {
        format!(
            "{}\n\n{COMPACTION_CONTINUATION_HINT}",
            summaries.join("\n\n")
        )
    })
}

/// Assemble messages from DB history into ChatMessage vec.
///
/// A compaction summary goes first, as a `user` message: it covers everything
/// older than the preserved tail, and only the leading system prompt reaches
/// the model as `system`. Anthropic would drop a later one and Gemini would let
/// it replace the system prompt (#1444). When the tail opens with a user
/// message the two are joined, because no reply sits between them.
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
    let mut summary = compaction_message(history);

    for msg in history
        .iter()
        .filter(|m| m.role != crate::persistence::Role::System)
    {
        let role = msg.role.as_str();
        let is_plain_user = role == "user" && msg.tool_call_id.is_none();

        if let Some(text) = summary.take() {
            if is_plain_user {
                let prompt = msg.content.as_deref().unwrap_or_default();
                messages.push(ChatMessage::text("user", &format!("{text}\n\n{prompt}")));
                continue;
            }
            messages.push(ChatMessage::text("user", &text));
        }

        if is_plain_user {
            repair_alternation(&mut messages, INTERRUPTED_TURN_SENTINEL);
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
            reasoning: msg.thinking_content.clone(),
        });
    }

    // Nothing but the summary in the history.
    if let Some(text) = summary {
        messages.push(ChatMessage::text("user", &text));
    }

    messages
}

/// Make room for a plain `user` message at the end of `messages`.
///
/// If a plain user message would immediately follow another user-side message,
/// the provider will reject the request. Insert `bridge` as an assistant message
/// to restore valid alternation (#875).
pub(crate) fn repair_alternation(messages: &mut Vec<ChatMessage>, bridge: &str) {
    let prev_is_user_side = messages
        .last()
        .is_some_and(|p| p.role == "user" || p.role == "tool");
    if prev_is_user_side {
        messages.push(ChatMessage::text("assistant", bridge));
    }
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
    // Bare numeric HTTP codes need word-boundary checks: a substring
    // match like `contains("429")` false-positives on ephemeral ports
    // (e.g. wiremock binding 127.0.0.1:14290 makes a totally-unrelated
    // timeout error look rate-limited). The original predicate caused
    // a Linux-only flake in `read_timeout_triggers_auto_retry_not_hard_failure`
    // because Linux's ephemeral port range overlaps the danger zone
    // ~1% of the time. We require the code to appear in a textual
    // status-line context (`status: 429`, `429 too many`, ` 429 `).
    let has_status_429 = has_http_status(&msg, "429");
    let has_status_529 = has_http_status(&msg, "529"); // Anthropic: API overloaded
    has_status_429
        || has_status_529
        || msg.contains("rate limit")
        || msg.contains("rate_limit")
        || msg.contains("too many requests")
        || msg.contains("quota exceeded")
        || msg.contains("overloaded") // Anthropic overload text
        // Gemini under load; a local server still loading a model says 503 too.
        || has_http_status(&msg, "503")
        || msg.contains("high demand")
}

/// One finished model call, for the log. Holds no conversation text.
pub(crate) struct ModelCall<'a> {
    /// What the call was for: `turn` or `compaction`.
    pub purpose: &'static str,
    /// The provider's name.
    pub provider: &'a str,
    /// The model id as requested.
    pub model: &'a str,
    /// Messages sent.
    pub messages: usize,
    /// Koda's estimate of the prompt, to compare with `usage.prompt_tokens`.
    pub estimated_tokens: usize,
    /// Time to the first stream chunk, for streamed calls.
    pub first_chunk: Option<std::time::Duration>,
    /// Time for the whole call.
    pub elapsed: std::time::Duration,
    /// Usage as the provider reported it.
    pub usage: &'a crate::providers::TokenUsage,
    /// Tool calls in the reply.
    pub tool_calls: usize,
    /// Characters of text in the reply.
    pub text_chars: usize,
}

/// Whether a reply ends by asking something. A cheap heuristic for telling a
/// turn that finished from one that stopped to ask: a question mark at the end
/// of the text, past any closing markdown.
pub(crate) fn ends_with_question(text: &str) -> bool {
    text.trim_end_matches(|c: char| c.is_whitespace() || "*_`".contains(c))
        .ends_with('?')
}

impl ModelCall<'_> {
    /// Write the `model call` line every run gets, whatever its log level.
    pub(crate) fn log(&self) {
        tracing::info!(
            purpose = self.purpose,
            provider = self.provider,
            model = self.model,
            messages = self.messages,
            estimated_tokens = self.estimated_tokens,
            first_chunk_ms = self.first_chunk.map(|d| d.as_millis() as u64),
            elapsed_ms = self.elapsed.as_millis() as u64,
            prompt_tokens = self.usage.prompt_tokens,
            completion_tokens = self.usage.completion_tokens,
            cache_read_tokens = self.usage.cache_read_tokens,
            thinking_tokens = self.usage.thinking_tokens,
            stop_reason = %self.usage.stop_reason,
            failed_stop = is_failed_stop(&self.usage.stop_reason),
            tool_calls = self.tool_calls,
            text_chars = self.text_chars,
            "model call"
        );
    }
}

/// Whether a provider stop reason means the model failed to answer, rather
/// than finished or hit its token cap. Gemini reports these as a normal reply
/// with no content, so they need the reason to be told from a quiet model.
pub fn is_failed_stop(stop_reason: &str) -> bool {
    matches!(
        stop_reason,
        "malformed_function_call"
            | "unexpected_tool_call"
            | "too_many_tool_calls"
            | "missing_thought_signature"
            | "malformed_response"
            | "safety"
            | "recitation"
            | "language"
            | "blocklist"
            | "prohibited_content"
            | "spii"
            | "image_safety"
            | "other"
    )
}

/// True iff `code` appears in `msg` (already lowercased) in a context
/// that's plausibly an HTTP status code rather than (e.g.) a port
/// number embedded in a URL. Cheap heuristic: look for the canonical
/// shapes — `"status: 429"`, `"429 too many"`, or surrounded by ASCII
/// whitespace / punctuation that bare port numbers don't produce.
fn has_http_status(msg: &str, code: &str) -> bool {
    // Common shapes from reqwest / hyper / openai / anthropic.
    if msg.contains(&format!("status: {code}"))
        || msg.contains(&format!("status code: {code}"))
        || msg.contains(&format!("http {code}"))
        || msg.contains(&format!("{code} too many"))
        || msg.contains(&format!("{code} overloaded"))
        || msg.contains(&format!("\"{code}\""))
    {
        return true;
    }
    // Generic word-boundary fallback: `code` flanked by non-digit
    // characters on both sides. Rules out port numbers (`:14290`,
    // `42935/`) and other digit-runs that happen to contain `code`.
    msg.split(code).enumerate().any(|(i, segment)| {
        if i == 0 {
            return false;
        }
        let left_ok = msg[..msg.len() - segment.len() - code.len()]
            .chars()
            .last()
            .is_none_or(|c| !c.is_ascii_digit());
        let right_ok = segment.chars().next().is_none_or(|c| !c.is_ascii_digit());
        left_ok && right_ok
    })
}

/// Maximum number of retries for rate-limited requests.
pub const RATE_LIMIT_MAX_RETRIES: u32 = 5;

/// Detect if an error is a transient network/transport failure that's worth
/// auto-retrying with backoff.
///
/// Covers low-level network conditions that don't surface as HTTP status
/// codes: idle-read timeouts, dropped half-open sockets, broken pipes,
/// short-lived DNS / TLS hiccups, etc. These are NOT recoverable via
/// context compaction (so they don't belong in `is_context_overflow_error`)
/// and they're NOT rate-limit signals (so they don't belong in
/// `is_rate_limit_error`), but they ARE worth retrying because the typical
/// cause is a stale TCP connection or a transiently-flaky network rather
/// than a permanent failure.
///
/// String patterns mirror the `RETRYABLE_NETWORK_CODES` list maintained by
/// gemini-cli (`packages/core/src/utils/retry.ts`), translated into the
/// human-readable forms that `reqwest` / `anyhow` emit on the Rust side.
/// See issue #1119 for the comparative analysis.
///
/// # Examples
///
/// ```
/// use koda_core::inference_helpers::is_network_transient_error;
///
/// assert!(is_network_transient_error(&anyhow::anyhow!("operation timed out")));
/// assert!(is_network_transient_error(&anyhow::anyhow!("connection reset by peer")));
/// assert!(is_network_transient_error(&anyhow::anyhow!("broken pipe")));
/// assert!(!is_network_transient_error(&anyhow::anyhow!("401 Unauthorized")));
/// ```
pub fn is_network_transient_error(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}").to_lowercase();
    msg.contains("operation timed out")
        || msg.contains("timed out")
        || msg.contains("connection reset")
        || msg.contains("connection closed")
        || msg.contains("connection aborted")
        || msg.contains("connection refused")
        || msg.contains("broken pipe")
        || msg.contains("unexpected end of file")
        || msg.contains("unexpected eof")
        || msg.contains("error trying to connect")
        || msg.contains("dns error")
        || msg.contains("failed to lookup address")
        || msg.contains("tls handshake")
}

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
/// - llama.cpp: "exceeds the available context size" (`exceed_context_size_error`)
/// - Generic: HTTP 400/413 with size-related messages
///
/// Not an overflow: LM Studio's answer to an overflowed request, a bare
/// `500 Internal Server Error` HTML page (`<pre>Internal Server Error</pre>`)
/// with nothing in it about size. It is the same page LM Studio returns for
/// other failures: in the 2026-10-28 eval screens Flint got it three times
/// past its 65,536-token load, and Qwen3.8 once at 18,647 tokens. Compacting
/// on it would throw away history for a failure that may not be about size,
/// then fail the same way, so it stays a server error (`is_server_error`),
/// which ends the turn. Codex and Claude Code draw the same line: Codex
/// types `ContextWindowExceeded` from the API's `context_length_exceeded`
/// code and Claude Code matches "prompt is too long" or a 413; neither reads
/// overflow into a bare 5xx. Koda avoids the LM Studio overflow instead by
/// counting against the window it loaded (`loaded_context_length`, see
/// `providers::openai_compat`) and compacting at the pre-flight threshold.
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
        || msg.contains("exceeds the available context size")
        || msg.contains("exceed_context_size_error")
        || (msg.contains("413") && msg.contains("too large"))
}

/// The server's context window, when its overflow error states one.
///
/// Lets Koda learn the real limit from a rejected request instead of
/// trusting a guess the server never confirmed:
/// - llama.cpp: `{"error":{"type":"exceed_context_size_error","n_ctx":4096,…}}`
/// - OpenAI and vLLM: "This model's maximum context length is 8192 tokens."
///
/// Returns `None` when no limit is stated (Anthropic's "prompt is too long",
/// LM Studio) or the number is zero.
///
/// # Examples
///
/// ```
/// use koda_core::inference_helpers::context_limit_from_error;
///
/// let body = r#"LLM API returned 400: {"error":{"n_prompt_tokens":6052,"n_ctx":4096}}"#;
/// let e = anyhow::anyhow!("{body}");
/// assert_eq!(context_limit_from_error(&e), Some(4096));
/// assert_eq!(context_limit_from_error(&anyhow::anyhow!("prompt is too long")), None);
/// ```
pub fn context_limit_from_error(err: &anyhow::Error) -> Option<usize> {
    static PATTERNS: std::sync::LazyLock<[regex::Regex; 3]> = std::sync::LazyLock::new(|| {
        [
            regex::Regex::new(r#""n_ctx"\s*:\s*(\d+)"#).unwrap(),
            regex::Regex::new(r"maximum context length is (\d+)").unwrap(),
            regex::Regex::new(r"available context size \((\d+) tokens\)").unwrap(),
        ]
    });
    let msg = format!("{err:#}");
    PATTERNS
        .iter()
        .find_map(|re| re.captures(&msg)?.get(1)?.as_str().parse::<usize>().ok())
        .filter(|&n| n > 0)
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

    /// Every provider sees one `system` message and the summary ahead of the
    /// tail, whatever the tail opens with (#1444).
    #[test]
    fn compaction_summary_goes_first_as_user() {
        let history = vec![
            msg("assistant", Some("kept"), None),
            msg("user", Some("next"), None),
            msg("system", Some("SUMMARY"), None),
        ];
        let out = assemble_messages(&system(), &history);
        let roles: Vec<_> = out.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, ["system", "user", "assistant", "user"]);
        let text = out[1].content.as_deref().unwrap();
        assert!(text.starts_with("SUMMARY"), "{text}");
        assert!(text.contains("Do not mention the summary"), "{text}");
    }

    /// No synthetic reply between the summary and a tail that opens with a
    /// user message: the sentinel would claim a turn was interrupted.
    #[test]
    fn compaction_summary_joins_a_leading_user_message() {
        let history = vec![
            msg("user", Some("the request"), None),
            msg("system", Some("SUMMARY"), None),
        ];
        let out = assemble_messages(&system(), &history);
        let roles: Vec<_> = out.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, ["system", "user"]);
        let text = out[1].content.as_deref().unwrap();
        assert!(text.starts_with("SUMMARY"), "{text}");
        assert!(text.ends_with("the request"), "{text}");
    }

    #[test]
    fn compaction_summary_alone_and_repeated_summaries() {
        let out = assemble_messages(&system(), &[msg("system", Some("ONLY"), None)]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].role, "user");

        let history = vec![
            msg("system", Some("OLDER"), None),
            msg("assistant", Some("kept"), None),
            msg("system", Some("NEWER"), None),
        ];
        let out = assemble_messages(&system(), &history);
        assert_eq!(out.len(), 3);
        let text = out[1].content.as_deref().unwrap();
        assert!(text.starts_with("OLDER\n\nNEWER"), "{text}");
    }

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

    /// The persisted reasoning of an assistant turn rides along on the rebuilt
    /// history, so the OpenAI-compatible path can send it back.
    #[test]
    fn assistant_thinking_content_becomes_reasoning() {
        let mut assistant = msg("assistant", Some("hi!"), None);
        assistant.thinking_content = Some("greet the user".into());
        let history = vec![msg("user", Some("hello"), None), assistant];
        let out = assemble_messages(&system(), &history);
        assert_eq!(out[1].reasoning, None);
        assert_eq!(out[2].reasoning.as_deref(), Some("greet the user"));
    }

    /// Replayed reasoning takes up context, so the estimate must count it.
    #[test]
    fn context_shape_splits_by_role_and_names_the_largest() {
        let assistant = ChatMessage {
            reasoning: Some("r".repeat(5)),
            tool_calls: Some(vec![ToolCall {
                id: "1".into(),
                function_name: "Read".into(),
                arguments: "{}".into(),
                thought_signature: Some("s".repeat(500)),
            }]),
            ..ChatMessage::text("assistant", "ok")
        };
        let messages = [
            ChatMessage::text("system", "sys"),
            ChatMessage::text("user", "hello"),
            assistant.clone(),
            ChatMessage::text("tool", "result"),
        ];

        let shape = ContextShape::of(&messages);

        assert_eq!(shape.system_chars, 3);
        assert_eq!(shape.user_chars, 5);
        assert_eq!(shape.assistant_chars, 2);
        assert_eq!(shape.tool_chars, 6);
        assert_eq!(shape.reasoning_chars, 5);
        assert!(shape.tool_calls_chars > 500, "{shape:?}");
        let (index, role, chars) = shape.largest.unwrap();
        assert_eq!((index, role.as_str()), (2, "assistant"));
        assert_eq!(chars, 2 + 5 + shape.tool_calls_chars);
    }

    #[test]
    fn ends_with_question_sees_past_closing_markdown() {
        assert!(ends_with_question("Should I fix it?"));
        assert!(ends_with_question("**Should I fix it?**\n"));
        assert!(!ends_with_question("Fixed. Was it the lexer? Yes."));
        assert!(!ends_with_question(""));
    }

    #[test]
    fn estimate_tokens_counts_replayed_reasoning() {
        let plain = ChatMessage::text("assistant", "done");
        let with_reasoning = ChatMessage {
            reasoning: Some("x".repeat(3_500)),
            ..plain.clone()
        };
        let base = estimate_tokens(&[plain]);
        let more = estimate_tokens(&[with_reasoning]);
        assert!(more >= base + 900, "base {base}, with reasoning {more}");
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

    /// llama-server's body for a 6,052-token request against `-c 4096`.
    const LLAMA_CPP_OVERFLOW: &str = r#"LLM API returned 400 Bad Request: {"error":{"code":400,"message":"request (6052 tokens) exceeds the available context size (4096 tokens), try increasing it","type":"exceed_context_size_error","n_prompt_tokens":6052,"n_ctx":4096}}"#;

    #[test]
    fn context_limit_is_read_from_overflow_errors() {
        let cases = [
            (LLAMA_CPP_OVERFLOW, Some(4096)),
            // The message alone, without the JSON fields.
            (
                "request (6052 tokens) exceeds the available context size (8192 tokens)",
                Some(8192),
            ),
            (
                "This model's maximum context length is 32768 tokens. However, you requested 40000 tokens.",
                Some(32768),
            ),
            ("Anthropic API returned 400: prompt is too long", None),
            ("context_length_exceeded", None),
            (r#"{"n_ctx":0}"#, None),
        ];
        for (msg, want) in cases {
            assert_eq!(
                context_limit_from_error(&anyhow::anyhow!("{msg}")),
                want,
                "{msg}"
            );
        }
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
        // llama-server, verbatim from a real 400 on 2026-10-27.
        assert!(is_context_overflow_error(&anyhow::anyhow!(
            "{}",
            LLAMA_CPP_OVERFLOW
        )));

        // Should NOT match
        assert!(!is_context_overflow_error(&anyhow::anyhow!(
            "rate limit exceeded"
        )));
        assert!(!is_context_overflow_error(&anyhow::anyhow!(
            "connection refused"
        )));
    }

    /// LM Studio's answer to an overflowed request, verbatim from the
    /// 2026-10-28 Flint eval screen. The same page comes back for failures
    /// that are not about size, so it is a server error, not an overflow.
    const LM_STUDIO_BARE_500: &str = "LLM API returned 500 Internal Server Error: <!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<title>Error</title>\n</head>\n<body>\n<pre>Internal Server Error</pre>\n</body>\n</html>\n";

    #[test]
    fn lm_studio_bare_500_is_a_server_error_not_an_overflow() {
        let err = anyhow::anyhow!("{}", LM_STUDIO_BARE_500);
        assert!(!is_context_overflow_error(&err));
        assert_eq!(context_limit_from_error(&err), None);
        assert!(is_server_error(&err));
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

        // Regression: ephemeral port numbers must NOT trip the
        // bare-numeric matchers. wiremock's `MockServer` binds to
        // 127.0.0.1:<random> and Linux's range overlaps the danger
        // zone roughly 1% of the time — caused intermittent CI
        // failures in `read_timeout_triggers_auto_retry_not_hard_failure`
        // before the word-boundary fix.
        assert!(
            !is_rate_limit_error(&anyhow::anyhow!(
                "error sending request for url (http://127.0.0.1:14290/v1/chat/completions): \
                 operation timed out"
            )),
            "port containing 429 must NOT classify as rate-limit"
        );
        assert!(
            !is_rate_limit_error(&anyhow::anyhow!(
                "http://127.0.0.1:42935/path: connection reset by peer"
            )),
            "port containing 429 anywhere must NOT classify as rate-limit"
        );
        assert!(
            !is_rate_limit_error(&anyhow::anyhow!("http://127.0.0.1:5295/foo: timed out")),
            "port containing 529 must NOT classify as rate-limit"
        );
        // But a real status line with the port in it is still a hit.
        assert!(is_rate_limit_error(&anyhow::anyhow!(
            "POST http://api.example.com:14290/v1/chat returned status: 429"
        )));
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
    fn test_is_network_transient_error() {
        // Idle / read timeouts (the original #1119 trigger)
        assert!(is_network_transient_error(&anyhow::anyhow!(
            "error sending request: operation timed out"
        )));
        assert!(is_network_transient_error(&anyhow::anyhow!(
            "request timed out after 180s"
        )));

        // Connection-state failures
        assert!(is_network_transient_error(&anyhow::anyhow!(
            "connection reset by peer"
        )));
        assert!(is_network_transient_error(&anyhow::anyhow!(
            "connection closed before message completed"
        )));
        assert!(is_network_transient_error(&anyhow::anyhow!(
            "connection aborted"
        )));
        assert!(is_network_transient_error(&anyhow::anyhow!(
            "connection refused"
        )));
        assert!(is_network_transient_error(&anyhow::anyhow!("broken pipe")));

        // Premature stream termination
        assert!(is_network_transient_error(&anyhow::anyhow!(
            "unexpected end of file"
        )));
        assert!(is_network_transient_error(&anyhow::anyhow!(
            "unexpected EOF"
        )));

        // Connect-phase failures (caller may want to retry once before erroring)
        assert!(is_network_transient_error(&anyhow::anyhow!(
            "error trying to connect: tcp connect error"
        )));
        assert!(is_network_transient_error(&anyhow::anyhow!(
            "dns error: failed to lookup address information"
        )));
        assert!(is_network_transient_error(&anyhow::anyhow!(
            "failed to lookup address information"
        )));
        assert!(is_network_transient_error(&anyhow::anyhow!(
            "tls handshake eof"
        )));

        // Should NOT match — these belong to other classifiers
        assert!(!is_network_transient_error(&anyhow::anyhow!(
            "401 Unauthorized"
        )));
        assert!(!is_network_transient_error(&anyhow::anyhow!(
            "prompt is too long"
        )));
        assert!(!is_network_transient_error(&anyhow::anyhow!(
            "429 Too Many Requests"
        )));
        assert!(!is_network_transient_error(&anyhow::anyhow!(
            "500 Internal Server Error"
        )));
        assert!(!is_network_transient_error(&anyhow::anyhow!(
            "invalid JSON in response"
        )));
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

    // ── estimate_subagent_preflight (#1232 §3a) ─────────────────────

    fn fake_tool_def(name: &str, desc: &str) -> crate::providers::ToolDefinition {
        crate::providers::ToolDefinition {
            name: name.to_string(),
            description: desc.to_string(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }
    }

    /// Tiny payloads must always fit in any reasonable budget. Sanity
    /// floor: if a 50-char system prompt + 1 trivial tool + 10-char
    /// user prompt blows the gate, the heuristic itself is broken.
    #[test]
    fn preflight_under_budget_for_tiny_payloads() {
        let pf = estimate_subagent_preflight(
            "You are helpful.",
            &[fake_tool_def("Read", "Read a file")],
            "do the thing",
            100_000,
        );
        assert!(
            !pf.is_over_budget(),
            "tiny payload must fit in 100k budget; got {}",
            pf.summary()
        );
        assert!(pf.total_tokens > 0, "some tokens should be counted");
    }

    /// Pathological case: a giant system prompt blows the gate. Pre-PR
    /// the dispatcher would have plowed ahead and let upstream return a
    /// raw 400.
    #[test]
    fn preflight_over_budget_when_system_prompt_dwarfs_window() {
        let huge_prompt = "x".repeat(500_000); // ~143k tokens
        let pf = estimate_subagent_preflight(&huge_prompt, &[], "hi", 100_000);
        assert!(
            pf.is_over_budget(),
            "500k-char system prompt must exceed 100k budget; got {}",
            pf.summary()
        );
    }

    /// Tool definitions count toward the budget. The bug-review session
    /// in #1232 specifically called out "~30 tools" as a non-trivial
    /// share of the baseline — if tools were free, we'd be lying about
    /// the cost.
    #[test]
    fn preflight_tool_defs_contribute_to_total() {
        let no_tools = estimate_subagent_preflight("sys", &[], "prompt", 100_000);
        let many_tools: Vec<_> = (0..30)
            .map(|i| fake_tool_def(&format!("Tool{i}"), &"description ".repeat(50)))
            .collect();
        let with_tools = estimate_subagent_preflight("sys", &many_tools, "prompt", 100_000);
        assert!(
            with_tools.total_tokens > no_tools.total_tokens,
            "30 tool defs must add to the total: {} vs {}",
            with_tools.summary(),
            no_tools.summary()
        );
        assert!(
            with_tools.tool_defs_tokens > 0,
            "tool_defs_tokens must be non-zero when tools are passed"
        );
    }

    /// Summary string is human-readable and surfaces every component so
    /// the caller's error message is actionable. Pin the format — the
    /// dispatcher quotes it directly into the bubbled-up error.
    #[test]
    fn preflight_summary_includes_all_components() {
        let pf = PreflightTokenBudget {
            system_prompt_tokens: 12_345,
            tool_defs_tokens: 6_789,
            user_prompt_tokens: 1_000,
            total_tokens: 20_134,
            limit_tokens: 100_000,
        };
        let s = pf.summary();
        assert!(
            s.contains("system="),
            "summary must name the system arm: {s}"
        );
        assert!(s.contains("tools="), "summary must name the tools arm: {s}");
        assert!(
            s.contains("prompt="),
            "summary must name the prompt arm: {s}"
        );
        assert!(s.contains("limit "), "summary must show the limit: {s}");
    }

    /// Boundary case: total exactly at the limit is NOT over budget
    /// (strict `>` keeps the gate tight; the heuristic is already
    /// conservative enough that we don't need extra slack).
    #[test]
    fn preflight_at_exact_limit_is_under_budget() {
        let pf = PreflightTokenBudget {
            system_prompt_tokens: 0,
            tool_defs_tokens: 0,
            user_prompt_tokens: 0,
            total_tokens: 100,
            limit_tokens: 100,
        };
        assert!(
            !pf.is_over_budget(),
            "exactly-at-limit must not trip the gate — over-budget should be strict >"
        );
    }

    // ── is_server_error ───────────────────────────────────────────────────

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
