//! Configuration loading for agents and global settings.

use anyhow::{Context, Result};

/// Metadata for a provider — single source of truth.
pub struct ProviderMeta {
    pub name: &'static str,
    pub url: &'static str,
    pub model: &'static str,
    pub env_key: &'static str,
    pub api_key: bool,
}
use crate::model_tier::ModelTier;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Supported LLM provider types.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderType {
    OpenAI,
    Anthropic,
    LMStudio,
    Gemini,
    Groq,
    Grok,
    Ollama,
    DeepSeek,
    Mistral,
    MiniMax,
    OpenRouter,
    Together,
    Fireworks,
    Vllm,
}

impl ProviderType {
    /// Consolidated provider metadata.
    pub fn meta(&self) -> ProviderMeta {
        match self {
            Self::OpenAI => ProviderMeta {
                name: "openai",
                url: "https://api.openai.com/v1",
                model: "gpt-4o",
                env_key: "OPENAI_API_KEY",
                api_key: true,
            },
            Self::Anthropic => ProviderMeta {
                name: "anthropic",
                url: "https://api.anthropic.com",
                model: "claude-sonnet-4-6",
                env_key: "ANTHROPIC_API_KEY",
                api_key: true,
            },
            Self::LMStudio => ProviderMeta {
                name: "lm-studio",
                url: "http://localhost:1234/v1",
                model: "auto-detect",
                env_key: "KODA_API_KEY",
                api_key: false,
            },
            Self::Gemini => ProviderMeta {
                name: "gemini",
                url: "https://generativelanguage.googleapis.com",
                model: "gemini-2.0-flash",
                env_key: "GEMINI_API_KEY",
                api_key: true,
            },
            Self::Groq => ProviderMeta {
                name: "groq",
                url: "https://api.groq.com/openai/v1",
                model: "llama-3.3-70b-versatile",
                env_key: "GROQ_API_KEY",
                api_key: true,
            },
            Self::Grok => ProviderMeta {
                name: "grok",
                url: "https://api.x.ai/v1",
                model: "grok-3",
                env_key: "XAI_API_KEY",
                api_key: true,
            },
            Self::Ollama => ProviderMeta {
                name: "ollama",
                url: "http://localhost:11434/v1",
                model: "auto-detect",
                env_key: "KODA_API_KEY",
                api_key: false,
            },
            Self::DeepSeek => ProviderMeta {
                name: "deepseek",
                url: "https://api.deepseek.com/v1",
                model: "deepseek-chat",
                env_key: "DEEPSEEK_API_KEY",
                api_key: true,
            },
            Self::Mistral => ProviderMeta {
                name: "mistral",
                url: "https://api.mistral.ai/v1",
                model: "mistral-large-latest",
                env_key: "MISTRAL_API_KEY",
                api_key: true,
            },
            Self::MiniMax => ProviderMeta {
                name: "minimax",
                url: "https://api.minimax.chat/v1",
                model: "minimax-text-01",
                env_key: "MINIMAX_API_KEY",
                api_key: true,
            },
            Self::OpenRouter => ProviderMeta {
                name: "openrouter",
                url: "https://openrouter.ai/api/v1",
                model: "anthropic/claude-3.5-sonnet",
                env_key: "OPENROUTER_API_KEY",
                api_key: true,
            },
            Self::Together => ProviderMeta {
                name: "together",
                url: "https://api.together.xyz/v1",
                model: "meta-llama/Llama-3.3-70B-Instruct-Turbo",
                env_key: "TOGETHER_API_KEY",
                api_key: true,
            },
            Self::Fireworks => ProviderMeta {
                name: "fireworks",
                url: "https://api.fireworks.ai/inference/v1",
                model: "accounts/fireworks/models/llama-v3p3-70b-instruct",
                env_key: "FIREWORKS_API_KEY",
                api_key: true,
            },
            Self::Vllm => ProviderMeta {
                name: "vllm",
                url: "http://localhost:8000/v1",
                model: "auto-detect",
                env_key: "KODA_API_KEY",
                api_key: false,
            },
        }
    }

