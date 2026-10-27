//! Model context window lookup.
//!
//! Maps model names to their known context window sizes (in tokens).
//! Falls back to a conservative default (100K) for unknown models.
//!
//! Context window size drives:
//! - Output caps (see [`crate::output_caps`]) — larger windows get more tool output
//! - Auto-compaction threshold — triggers at ~80% of context window
//! - Microcompact aggressiveness — scales with remaining capacity

/// Default context window when the model is unknown.
const DEFAULT_CONTEXT: usize = 128_000;

/// Minimum context window (safety floor for local/unknown models).
const MIN_CONTEXT: usize = 4_096;

/// Look up the context window size for a model by name.
///
/// Matches known models by prefix/pattern. Returns the full context window
/// in tokens. The caller should apply a usage budget (e.g., 95%) to leave
/// room for the response.
///
/// # Examples
///
/// ```
/// use koda_core::model_context::context_window_for_model;
///
/// assert_eq!(context_window_for_model("claude-sonnet-5-5"), 1_000_000);
/// assert_eq!(context_window_for_model("claude-haiku-4-5"), 200_000);
/// assert_eq!(context_window_for_model("gpt-4o"), 128_000);
/// assert_eq!(context_window_for_model("gemini-2.5-pro"), 1_048_576);
///
/// // Unknown models get a conservative default:
/// assert_eq!(context_window_for_model("mystery-model"), 128_000);
/// ```
pub fn context_window_for_model(model: &str) -> usize {
    let m = model.to_lowercase();

    // ── Anthropic ─────────────────────────────────────
    // Checked 2026-10-27 against
    // https://platform.claude.com/docs/en/build-with-claude/context-windows:
    // every 5.x model (Fable/Mythos 5, Opus 5, Sonnet 5, Haiku 5.5), Opus
    // 4.6/4.7/4.8 and Sonnet 4.6 have a 1M-token window by default, with
    // no beta header. Opus 4.5, Haiku 4.5, Sonnet 4.5, Sonnet 4 and
    // Claude 3.x have 200K.
    //
    // The virtual "-1m" suffix predates native 1M: it opts Sonnet 4.5 / 4
    // into the `context-1m-2025-08-07` beta (see providers/anthropic.rs).
    // On a natively-1M model the suffix is accepted and means nothing; on
    // any other model it falls through to that model's 200K.
    if m.contains("claude") {
        let base = m.strip_suffix("-1m").unwrap_or(&m);
        if has_native_1m_context(base) {
            return 1_000_000;
        }
        if base.len() != m.len() {
            // "-1m" requested on a model without native 1M. Sonnet 4.5 and
            // the dated Sonnet 4 snapshots take the beta header.
            if base.starts_with("claude-sonnet-4") {
                return 1_000_000;
            }
            tracing::warn!(
                "Model '{}' does not support 1M extended context; using 200K.",
                model
            );
            return 200_000;
        }
        if base.starts_with("claude-opus-4")
            || base.starts_with("claude-sonnet-4")
            || base.starts_with("claude-haiku-4")
            || base.starts_with("claude-3")
        {
            return 200_000;
        }
        // Catch-all for Claude models this table hasn't seen yet.
        tracing::warn!(
            "Unknown Claude model '{}' — assuming 200K context. \
             Update model_context.rs if this model has a different context window.",
            model
        );
        return 200_000;
    }

    // ── OpenAI ────────────────────────────────────────────
    // GPT-6 family (gpt-6-astra, gpt-6.1-sol, gpt-6.1-terra, gpt-6-luna):
    // 1,050,000 tokens per https://developers.openai.com/api/docs/models
    // (checked 2026-10-27).
    if m.starts_with("gpt-6") {
        return 1_050_000;
    }
    if m.starts_with("gpt-4o") || m.starts_with("gpt-4.1") || m.starts_with("chatgpt-4o") {
        return 128_000;
    }
    if m.starts_with("gpt-4-turbo") || m.starts_with("gpt-4-1106") || m.starts_with("gpt-4-0125") {
        return 128_000;
    }
    if m.starts_with("gpt-4") {
        return 8_192;
    }
    if m.starts_with("gpt-3.5-turbo-16k") {
        return 16_384;
    }
    if m.starts_with("gpt-3.5") {
        return 16_384;
    }
    if m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4") {
        return 200_000;
    }

    // ── Google Gemini ─────────────────────────────────────
    if m.contains("gemini-2.5") {
        return 1_048_576;
    }
    if m.contains("gemini-2.0") {
        return 1_048_576;
    }
    if m.contains("gemini-1.5-pro") {
        return 2_097_152;
    }
    if m.contains("gemini-1.5-flash") {
        return 1_048_576;
    }
    if m.contains("gemini") {
        return 1_048_576;
    }

    // ── Grok (xAI) ────────────────────────────────────────
    // Per https://docs.x.ai/docs/models (checked 2026-10-27): grok-4.5,
    // 4.6 and 4.7 have 500K; grok-4.3 and grok-4.20 have 1M.
    if m.starts_with("grok-4.5") || m.starts_with("grok-4.6") || m.starts_with("grok-4.7") {
        return 500_000;
    }
    if m.starts_with("grok-4.3") || m.starts_with("grok-4.20") {
        return 1_000_000;
    }
    if m.starts_with("grok-3") {
        return 131_072;
    }
    if m.starts_with("grok") {
        return 131_072;
    }

    // ── DeepSeek ──────────────────────────────────────────
    if m.contains("deepseek") {
        return 128_000;
    }

    // ── Mistral ───────────────────────────────────────────
    if m.contains("mistral-large") {
        return 128_000;
    }
    if m.contains("mistral-medium") {
        return 32_000;
    }
    if m.contains("mistral-small") || m.contains("mistral-7b") {
        return 32_000;
    }
    if m.contains("mixtral") || m.contains("mistral") {
        return 32_000;
    }

    // ── Meta Llama ────────────────────────────────────────
    if m.contains("llama-3.3") || m.contains("llama-3.1") {
        return 128_000;
    }
    if m.contains("llama-3") || m.contains("llama3") {
        return 8_192;
    }
    if m.contains("llama") {
        return 4_096;
    }

    // ── Qwen ──────────────────────────────────────────────
    if m.contains("qwen-2.5") || m.contains("qwen2.5") {
        return 128_000;
    }
    if m.contains("qwen") {
        return 32_000;
    }

    // ── Local / auto-detect ───────────────────────────────
    if m == "auto-detect" {
        return MIN_CONTEXT;
    }

    DEFAULT_CONTEXT
}

