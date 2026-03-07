//! Per-turn cost estimation based on model and provider.
//!
//! Pricing is best-effort — returns None for unknown models.
//! Prices are per million tokens (as of March 2025).

/// Pricing per million tokens.
struct ModelPrice {
    input: f64,
    output: f64,
    /// Cache read discount (multiplier, e.g. 0.1 = 90% cheaper).
    cache_read: f64,
}

/// Estimate cost for a single turn.
///
/// Returns None if the model isn't in our pricing table.
/// `thinking_tokens` are billed at the output token rate (Anthropic/OpenAI).
pub fn estimate_turn_cost(
    model: &str,
    prompt_tokens: i64,
    completion_tokens: i64,
    cache_read_tokens: i64,
    thinking_tokens: i64,
) -> Option<f64> {
    let price = lookup_price(model)?;

    let billable_input = (prompt_tokens - cache_read_tokens).max(0);
    let cached = cache_read_tokens.max(0);

    let input_cost = billable_input as f64 * price.input / 1_000_000.0;
    let cache_cost = cached as f64 * price.input * price.cache_read / 1_000_000.0;
    let output_cost = completion_tokens as f64 * price.output / 1_000_000.0;
    // Thinking/reasoning tokens are billed at output rate
    let thinking_cost = thinking_tokens.max(0) as f64 * price.output / 1_000_000.0;

    Some(input_cost + cache_cost + output_cost + thinking_cost)
}

fn lookup_price(model: &str) -> Option<ModelPrice> {
    // Normalize: lowercase, strip version suffixes for matching.
    let m = model.to_lowercase();

    // Anthropic
    if m.contains("claude-3-5-sonnet") || m.contains("claude-sonnet-4") {
        return Some(ModelPrice {
            input: 3.0,
            output: 15.0,
            cache_read: 0.1,
        });
    }
    if m.contains("claude-3-5-haiku") || m.contains("claude-haiku-4") {
        return Some(ModelPrice {
            input: 0.80,
            output: 4.0,
            cache_read: 0.1,
        });
    }
    if m.contains("claude-3-opus") || m.contains("claude-opus-4") {
        return Some(ModelPrice {
            input: 15.0,
            output: 75.0,
            cache_read: 0.1,
        });
    }

    // OpenAI
    if m.contains("gpt-4o-mini") {
        return Some(ModelPrice {
            input: 0.15,
            output: 0.60,
            cache_read: 0.5,
        });
    }
    if m.contains("gpt-4o") {
        return Some(ModelPrice {
            input: 2.50,
            output: 10.0,
            cache_read: 0.5,
        });
    }
    if m.contains("o3-mini") {
        return Some(ModelPrice {
            input: 1.10,
            output: 4.40,
            cache_read: 0.5,
        });
    }
    if m.contains("o3") && !m.contains("o3-mini") {
        return Some(ModelPrice {
            input: 10.0,
            output: 40.0,
            cache_read: 0.5,
        });
    }
    if m.contains("o1-mini") {
        return Some(ModelPrice {
            input: 1.10,
            output: 4.40,
            cache_read: 0.5,
        });
    }
    if m.contains("o1") && !m.contains("o1-mini") {
        return Some(ModelPrice {
            input: 15.0,
            output: 60.0,
            cache_read: 0.5,
        });
    }

    // Google Gemini
    if m.contains("gemini-2.0-flash") || m.contains("gemini-2.5-flash") {
        return Some(ModelPrice {
            input: 0.075,
            output: 0.30,
            cache_read: 1.0,
        });
    }
    if m.contains("gemini-2.5-pro") || m.contains("gemini-1.5-pro") {
        return Some(ModelPrice {
            input: 1.25,
            output: 5.0,
            cache_read: 1.0,
        });
    }

    // DeepSeek
    if m.contains("deepseek-chat") || m.contains("deepseek-v3") {
        return Some(ModelPrice {
            input: 0.27,
            output: 1.10,
            cache_read: 0.1,
        });
    }
    if m.contains("deepseek-reasoner") {
        return Some(ModelPrice {
            input: 0.55,
            output: 2.19,
            cache_read: 0.1,
        });
    }

    // Groq (free tier / very cheap)
    if m.contains("llama") && m.contains("groq") {
        return Some(ModelPrice {
            input: 0.05,
            output: 0.08,
            cache_read: 1.0,
        });
    }

    // Mistral
    if m.contains("mistral-large") {
        return Some(ModelPrice {
            input: 2.0,
            output: 6.0,
            cache_read: 1.0,
        });
    }

    // Local models — free
    if m.contains("auto-detect") || m.contains("local") {
        return Some(ModelPrice {
            input: 0.0,
            output: 0.0,
            cache_read: 1.0,
        });
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_claude_sonnet_cost() {
        let cost = estimate_turn_cost("claude-sonnet-4-6", 1000, 500, 0, 0).unwrap();
        // 1000 * 3.0/1M + 500 * 15.0/1M = 0.003 + 0.0075 = 0.0105
        assert!((cost - 0.0105).abs() < 0.0001);
    }

    #[test]
    fn test_cache_discount() {
        let cost = estimate_turn_cost("claude-sonnet-4-6", 1000, 500, 800, 0).unwrap();
        // billable_input = 200, cached = 800
        // 200 * 3.0/1M + 800 * 3.0 * 0.1/1M + 500 * 15.0/1M
        // = 0.0006 + 0.00024 + 0.0075 = 0.00834
        assert!((cost - 0.00834).abs() < 0.0001);
    }

    #[test]
    fn test_unknown_model() {
        assert!(estimate_turn_cost("unknown-model-42", 1000, 500, 0, 0).is_none());
    }

    #[test]
    fn test_local_model_free() {
        let cost = estimate_turn_cost("auto-detect", 10000, 5000, 0, 0).unwrap();
        assert_eq!(cost, 0.0);
    }

    #[test]
    fn test_gpt4o_cost() {
        let cost = estimate_turn_cost("gpt-4o", 10000, 1000, 0, 0).unwrap();
        // 10000 * 2.5/1M + 1000 * 10.0/1M = 0.025 + 0.01 = 0.035
        assert!((cost - 0.035).abs() < 0.001);
    }

    #[test]
    fn test_thinking_tokens_billed_at_output_rate() {
        // With thinking: completion=500, thinking=1000
        let cost_with = estimate_turn_cost("claude-opus-4-6", 1000, 500, 0, 1000).unwrap();
        let cost_without = estimate_turn_cost("claude-opus-4-6", 1000, 500, 0, 0).unwrap();
        // thinking adds 1000 * 75.0/1M = 0.075
        let thinking_cost = 1000.0 * 75.0 / 1_000_000.0;
        assert!((cost_with - cost_without - thinking_cost).abs() < 0.0001);
    }
}