    pub fn requires_api_key(&self) -> bool {
        self.meta().api_key
    }
    pub fn default_base_url(&self) -> &str {
        self.meta().url
    }
    pub fn default_model(&self) -> &str {
        self.meta().model
    }
    pub fn env_key_name(&self) -> &str {
        self.meta().env_key
    }

    /// Detect provider type from a base URL or explicit name.
    pub fn from_url_or_name(url: &str, name: Option<&str>) -> Self {
        if let Some(n) = name {
            return match n.to_lowercase().as_str() {
                "anthropic" | "claude" => Self::Anthropic,
                "gemini" | "google" => Self::Gemini,
                "groq" => Self::Groq,
                "grok" | "xai" => Self::Grok,
                "lmstudio" | "lm-studio" => Self::LMStudio,
                "ollama" => Self::Ollama,
                "deepseek" => Self::DeepSeek,
                "mistral" => Self::Mistral,
                "minimax" => Self::MiniMax,
                "openrouter" => Self::OpenRouter,
                "together" => Self::Together,
                "fireworks" => Self::Fireworks,
                "vllm" => Self::Vllm,
                _ => Self::OpenAI,
            };
        }
        // Auto-detect from URL
        let url = url.to_lowercase();
        if url.contains("anthropic.com") {
            Self::Anthropic
        } else if url.contains("localhost:11434") || url.contains("127.0.0.1:11434") {
            Self::Ollama
        } else if url.contains("localhost:8000") || url.contains("127.0.0.1:8000") {
            Self::Vllm
        } else if url.contains("localhost") || url.contains("127.0.0.1") {
            Self::LMStudio
        } else if url.contains("generativelanguage.googleapis.com") {
            Self::Gemini
        } else if url.contains("groq.com") {
            Self::Groq
        } else if url.contains("x.ai") {
            Self::Grok
        } else if url.contains("deepseek.com") {
            Self::DeepSeek
        } else if url.contains("mistral.ai") {
            Self::Mistral
        } else if url.contains("minimax.chat") || url.contains("minimaxi.com") {
            Self::MiniMax
        } else if url.contains("openrouter.ai") {
            Self::OpenRouter
        } else if url.contains("together.xyz") {
            Self::Together
        } else if url.contains("fireworks.ai") {
            Self::Fireworks
        } else {
            Self::OpenAI
        }
    }
}

impl std::fmt::Display for ProviderType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.meta().name)
    }
}

/// Model-specific settings that control LLM behavior.
#[derive(Debug, Clone)]
pub struct ModelSettings {
    /// Model name / ID.
    pub model: String,
    /// Maximum output tokens (provider-specific default if None).
    pub max_tokens: Option<u32>,
    /// Sampling temperature.
    pub temperature: Option<f64>,
    /// Anthropic extended thinking budget (tokens).
    pub thinking_budget: Option<u32>,
    /// OpenAI reasoning effort: "low", "medium", or "high".
    pub reasoning_effort: Option<String>,
    /// Maximum context window size in tokens.
    pub max_context_tokens: usize,
}

impl ModelSettings {
    /// Build settings with provider-appropriate defaults.
    pub fn defaults_for(model: &str, provider: &ProviderType) -> Self {
        let max_tokens = match provider {
            ProviderType::Anthropic => Some(16384),
            _ => None,
        };
        let max_context_tokens = crate::model_context::context_window_for_model(model);
        Self {
            model: model.to_string(),
            max_tokens,
            temperature: None,
            thinking_budget: None,
            reasoning_effort: None,
            max_context_tokens,
        }
    }
}

/// Top-level agent configuration loaded from JSON.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    pub name: String,
    pub system_prompt: String,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub thinking_budget: Option<u32>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub max_context_tokens: Option<usize>,
    #[serde(default)]
    pub max_iterations: Option<u32>,
    #[serde(default)]
    pub auto_compact_threshold: Option<usize>,
    /// Override the auto-detected model tier.
    #[serde(default)]
    pub model_tier: Option<ModelTier>,
}

