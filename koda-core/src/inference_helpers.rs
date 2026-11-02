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

use crate::providers::{ChatMessage, ToolCall};

/// Context usage % at which a pre-flight auto-compact fires.
/// Matches CC's default (~85%). Hard-coded — no config knob needed.
pub const AUTO_COMPACT_THRESHOLD: usize = 85;

/// A history under a window divided by this isn't worth a compaction: most of
/// the context is then the system prompt and tools, which can't shrink.
pub const MIN_COMPACTABLE_DIVISOR: usize = 5;

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

/// Tokens a request spends on tool definitions, which travel as JSON beside the
/// messages and fill the window like them.
///
/// `serde_json::to_string` failures are vanishingly unlikely for our owned
/// types, but treat any failure as zero rather than panicking: an estimate that
/// crashes is strictly worse than one that runs a few k tokens low.
pub fn estimate_tool_defs(tool_defs: &[crate::providers::ToolDefinition]) -> usize {
    let chars = serde_json::to_string(tool_defs).map_or(0, |s| s.len());
    (chars as f64 / CHARS_PER_TOKEN) as usize
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

    let tool_defs_tokens = estimate_tool_defs(tool_defs);

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
    the conversation naturally based on the summarized context, with all your tools still \
    available.";

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

/// One finished model call, for the log. Holds no conversation text.
pub(crate) struct ModelCall<'a> {
    /// What the call was for: `turn` or `compaction`.
    pub purpose: &'static str,
    /// Hash of the system prompt sent, and of the tool definitions. Compaction
    /// sends neither.
    pub system_hash: Option<&'a str>,
    pub tools_hash: Option<&'a str>,
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
    /// What the provider says the request put in the context window.
    pub context_tokens: i64,
    /// Usage as the provider reported it.
    pub usage: &'a crate::providers::TokenUsage,
    /// Tool calls in the reply.
    pub tool_calls: usize,
    /// Characters of text in the reply.
    pub text_chars: usize,
}

/// A short fingerprint of `text`, to tell in a log whether two calls sent the
/// same thing without saving it. Comparable within a build, not across them.
pub(crate) fn short_hash(text: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
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
            system_hash = self.system_hash,
            tools_hash = self.tools_hash,
            provider = self.provider,
            model = self.model,
            messages = self.messages,
            estimated_tokens = self.estimated_tokens,
            context_tokens = self.context_tokens,
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

/// Attempts a request gets: the first, then four retries, matching Codex's
/// `DEFAULT_REQUEST_MAX_RETRIES`.
pub const RATE_LIMIT_MAX_RETRIES: u32 = 5;

/// Retries for a reply whose stream failed partway, matching Codex's
/// `DEFAULT_STREAM_MAX_RETRIES`.
pub const STREAM_MAX_RETRIES: u32 = 5;

/// The longest wait a server can ask for. Past it the limit won't lift
/// within a turn, so the request fails instead of sitting silent.
pub const MAX_SERVER_DELAY: std::time::Duration = std::time::Duration::from_secs(300);

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
}
