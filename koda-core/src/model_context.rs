//! Model context window lookup.
//!
//! Maps model names to their known context window sizes.
//! Falls back to a conservative default for unknown models.

/// Default context window when the model is unknown.
const DEFAULT_CONTEXT: usize = 128_000;

/// Minimum context window (safety floor for local/unknown models).
const MIN_CONTEXT: usize = 4_096;

/// Look up the context window size for a model by name.
///
/// Matches known models by prefix/pattern. Returns the full context window
/// in tokens. The caller should apply a usage budget (e.g., 95%) to leave
/// room for the response.
pub fn context_window_for_model(model: &str) -> usize {
    let m = model.to_lowercase();

    // ── Anthropic ─────────────────────────────────────
    // All current Claude models (3.x, 4.x) have 200K context.
    // When Anthropic changes this, add specific prefixes ABOVE
    // the catch-all so they match first.
    //
    // NOTE: Anthropic's API does not expose context_window today.
    // If they add it, `model_capabilities()` in anthropic.rs will
    // return it and this table will be bypassed automatically.
    if m.starts_with("claude-opus-4")
        || m.starts_with("claude-sonnet-4")
        || m.starts_with("claude-haiku-4")
    {
        return 200_000;
    }
    if m.starts_with("claude-3.5") || m.starts_with("claude-3-5") {
        return 200_000;
    }
    if m.starts_with("claude-3") {
        return 200_000;
    }
    // Catch-all for future Claude models we haven't seen yet.
    // Log a warning so we notice when this fires.
    if m.contains("claude") {
        tracing::warn!(
            "Unknown Claude model '{}' — assuming 200K context. \
             Update model_context.rs if this model has a different context window.",
            model
        );
        return 200_000;
    }

    // ── OpenAI ────────────────────────────────────────────
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
    fn test_claude_models() {
        assert_eq!(context_window_for_model("claude-sonnet-4-6"), 200_000);
        assert_eq!(context_window_for_model("claude-3-opus-20240229"), 200_000);
        assert_eq!(context_window_for_model("claude-3-haiku-20240307"), 200_000);
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
        assert_eq!(context_window_for_model("Claude-Sonnet-4-6"), 200_000);
        assert_eq!(context_window_for_model("GPT-4O"), 128_000);
    }
}