/// Runtime configuration assembled from CLI args, env vars, and agent JSON.
#[derive(Debug, Clone)]
pub struct KodaConfig {
    pub agent_name: String,
    pub system_prompt: String,
    pub allowed_tools: Vec<String>,
    pub provider_type: ProviderType,
    pub base_url: String,
    pub model: String,
    pub max_context_tokens: usize,
    pub agents_dir: PathBuf,
    pub model_settings: ModelSettings,
    pub max_iterations: u32,
    /// Context usage percentage (0-100) that triggers auto-compact. 0 = disabled.
    pub auto_compact_threshold: usize,
    /// Model capability tier (auto-detected or overridden).
    pub model_tier: ModelTier,
}

impl KodaConfig {
    /// Load config from the agent JSON file.
    /// Search order: project agents/ → user ~/.config/koda/agents/ → built-in (embedded).
    pub fn load(project_root: &Path, agent_name: &str) -> Result<Self> {
        let agents_dir =
            Self::find_agents_dir(project_root).unwrap_or_else(|_| PathBuf::from("agents"));

        // 1. Try project-local or user-level agent file on disk
        let agent_file = agents_dir.join(format!("{agent_name}.json"));
        let agent: AgentConfig = if agent_file.exists() {
            let json = std::fs::read_to_string(&agent_file)
                .with_context(|| format!("Failed to read agent config: {agent_file:?}"))?;
            serde_json::from_str(&json)
                .with_context(|| format!("Failed to parse agent config: {agent_file:?}"))?
        } else if let Some(builtin) = Self::load_builtin(agent_name) {
            // 2. Fall back to embedded built-in agent
            builtin
        } else {
            anyhow::bail!("Agent '{agent_name}' not found (checked disk and built-ins)");
        };

        let default_url = agent
            .base_url
            .clone()
            .unwrap_or_else(|| "http://localhost:1234/v1".to_string());
        let provider_type = ProviderType::from_url_or_name(&default_url, agent.provider.as_deref());

        // If it's a local provider and we have a user-defined default in env, use it
        let mut base_url = agent.base_url;
        if base_url.is_none()
            && !provider_type.requires_api_key()
            && let Some(env_url) = crate::runtime_env::get("KODA_LOCAL_URL")
        {
            base_url = Some(env_url);
        }

        let base_url = base_url.unwrap_or_else(|| provider_type.default_base_url().to_string());
        let model = agent
            .model
            .unwrap_or_else(|| provider_type.default_model().to_string());

        let mut settings = ModelSettings::defaults_for(&model, &provider_type);
        // Agent config can override the auto-detected context window
        if let Some(ctx) = agent.max_context_tokens {
            settings.max_context_tokens = ctx;
        }
        let max_context_tokens = settings.max_context_tokens;
        if let Some(mt) = agent.max_tokens {
            settings.max_tokens = Some(mt);
        }
        if let Some(t) = agent.temperature {
            settings.temperature = Some(t);
        }
        if let Some(tb) = agent.thinking_budget {
            settings.thinking_budget = Some(tb);
        }
        if let Some(ref re) = agent.reasoning_effort {
            settings.reasoning_effort = Some(re.clone());
        }

        let model_tier = agent
            .model_tier
            .unwrap_or_else(|| ModelTier::from_model_name(&model, &provider_type));

        let max_iterations = agent
            .max_iterations
            .unwrap_or_else(|| model_tier.default_max_iterations());

        let auto_compact_threshold = agent
            .auto_compact_threshold
            .unwrap_or_else(|| model_tier.default_auto_compact_threshold());

        Ok(Self {
            agent_name: agent.name,
            system_prompt: agent.system_prompt,
            allowed_tools: agent.allowed_tools,
            provider_type,
            base_url,
            model: model.clone(),
            max_context_tokens,
            agents_dir,
            model_settings: settings,
            max_iterations,
            auto_compact_threshold,
            model_tier,
        })
    }

