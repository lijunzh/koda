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
    pub fn from_model_name(model: &str, provider: &ProviderType) -> Self {
        let m = model.to_lowercase();

        // Strong tier — frontier models
        if m.contains("opus")
            || m.contains("sonnet")
            || (m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4"))
            || m.starts_with("gpt-4o")
            || m.starts_with("gpt-4.1")
            || m.contains("gemini-2.5-pro")
            || m.contains("deepseek-r1")
            || m.contains("grok-3")
        {
            return Self::Strong;
        }

        // Lite tier — small/local models
        if m.contains("lite")
            || m.contains("nano")
            || m.contains("-8b")
            || m.contains("-7b")
            || m.contains("-3b")
            || m.contains("-1b")
            || m == "auto-detect"
            || matches!(
                provider,
                ProviderType::LMStudio | ProviderType::Ollama | ProviderType::Vllm
            )
        {
            return Self::Lite;
        }

        // Everything else is Standard
        Self::Standard
    }

    /// Default max iterations for the inference loop.
    pub fn default_max_iterations(self) -> u32 {
        match self {
            Self::Strong => 200,
            Self::Standard => 200,
            Self::Lite => 50,
        }
    }

    /// Default auto-compact threshold (percentage).
    pub fn default_auto_compact_threshold(self) -> usize {
        match self {
            Self::Strong => 90,
            Self::Standard => 80,
            Self::Lite => 70,
        }
    }

    /// Whether parallel tool execution is allowed.
    pub fn allows_parallel_tools(self) -> bool {
        match self {
            Self::Strong | Self::Standard => true,
            Self::Lite => false,
        }
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
    fn test_strong_models() {
        let p = ProviderType::Anthropic;
        assert_eq!(
            ModelTier::from_model_name("claude-opus-4-6", &p),
            ModelTier::Strong
        );
        assert_eq!(
            ModelTier::from_model_name("claude-sonnet-4-6", &p),
            ModelTier::Strong
        );

        let p = ProviderType::OpenAI;
        assert_eq!(ModelTier::from_model_name("gpt-4o", &p), ModelTier::Strong);
        assert_eq!(
            ModelTier::from_model_name("gpt-4o-mini", &p),
            ModelTier::Strong
        );
        assert_eq!(ModelTier::from_model_name("o3-mini", &p), ModelTier::Strong);
        assert_eq!(ModelTier::from_model_name("o1", &p), ModelTier::Strong);
        assert_eq!(ModelTier::from_model_name("gpt-4.1", &p), ModelTier::Strong);

        let p = ProviderType::Gemini;
        assert_eq!(
            ModelTier::from_model_name("gemini-2.5-pro", &p),
            ModelTier::Strong
        );

        let p = ProviderType::Grok;
        assert_eq!(ModelTier::from_model_name("grok-3", &p), ModelTier::Strong);
    }

    #[test]
    fn test_standard_models() {
        assert_eq!(
            ModelTier::from_model_name("gemini-2.5-flash", &ProviderType::Gemini),
            ModelTier::Standard
        );
        assert_eq!(
            ModelTier::from_model_name("gemini-2.0-flash", &ProviderType::Gemini),
            ModelTier::Standard
        );
        assert_eq!(
            ModelTier::from_model_name("deepseek-chat", &ProviderType::DeepSeek),
            ModelTier::Standard
        );
        assert_eq!(
            ModelTier::from_model_name("mistral-large-latest", &ProviderType::Mistral),
            ModelTier::Standard
        );
        assert_eq!(
            ModelTier::from_model_name("llama-3.3-70b-versatile", &ProviderType::Groq),
            ModelTier::Standard
        );
    }

    #[test]
    fn test_lite_models() {
        assert_eq!(
            ModelTier::from_model_name("auto-detect", &ProviderType::LMStudio),
            ModelTier::Lite
        );
        assert_eq!(
            ModelTier::from_model_name("auto-detect", &ProviderType::Ollama),
            ModelTier::Lite
        );
        assert_eq!(
            ModelTier::from_model_name("llama-3-8b", &ProviderType::Groq),
            ModelTier::Lite
        );
        assert_eq!(
            ModelTier::from_model_name("gemini-flash-lite", &ProviderType::Gemini),
            ModelTier::Lite
        );
        assert_eq!(
            ModelTier::from_model_name("qwen-2.5-7b", &ProviderType::Ollama),
            ModelTier::Lite
        );
    }

    #[test]
    fn test_tier_defaults() {
        assert_eq!(ModelTier::Strong.default_max_iterations(), 200);
        assert_eq!(ModelTier::Standard.default_max_iterations(), 200);
        assert_eq!(ModelTier::Lite.default_max_iterations(), 50);

        assert_eq!(ModelTier::Strong.default_auto_compact_threshold(), 90);
        assert_eq!(ModelTier::Lite.default_auto_compact_threshold(), 70);

        assert!(ModelTier::Strong.allows_parallel_tools());
        assert!(!ModelTier::Lite.allows_parallel_tools());
    }

    #[test]
    fn test_display() {
        assert_eq!(format!("{}", ModelTier::Strong), "Strong");
        assert_eq!(format!("{}", ModelTier::Standard), "Standard");
        assert_eq!(format!("{}", ModelTier::Lite), "Lite");
    }
}