/// Whether a Claude model has a 1M-token context window by default, with
/// no beta header.
///
/// Takes the real API model id, without the virtual `-1m` suffix. The
/// prefixes were checked on 2026-10-27 against
/// <https://platform.claude.com/docs/en/build-with-claude/context-windows>;
/// add one when a new family ships with 1M.
pub fn has_native_1m_context(model: &str) -> bool {
    const NATIVE_1M: &[&str] = &[
        "claude-fable-5",
        "claude-mythos-5",
        "claude-opus-5",
        "claude-sonnet-5",
        "claude-haiku-5",
        "claude-opus-4-6",
        "claude-opus-4-7",
        "claude-opus-4-8",
        "claude-sonnet-4-6",
    ];
    let m = model.to_lowercase();
    NATIVE_1M.iter().any(|prefix| m.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_claude_models() {
        // 5.x, Opus 4.6+ and Sonnet 4.6: 1M by default, no beta header
        assert_eq!(context_window_for_model("claude-haiku-5-5"), 1_000_000);
        assert_eq!(context_window_for_model("claude-sonnet-5-5"), 1_000_000);
        assert_eq!(context_window_for_model("claude-opus-5-5"), 1_000_000);
        assert_eq!(context_window_for_model("claude-opus-5"), 1_000_000);
        assert_eq!(context_window_for_model("claude-sonnet-5"), 1_000_000);
        assert_eq!(context_window_for_model("claude-fable-5-1"), 1_000_000);
        assert_eq!(context_window_for_model("claude-opus-4-8"), 1_000_000);
        assert_eq!(context_window_for_model("claude-opus-4-7"), 1_000_000);
        assert_eq!(context_window_for_model("claude-opus-4-6"), 1_000_000);
        assert_eq!(context_window_for_model("claude-sonnet-4-6"), 1_000_000);
        // Older 4.x: 200K
        assert_eq!(
            context_window_for_model("claude-opus-4-5-20251101"),
            200_000
        );
        assert_eq!(context_window_for_model("claude-haiku-4-5"), 200_000);
        assert_eq!(
            context_window_for_model("claude-haiku-4-5-20251001"),
            200_000
        );
        assert_eq!(
            context_window_for_model("claude-sonnet-4-5-20250929"),
            200_000
        );
        assert_eq!(context_window_for_model("claude-opus-4-20250514"), 200_000);
        assert_eq!(
            context_window_for_model("claude-sonnet-4-20250514"),
            200_000
        );
        // Claude 3.x: 200K context
        assert_eq!(context_window_for_model("claude-3-opus-20240229"), 200_000);
        assert_eq!(context_window_for_model("claude-3-haiku-20240307"), 200_000);
        assert_eq!(
            context_window_for_model("claude-3-5-sonnet-20240620"),
            200_000
        );
    }

    #[test]
    fn test_unknown_claude_model_assumes_200k() {
        // A family this table hasn't seen gets the conservative window.
        assert_eq!(context_window_for_model("claude-sonnet-7"), 200_000);
    }

    #[test]
    fn test_native_1m_prefixes() {
        assert!(has_native_1m_context("claude-sonnet-5-5"));
        assert!(has_native_1m_context("claude-haiku-5-5"));
        assert!(has_native_1m_context("claude-opus-4-6"));
        assert!(has_native_1m_context("Claude-Sonnet-4-6"));
        assert!(!has_native_1m_context("claude-haiku-4-5"));
        assert!(!has_native_1m_context("claude-opus-4-5-20251101"));
        assert!(!has_native_1m_context("claude-sonnet-4-5-20250929"));
        assert!(!has_native_1m_context("claude-3-opus-20240229"));
    }

    #[test]
    fn test_claude_1m_virtual_models() {
        // Beta-eligible models with -1m suffix get 1M
        assert_eq!(
            context_window_for_model("claude-sonnet-4-5-20250929-1m"),
            1_000_000
        );
        assert_eq!(
            context_window_for_model("claude-sonnet-4-20250514-1m"),
            1_000_000
        );
        // Natively-1M models accept the suffix as a no-op
        assert_eq!(context_window_for_model("claude-sonnet-4-6-1m"), 1_000_000);
        assert_eq!(context_window_for_model("claude-opus-4-6-1m"), 1_000_000);
        assert_eq!(context_window_for_model("claude-sonnet-5-5-1m"), 1_000_000);
    }

    #[test]
    fn test_claude_1m_ineligible_models() {
        // Ineligible models with -1m suffix stay at 200K
        assert_eq!(
            context_window_for_model("claude-opus-4-5-20251101-1m"),
            200_000
        );
        assert_eq!(
            context_window_for_model("claude-haiku-4-5-20251001-1m"),
            200_000
        );
        assert_eq!(
            context_window_for_model("claude-3-opus-20240229-1m"),
            200_000
        );
    }

    #[test]
    fn test_gpt6_models() {
        assert_eq!(context_window_for_model("gpt-6-astra"), 1_050_000);
        assert_eq!(context_window_for_model("gpt-6.1-sol"), 1_050_000);
        assert_eq!(context_window_for_model("gpt-6-luna"), 1_050_000);
    }

    #[test]
    fn test_gpt4o_models() {
        assert_eq!(context_window_for_model("gpt-4o"), 128_000);
        assert_eq!(context_window_for_model("gpt-4o-mini"), 128_000);
        assert_eq!(context_window_for_model("gpt-4.1"), 128_000);
    }

    #[test]
    fn test_gpt4_legacy() {
        assert_eq!(context_window_for_model("gpt-4"), 8_192);
        assert_eq!(context_window_for_model("gpt-4-0613"), 8_192);
    }

    #[test]
    fn test_gpt4_turbo() {
        assert_eq!(context_window_for_model("gpt-4-turbo"), 128_000);
        assert_eq!(context_window_for_model("gpt-4-turbo-preview"), 128_000);
    }

    #[test]
    fn test_o_series() {
        assert_eq!(context_window_for_model("o1"), 200_000);
        assert_eq!(context_window_for_model("o1-preview"), 200_000);
        assert_eq!(context_window_for_model("o3-mini"), 200_000);
        assert_eq!(context_window_for_model("o4-mini"), 200_000);
    }

    #[test]
    fn test_gemini_models() {
        assert_eq!(context_window_for_model("gemini-2.0-flash"), 1_048_576);
        assert_eq!(context_window_for_model("gemini-2.5-pro"), 1_048_576);
        assert_eq!(context_window_for_model("gemini-1.5-pro"), 2_097_152);
    }

    #[test]
    fn test_grok_models() {
        assert_eq!(context_window_for_model("grok-4.7"), 500_000);
        assert_eq!(context_window_for_model("grok-4.5"), 500_000);
        assert_eq!(context_window_for_model("grok-4.3"), 1_000_000);
        assert_eq!(
            context_window_for_model("grok-4.20-0309-reasoning"),
            1_000_000
        );
        assert_eq!(context_window_for_model("grok-3"), 131_072);
    }

    #[test]
    fn test_deepseek() {
        assert_eq!(context_window_for_model("deepseek-chat"), 128_000);
        assert_eq!(context_window_for_model("deepseek-coder"), 128_000);
    }

    #[test]
    fn test_llama_models() {
        assert_eq!(context_window_for_model("llama-3.3-70b-versatile"), 128_000);
        assert_eq!(context_window_for_model("llama-3-8b"), 8_192);
    }

    #[test]
    fn test_auto_detect_is_conservative() {
        assert_eq!(context_window_for_model("auto-detect"), MIN_CONTEXT);
    }

    #[test]
    fn test_unknown_model_gets_default() {
        assert_eq!(
            context_window_for_model("some-random-model"),
            DEFAULT_CONTEXT
        );
    }

    #[test]
    fn test_case_insensitive() {
        assert_eq!(context_window_for_model("Claude-Sonnet-5-5"), 1_000_000);
        assert_eq!(context_window_for_model("Claude-Haiku-4-5"), 200_000);
        assert_eq!(context_window_for_model("GPT-4O"), 128_000);
    }
}