    /// Apply CLI/env overrides on top of the loaded config.
    pub fn with_overrides(
        mut self,
        base_url: Option<String>,
        model: Option<String>,
        provider: Option<String>,
    ) -> Self {
        if let Some(ref url) = base_url {
            self.base_url = url.clone();
        }
        if let Some(ref p) = provider {
            self.provider_type = ProviderType::from_url_or_name(&self.base_url, Some(p));
        }
        if base_url.is_some() && provider.is_none() {
            // Re-detect provider from new URL
            self.provider_type = ProviderType::from_url_or_name(&self.base_url, None);
        }
        if let Some(m) = model {
            self.model = m.clone();
            self.model_settings.model = m.clone();
            // Recalculate context window and tier for the new model
            self.recalculate_model_derived();
        }
        self
    }

    /// Apply model-specific setting overrides from CLI.
    pub fn with_model_overrides(
        mut self,
        max_tokens: Option<u32>,
        temperature: Option<f64>,
        thinking_budget: Option<u32>,
        reasoning_effort: Option<String>,
    ) -> Self {
        if let Some(mt) = max_tokens {
            self.model_settings.max_tokens = Some(mt);
        }
        if let Some(t) = temperature {
            self.model_settings.temperature = Some(t);
        }
        if let Some(tb) = thinking_budget {
            self.model_settings.thinking_budget = Some(tb);
        }
        if let Some(re) = reasoning_effort {
            self.model_settings.reasoning_effort = Some(re);
        }
        self
    }

    /// Override the auto-detected model tier.
    pub fn with_tier_override(mut self, tier_str: Option<&str>) -> Self {
        if let Some(t) = tier_str {
            self.model_tier = match t {
                "strong" => ModelTier::Strong,
                "lite" => ModelTier::Lite,
                _ => ModelTier::Standard,
            };
        }
        self
    }

    /// Recalculate model-derived settings (context window, tier, iteration limits).
    ///
    /// Call this whenever `self.model` or `self.provider_type` changes to keep
    /// context window, tier, and iteration defaults in sync with the new model.
    pub fn recalculate_model_derived(&mut self) {
        let new_ctx = crate::model_context::context_window_for_model(&self.model);
        self.max_context_tokens = new_ctx;
        self.model_settings.max_context_tokens = new_ctx;

        self.model_tier = ModelTier::from_model_name(&self.model, &self.provider_type);
        self.max_iterations = self.model_tier.default_max_iterations();
        self.auto_compact_threshold = self.model_tier.default_auto_compact_threshold();
    }

    /// Built-in agent configs, embedded at compile time.
    /// These are always available regardless of disk state.
    const BUILTIN_AGENTS: &[(&str, &str)] = &[
        ("default", include_str!("../agents/default.json")),
        ("testgen", include_str!("../agents/testgen.json")),
        ("releaser", include_str!("../agents/releaser.json")),
        ("scout", include_str!("../agents/scout.json")),
        ("planner", include_str!("../agents/planner.json")),
        ("verifier", include_str!("../agents/verifier.json")),
    ];

    /// Try to load a built-in (embedded) agent by name.
    pub fn load_builtin(name: &str) -> Option<AgentConfig> {
        Self::BUILTIN_AGENTS
            .iter()
            .find(|(n, _)| *n == name)
            .and_then(|(_, json)| serde_json::from_str(json).ok())
    }

    /// Return all built-in agent configs (name, parsed config).
    pub fn builtin_agents() -> Vec<(String, AgentConfig)> {
        Self::BUILTIN_AGENTS
            .iter()
            .filter_map(|(name, json)| {
                let config: AgentConfig = serde_json::from_str(json).ok()?;
                Some((name.to_string(), config))
            })
            .collect()
    }

