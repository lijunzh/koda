//! Model capability tiers.
//!
//! Classifies models into Strong/Standard/Lite tiers based on name and
//! provider. Each tier gets different system prompts, tool loading
//! strategies, loop limits, and inference parameters.

use crate::config::ProviderType;

/// Model capability tier — determines prompt verbosity, tool loading,
/// and inference parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelTier {
    /// Frontier models: excellent reasoning, tool use, intent inference.
    /// Can work with minimal prompts and discover tools on demand.
    Strong,
    /// Capable models: good reasoning, reliable tool use.
    /// Need standard prompts but handle some ambiguity.
    Standard,
    /// Smaller/cheaper models: basic tool use, need explicit instructions.
    /// Require all schemas upfront, step-by-step guidance.
    Lite,
}

impl ModelTier {
    /// Auto-detect tier from model name and provider.
    ///
    /// Returns Standard as the default. The `TierObserver` will
    /// promote to Strong or demote to Lite based on observed behavior.
    /// This is only used as an initial hint; the observer has final say.
    pub fn from_model_name(_model: &str, _provider: &ProviderType) -> Self {
        // All models start at Standard. TierObserver promotes/demotes
        // based on actual tool-use quality rather than name guessing.
        Self::Standard
    }

    /// Default max iterations for the inference loop.
    ///
    /// All tiers get the same cap. Local models are free to run,
    /// and the user can extend interactively.
    pub fn default_max_iterations(self) -> u32 {
        200
    }

    /// Default auto-compact threshold (percentage of context window).
    pub fn default_auto_compact_threshold(self) -> usize {
        85
    }

    /// Whether parallel tool execution is allowed.
    ///
    /// Enabled for all tiers. If a model sends broken parallel calls,
    /// the errors will trigger demotion via TierObserver.
    pub fn allows_parallel_tools(self) -> bool {
        true
    }

    /// Display label for status bar.
    pub fn label(self) -> &'static str {
        match self {
            Self::Strong => "Strong",
            Self::Standard => "Standard",
            Self::Lite => "Lite",
        }
    }
}

impl std::fmt::Display for ModelTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.label())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_models_default_to_standard() {
        // Name-based detection is removed; everything starts Standard.
        let cases = vec![
            ("claude-opus-4-6", ProviderType::Anthropic),
            ("gpt-4o", ProviderType::OpenAI),
            ("gemini-2.5-flash", ProviderType::Gemini),
            ("llama-3-8b", ProviderType::Groq),
            ("auto-detect", ProviderType::LMStudio),
            ("qwen-2.5-7b", ProviderType::Ollama),
        ];
        for (model, provider) in cases {
            assert_eq!(
                ModelTier::from_model_name(model, &provider),
                ModelTier::Standard,
                "Expected Standard for {model} on {provider:?}"
            );
        }
    }

    #[test]
    fn resource_limits_are_tier_independent() {
        // All tiers get the same resource limits (decoupled from prompt strategy).
        assert_eq!(ModelTier::Strong.default_max_iterations(), 200);
        assert_eq!(ModelTier::Standard.default_max_iterations(), 200);
        assert_eq!(ModelTier::Lite.default_max_iterations(), 200);

        assert_eq!(ModelTier::Strong.default_auto_compact_threshold(), 85);
        assert_eq!(ModelTier::Standard.default_auto_compact_threshold(), 85);
        assert_eq!(ModelTier::Lite.default_auto_compact_threshold(), 85);

        assert!(ModelTier::Strong.allows_parallel_tools());
        assert!(ModelTier::Standard.allows_parallel_tools());
        assert!(ModelTier::Lite.allows_parallel_tools());
    }

    #[test]
    fn display() {
        assert_eq!(format!("{}", ModelTier::Strong), "Strong");
        assert_eq!(format!("{}", ModelTier::Standard), "Standard");
        assert_eq!(format!("{}", ModelTier::Lite), "Lite");
    }
}
