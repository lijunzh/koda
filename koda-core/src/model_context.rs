//! Model context window lookup.
//!
//! Maps model names to their known context window sizes (in tokens).
//! Falls back to a conservative default (100K) for unknown models.
//!
//! Context window size drives:
//! - Output caps (see [`crate::output_caps`]) — larger windows get more tool output
//! - Auto-compaction threshold — triggers at ~80% of context window

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
/// assert_eq!(context_window_for_model("gpt-4o"), 128_000);
/// assert_eq!(context_window_for_model("gemini-2.5-pro"), 1_048_576);
///
/// // Unknown models get a conservative default:
/// assert_eq!(context_window_for_model("mystery-model"), 128_000);
/// ```
pub fn context_window_for_model(model: &str) -> usize {
    let m = model.to_lowercase();

    // ── Anthropic ─────────────────────────────────────
    // Every current Claude model, 5.x and Fable, has a 1M-token window
    // (checked 2026-10-27 against
    // https://platform.claude.com/docs/en/build-with-claude/context-windows),
    // and Koda is built for the current models: an older one, which has 200K,
    // is warned about and otherwise treated as current. The Models API's
    // `max_input_tokens` replaces this when Koda can read it.
    if m.contains("claude") {
        return 1_000_000;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_claude_model_is_current_with_1m() {
        for model in [
            "claude-haiku-5-5",
            "claude-sonnet-5-5",
            "claude-opus-5-5",
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-fable-5-2",
            // A model released after this table, and one from the older
            // line, which Koda warns about and otherwise treats as current.
            "claude-sonnet-7",
            "claude-haiku-4-5",
        ] {
            assert_eq!(context_window_for_model(model), 1_000_000, "{model}");
        }
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
        assert_eq!(context_window_for_model("GPT-4O"), 128_000);
    }
}