    /// Create a minimal config for testing.
    /// Available in both koda-core and downstream crate tests.
    pub fn default_for_testing(provider_type: ProviderType) -> Self {
        let model = provider_type.default_model().to_string();
        let model_settings = ModelSettings::defaults_for(&model, &provider_type);
        let max_context_tokens = model_settings.max_context_tokens;
        let model_tier = ModelTier::from_model_name(&model, &provider_type);
        Self {
            agent_name: "test".to_string(),
            system_prompt: "You are a test agent.".to_string(),
            allowed_tools: Vec::new(),
            base_url: provider_type.default_base_url().to_string(),
            model,
            provider_type,
            max_context_tokens,
            agents_dir: PathBuf::from("agents"),
            model_settings,
            max_iterations: crate::loop_guard::MAX_ITERATIONS_DEFAULT,
            auto_compact_threshold: 80,
            model_tier,
        }
    }

    /// Locate the agents directory on disk (for project/user overrides).
    ///
    /// Search order:
    /// 1. `<project_root>/agents/`  — repo-local agents
    /// 2. `~/.config/koda/agents/` — user-level agents
    ///
    /// Built-in agents are always available from embedded configs,
    /// so this may return Err if no disk directory exists (that's fine).
    fn find_agents_dir(project_root: &Path) -> Result<PathBuf> {
        // 1. Project-local
        let local = project_root.join("agents");
        if local.is_dir() {
            return Ok(local);
        }

        // 2. User config dir (~/.config/koda/agents/)
        let config_agents = Self::user_agents_dir()?;
        if config_agents.is_dir() {
            return Ok(config_agents);
        }

        // No disk directory found — built-in agents still work
        anyhow::bail!("No agents directory on disk (built-in agents are still available)")
    }

    /// Return the user-level agents directory path (`~/.config/koda/agents/`).
    fn user_agents_dir() -> Result<PathBuf> {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        Ok(home.join(".config").join("koda").join("agents"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── Provider detection ────────────────────────────────────

    #[test]
    fn test_provider_from_url_anthropic() {
        assert_eq!(
            ProviderType::from_url_or_name("https://api.anthropic.com/v1", None),
            ProviderType::Anthropic
        );
    }

    #[test]
    fn test_provider_from_url_localhost_defaults_to_lmstudio() {
        assert_eq!(
            ProviderType::from_url_or_name("http://localhost:1234/v1", None),
            ProviderType::LMStudio
        );
    }

    #[test]
    fn test_provider_from_explicit_name_overrides_url() {
        assert_eq!(
            ProviderType::from_url_or_name("https://my-proxy.corp.com/v1", Some("anthropic")),
            ProviderType::Anthropic
        );
    }

    #[test]
    fn test_unknown_url_defaults_to_openai() {
        assert_eq!(
            ProviderType::from_url_or_name("https://random.example.com/v1", None),
            ProviderType::OpenAI
        );
    }

    #[test]
    fn test_provider_name_aliases() {
        assert_eq!(
            ProviderType::from_url_or_name("", Some("claude")),
            ProviderType::Anthropic
        );
        assert_eq!(
            ProviderType::from_url_or_name("", Some("google")),
            ProviderType::Gemini
        );
        assert_eq!(
            ProviderType::from_url_or_name("", Some("xai")),
            ProviderType::Grok
        );
        assert_eq!(
            ProviderType::from_url_or_name("", Some("lm-studio")),
            ProviderType::LMStudio
        );
    }

    #[test]
    fn test_provider_display() {
        assert_eq!(format!("{}", ProviderType::OpenAI), "openai");
        assert_eq!(format!("{}", ProviderType::Anthropic), "anthropic");
        assert_eq!(format!("{}", ProviderType::LMStudio), "lm-studio");
    }

    #[test]
    fn test_each_provider_has_default_url_and_model() {
        let providers = [
            ProviderType::OpenAI,
            ProviderType::Anthropic,
            ProviderType::LMStudio,
            ProviderType::Gemini,
            ProviderType::Groq,
            ProviderType::Grok,
        ];
        for p in providers {
            assert!(!p.default_base_url().is_empty());
            assert!(!p.default_model().is_empty());
            assert!(!p.env_key_name().is_empty());
        }
    }

    // ── Config loading ────────────────────────────────────────

    #[test]
    fn test_load_valid_agent_config() {
        let tmp = TempDir::new().unwrap();
        let agents_dir = tmp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("test.json"),
            r#"{
            "name": "test",
            "system_prompt": "You are a test.",
            "allowed_tools": ["Read", "Write"]
        }"#,
        )
        .unwrap();
        let config = KodaConfig::load(tmp.path(), "test").unwrap();
        assert_eq!(config.agent_name, "test");
        assert_eq!(config.allowed_tools, vec!["Read", "Write"]);
    }

    #[test]
    fn test_load_missing_agent_returns_error() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("agents")).unwrap();
        assert!(KodaConfig::load(tmp.path(), "nonexistent").is_err());
    }

    #[test]
    fn test_load_malformed_json_returns_error() {
        let tmp = TempDir::new().unwrap();
        let agents_dir = tmp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(agents_dir.join("bad.json"), "NOT JSON").unwrap();
        assert!(KodaConfig::load(tmp.path(), "bad").is_err());
    }

    // ── Override logic ────────────────────────────────────────

    #[test]
    fn test_with_overrides_model() {
        let config = KodaConfig::default_for_testing(ProviderType::OpenAI).with_overrides(
            None,
            Some("gpt-4-turbo".into()),
            None,
        );
        assert_eq!(config.model, "gpt-4-turbo");
    }

    #[test]
    fn test_with_overrides_base_url_re_detects_provider() {
        let config = KodaConfig::default_for_testing(ProviderType::OpenAI).with_overrides(
            Some("https://api.anthropic.com".into()),
            None,
            None,
        );
        assert_eq!(config.provider_type, ProviderType::Anthropic);
    }

    #[test]
    fn test_with_overrides_explicit_provider_wins() {
        let config = KodaConfig::default_for_testing(ProviderType::OpenAI).with_overrides(
            Some("https://my-proxy.com".into()),
            None,
            Some("anthropic".into()),
        );
        assert_eq!(config.provider_type, ProviderType::Anthropic);
    }

    #[test]
    fn test_with_overrides_no_changes() {
        let config =
            KodaConfig::default_for_testing(ProviderType::Gemini).with_overrides(None, None, None);
        assert_eq!(config.provider_type, ProviderType::Gemini);
        assert_eq!(config.model, "gemini-2.0-flash");
    }

    // ── recalculate_model_derived ──────────────────────────────

    #[test]
    fn test_recalculate_updates_context_window() {
        // Start with LMStudio auto-detect (4096 tokens)
        let mut config = KodaConfig::default_for_testing(ProviderType::LMStudio);
        assert_eq!(config.max_context_tokens, 4_096); // MIN_CONTEXT for auto-detect
        assert_eq!(config.model_tier, ModelTier::Lite);

        // Switch to Claude Sonnet
        config.model = "claude-sonnet-4-6".to_string();
        config.model_settings.model = config.model.clone();
        config.provider_type = ProviderType::Anthropic;
        config.recalculate_model_derived();

        assert_eq!(config.max_context_tokens, 200_000);
        assert_eq!(config.model_settings.max_context_tokens, 200_000);
        assert_eq!(config.model_tier, ModelTier::Strong);
        assert_eq!(config.max_iterations, 200); // Strong tier default
    }

    #[test]
    fn test_with_overrides_model_recalculates() {
        let config = KodaConfig::default_for_testing(ProviderType::LMStudio);
        assert_eq!(config.max_context_tokens, 4_096);

        let config = config.with_overrides(None, Some("gpt-4o".into()), Some("openai".into()));
        assert_eq!(config.model, "gpt-4o");
        assert_eq!(config.max_context_tokens, 128_000);
        assert_eq!(config.model_tier, ModelTier::Strong);
    }
}
