//! Configuration loading for agents and global settings.
//!
//! ## Agent discovery order
//!
//! 1. **Project agents** — `<project>/agents/*.json` (highest priority)
//! 2. **User agents** — `~/.config/koda/agents/*.json`
//! 3. **Built-in agents** — embedded at compile time (lowest priority)
//!
//! Project agents override user agents, which override built-ins.
//!
//! ## Built-in agents
//!
//! | Name | Purpose | Tools |
//! |---|---|---|
//! | `default` | Main interactive agent | All |
//! | `task` | General-purpose delegated worker | All (write access) |
//! | `explore` | Read-only code search | Read, Grep, Glob, List |
//! | `guide` | Documentation assistant | WebFetch, WebSearch |
//! | `plan` | Architecture planning | Read-only |
//! | `verify` | Code review and verification | Read-only |

use anyhow::{Context, Result};

/// Metadata for a provider — single source of truth.
pub struct ProviderMeta {
    /// Display name.
    pub name: &'static str,
    /// Default API base URL.
    pub url: &'static str,
    /// Default model identifier.
    pub model: &'static str,
    /// Environment variable for the API key.
    pub env_key: &'static str,
    /// Whether this provider requires an API key.
    pub api_key: bool,
}
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Supported LLM provider types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderType {
    /// OpenAI API.
    OpenAI,
    /// Anthropic Claude API.
    Anthropic,
    /// LM Studio (local, OpenAI-compatible).
    LMStudio,
    /// Google Gemini API.
    Gemini,
    /// Groq (OpenAI-compatible).
    Groq,
    /// Grok / xAI API.
    Grok,
    /// Ollama (local, OpenAI-compatible).
    Ollama,
    /// DeepSeek API.
    DeepSeek,
    /// Mistral AI API.
    Mistral,
    /// MiniMax API.
    MiniMax,
    /// OpenRouter (multi-provider gateway).
    OpenRouter,
    /// Together AI API.
    Together,
    /// Fireworks AI API.
    Fireworks,
    /// vLLM (local, OpenAI-compatible).
    Vllm,
    /// Mock provider for testing (reads KODA_MOCK_RESPONSES env var).
    #[cfg(any(test, feature = "test-support"))]
    Mock,
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
                model: "gemini-flash-latest",
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
                url: "https://api.minimax.io/v1",
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
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock => ProviderMeta {
                name: "mock",
                url: "http://localhost:0",
                model: "mock-model",
                env_key: "KODA_API_KEY",
                api_key: false,
            },
        }
    }

    /// Whether this provider requires an API key.
    pub fn requires_api_key(&self) -> bool {
        self.meta().api_key
    }
    /// Default API base URL for this provider.
    pub fn default_base_url(&self) -> &str {
        self.meta().url
    }
    /// Default model identifier for this provider.
    pub fn default_model(&self) -> &str {
        self.meta().model
    }
    /// Environment variable name for this provider's API key.
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
                #[cfg(any(test, feature = "test-support"))]
                "mock" => Self::Mock,
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
///
/// Place agent JSON files in:
/// - `agents/` (project-level, highest priority)
/// - `~/.config/koda/agents/` (user-level)
/// - Built-in (embedded at compile time, lowest priority)
///
/// ## Example
///
/// ```json
/// {
///   "name": "testgen",
///   "system_prompt": "You are a test generation specialist.",
///   "model": "gemini-2.5-flash",
///   "write_access": true,
///   "allowed_tools": ["Read", "Write", "Edit", "Bash", "Grep", "Glob"],
///   "max_iterations": 20
/// }
/// ```
///
/// ## Fields
///
/// - **`name`** — Agent identifier (used with `InvokeAgent`)
/// - **`system_prompt`** — Behavioral instructions for the LLM
/// - **`allowed_tools`** — Allowlist (empty = all tools available)
/// - **`disallowed_tools`** — Denylist (excluded even if `allowed_tools` is empty)
/// - **`model`** — Override the default model (e.g. `"gemini-2.5-flash"` for cheap workers)
/// - **`write_access`** — Grant Write/Edit/Delete tools (default: `false`)
/// - **`max_iterations`** — Cap inference loops (prevents runaway agents)
#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    /// Agent identifier.
    pub name: String,
    /// One-line description shown in the main agent's system prompt listing.
    /// Optional — agents without a description are listed by name only.
    #[serde(default)]
    pub description: Option<String>,
    /// System prompt template.
    pub system_prompt: String,
    /// Allowlisted tool names (empty = all tools).
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// Denylisted tool names — excluded even if `allowed_tools` is empty.
    #[serde(default)]
    pub disallowed_tools: Vec<String>,
    /// Override model identifier.
    #[serde(default)]
    pub model: Option<String>,
    /// Override API base URL.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Override provider type.
    #[serde(default)]
    pub provider: Option<String>,
    /// Override max output tokens.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Override temperature.
    #[serde(default)]
    pub temperature: Option<f64>,
    /// Override thinking budget (Anthropic extended thinking).
    #[serde(default)]
    pub thinking_budget: Option<u32>,
    /// Override reasoning effort (OpenAI reasoning models).
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Override max context window tokens.
    #[serde(default)]
    pub max_context_tokens: Option<usize>,
    /// Override max inference iterations.
    #[serde(default)]
    pub max_iterations: Option<u32>,
    /// Grant write access (Write/Edit/Delete tools). Default: false.
    /// Sub-agents are read-only by default (principle of least privilege).
    /// Set to `true` for agents that need to create or modify files.
    #[serde(default)]
    pub write_access: bool,
    /// Skip injecting project/global memory into the system prompt. Default: false.
    /// Read-only agents (explore, plan) don't need memory context — skipping it
    /// saves tokens without affecting their ability to search the codebase.
    #[serde(default)]
    pub skip_memory: bool,
}

/// Runtime configuration assembled from CLI args, env vars, and agent JSON.
#[derive(Debug, Clone)]
pub struct KodaConfig {
    /// Agent name (e.g. `"koda"`, `"scout"`).
    pub agent_name: String,
    /// Assembled system prompt.
    pub system_prompt: String,
    /// Allowlisted tool names (empty = all tools).
    pub allowed_tools: Vec<String>,
    /// Denylisted tool names.
    pub disallowed_tools: Vec<String>,
    /// Active provider type.
    pub provider_type: ProviderType,
    /// API base URL.
    pub base_url: String,
    /// Model identifier.
    pub model: String,
    /// Max context window tokens.
    pub max_context_tokens: usize,
    /// Directory containing agent JSON configs.
    pub agents_dir: PathBuf,
    /// Model-specific settings (max_tokens, temperature, etc.).
    pub model_settings: ModelSettings,
    /// Max inference iterations per turn.
    pub max_iterations: u32,
    /// Skip injecting project/global memory into the system prompt.
    /// Set by `skip_memory: true` in agent JSON. Default: `false`.
    pub skip_memory: bool,
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

        let max_iterations = agent.max_iterations.unwrap_or(200);

        Ok(Self {
            agent_name: agent.name,
            system_prompt: agent.system_prompt,
            allowed_tools: agent.allowed_tools,
            disallowed_tools: Self::apply_default_deny(agent.disallowed_tools, agent.write_access),
            provider_type,
            base_url,
            model: model.clone(),
            max_context_tokens,
            agents_dir,
            model_settings: settings,
            max_iterations,
            skip_memory: agent.skip_memory,
        })
    }

    /// Write tools that are blocked by default for sub-agents.
    /// Sub-agents must opt in with `"write_access": true` in their JSON config.
    const WRITE_TOOLS: &'static [&'static str] = &["Write", "Edit", "Delete"];

    /// Apply default-deny for write tools. If `write_access` is false,
    /// inject Write/Edit/Delete into disallowed_tools (deduped).
    fn apply_default_deny(mut disallowed: Vec<String>, write_access: bool) -> Vec<String> {
        if !write_access {
            for tool in Self::WRITE_TOOLS {
                let name = tool.to_string();
                if !disallowed.contains(&name) {
                    disallowed.push(name);
                }
            }
        }
        disallowed
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

    /// Recalculate model-derived settings (context window, tier, iteration limits).
    ///
    /// Call this whenever `self.model` or `self.provider_type` changes to keep
    /// context window, tier, and iteration defaults in sync with the new model.
    /// Uses the hardcoded lookup table as a synchronous fallback.
    /// For API-sourced values, call `apply_provider_capabilities` after this.
    pub fn recalculate_model_derived(&mut self) {
        let new_ctx = crate::model_context::context_window_for_model(&self.model);
        self.max_context_tokens = new_ctx;
        self.model_settings.max_context_tokens = new_ctx;

        self.max_iterations = 200;
    }

    /// Apply capabilities queried from the provider API.
    ///
    /// Overrides the hardcoded context window and max output tokens with
    /// values reported by the provider. Call this after `recalculate_model_derived`
    /// when you have access to the provider.
    pub fn apply_provider_capabilities(&mut self, caps: &crate::providers::ModelCapabilities) {
        if let Some(ctx) = caps.context_window {
            self.max_context_tokens = ctx;
            self.model_settings.max_context_tokens = ctx;
            tracing::info!("Context window from API: {} tokens for {}", ctx, self.model);
        }
        if let Some(max_out) = caps.max_output_tokens {
            // Only override if not explicitly set by the user/agent config
            if self.model_settings.max_tokens.is_none() {
                self.model_settings.max_tokens = Some(max_out as u32);
                tracing::info!("Max output tokens from API: {} for {}", max_out, self.model);
            }
        }
    }

    /// Query the provider API for model capabilities and apply them.
    ///
    /// Convenience wrapper: queries `model_capabilities()` on the provider
    /// and applies the result. Logs a debug message if the API doesn't
    /// report capabilities (falls back to hardcoded lookup).
    pub async fn query_and_apply_capabilities(
        &mut self,
        provider: &dyn crate::providers::LlmProvider,
    ) {
        match provider.model_capabilities(&self.model).await {
            Ok(caps) if caps.context_window.is_some() || caps.max_output_tokens.is_some() => {
                self.apply_provider_capabilities(&caps);
            }
            Ok(_) => {
                tracing::debug!(
                    "Provider did not report capabilities for {}; using lookup table ({}k tokens)",
                    self.model,
                    self.max_context_tokens / 1000
                );
            }
            Err(e) => {
                tracing::debug!("Could not query model capabilities: {e:#}");
            }
        }
    }

    /// Built-in agent configs, embedded at compile time.
    /// These are always available regardless of disk state.
    const BUILTIN_AGENTS: &[(&str, &str)] = &[
        ("default", include_str!("../agents/default.json")),
        ("task", include_str!("../agents/task.json")),
        ("explore", include_str!("../agents/explore.json")),
        ("plan", include_str!("../agents/plan.json")),
        ("verify", include_str!("../agents/verify.json")),
    ];

    /// Load the raw `AgentConfig` for an agent without resolving it into a
    /// full `KodaConfig`. Preserves `Option<String>` fields so callers can
    /// distinguish "explicitly set" from "not set" — used by sub-agent
    /// dispatch to decide which parent fields to inherit.
    ///
    /// Search order mirrors `load`: project agents/ → built-in.
    pub fn load_agent_json(project_root: &Path, agent_name: &str) -> Result<AgentConfig> {
        let agents_dir =
            Self::find_agents_dir(project_root).unwrap_or_else(|_| PathBuf::from("agents"));
        let agent_file = agents_dir.join(format!("{agent_name}.json"));
        if agent_file.exists() {
            let json = std::fs::read_to_string(&agent_file)
                .with_context(|| format!("Failed to read agent config: {agent_file:?}"))?;
            serde_json::from_str(&json)
                .with_context(|| format!("Failed to parse agent config: {agent_file:?}"))
        } else {
            Self::load_builtin(agent_name)
                .ok_or_else(|| anyhow::anyhow!("Agent '{agent_name}' not found"))
        }
    }

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

        Self {
            agent_name: "test".to_string(),
            system_prompt: "You are a test agent.".to_string(),
            allowed_tools: Vec::new(),
            disallowed_tools: Vec::new(),
            base_url: provider_type.default_base_url().to_string(),
            model,
            provider_type,
            max_context_tokens,
            agents_dir: PathBuf::from("agents"),
            model_settings,
            max_iterations: crate::loop_guard::MAX_ITERATIONS_DEFAULT,
            skip_memory: false,
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
            ProviderType::Mock,
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
            "allowed_tools": ["Read", "Write"],
            "write_access": true
        }"#,
        )
        .unwrap();
        let config = KodaConfig::load(tmp.path(), "test").unwrap();
        assert_eq!(config.agent_name, "test");
        assert_eq!(config.allowed_tools, vec!["Read", "Write"]);
        assert!(config.disallowed_tools.is_empty());
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

    // ── Default-deny write access ─────────────────────────────

    #[test]
    fn test_default_deny_blocks_write_tools() {
        let result = KodaConfig::apply_default_deny(vec![], false);
        assert!(result.contains(&"Write".to_string()));
        assert!(result.contains(&"Edit".to_string()));
        assert!(result.contains(&"Delete".to_string()));
    }

    #[test]
    fn test_write_access_true_allows_write_tools() {
        let result = KodaConfig::apply_default_deny(vec![], true);
        assert!(result.is_empty());
    }

    #[test]
    fn test_default_deny_deduplicates() {
        // If Write is already in disallowed, don't add it again
        let result =
            KodaConfig::apply_default_deny(vec!["Write".to_string(), "Bash".to_string()], false);
        assert_eq!(result.iter().filter(|t| *t == "Write").count(), 1);
        assert!(result.contains(&"Edit".to_string()));
        assert!(result.contains(&"Delete".to_string()));
        assert!(result.contains(&"Bash".to_string()));
    }

    #[test]
    fn test_custom_agent_without_write_access_is_readonly() {
        let tmp = TempDir::new().unwrap();
        let agents_dir = tmp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("custom.json"),
            r#"{
                "name": "custom",
                "system_prompt": "I am custom."
            }"#,
        )
        .unwrap();
        let config = KodaConfig::load(tmp.path(), "custom").unwrap();
        assert!(config.disallowed_tools.contains(&"Write".to_string()));
        assert!(config.disallowed_tools.contains(&"Edit".to_string()));
        assert!(config.disallowed_tools.contains(&"Delete".to_string()));
    }

    #[test]
    fn test_builtin_task_has_write_access() {
        let agent = KodaConfig::load_builtin("task").unwrap();
        assert!(agent.write_access, "task agent should have write_access");
    }

    #[test]
    fn test_builtin_explore_no_write_access() {
        let agent = KodaConfig::load_builtin("explore").unwrap();
        assert!(!agent.write_access, "explore should be read-only");
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
        assert_eq!(config.model, "gemini-flash-latest");
    }

    // ── recalculate_model_derived ──────────────────────────────

    #[test]
    fn test_recalculate_updates_context_window() {
        // Start with LMStudio auto-detect (4096 tokens)
        let mut config = KodaConfig::default_for_testing(ProviderType::LMStudio);
        assert_eq!(config.max_context_tokens, 4_096); // MIN_CONTEXT for auto-detect

        // Switch to Claude Sonnet
        config.model = "claude-sonnet-4-6".to_string();
        config.model_settings.model = config.model.clone();
        config.provider_type = ProviderType::Anthropic;
        config.recalculate_model_derived();

        assert_eq!(config.max_context_tokens, 200_000);
        assert_eq!(config.model_settings.max_context_tokens, 200_000);
        assert_eq!(config.max_iterations, 200);
    }

    #[test]
    fn test_with_overrides_model_recalculates() {
        let config = KodaConfig::default_for_testing(ProviderType::LMStudio);
        assert_eq!(config.max_context_tokens, 4_096);

        let config = config.with_overrides(None, Some("gpt-4o".into()), Some("openai".into()));
        assert_eq!(config.model, "gpt-4o");
        assert_eq!(config.max_context_tokens, 128_000);
    }

    // ── URL-based provider detection (remaining providers) ─────────────────

    #[test]
    fn test_provider_from_url_ollama() {
        assert_eq!(
            ProviderType::from_url_or_name("http://localhost:11434/api", None),
            ProviderType::Ollama
        );
    }

    #[test]
    fn test_provider_from_url_vllm() {
        assert_eq!(
            ProviderType::from_url_or_name("http://localhost:8000/v1", None),
            ProviderType::Vllm
        );
    }

    #[test]
    fn test_provider_from_url_gemini() {
        assert_eq!(
            ProviderType::from_url_or_name(
                "https://generativelanguage.googleapis.com/v1beta",
                None
            ),
            ProviderType::Gemini
        );
    }

    #[test]
    fn test_provider_from_url_groq() {
        assert_eq!(
            ProviderType::from_url_or_name("https://api.groq.com/openai/v1", None),
            ProviderType::Groq
        );
    }

    #[test]
    fn test_provider_from_url_grok() {
        assert_eq!(
            ProviderType::from_url_or_name("https://api.x.ai/v1", None),
            ProviderType::Grok
        );
    }

    #[test]
    fn test_provider_from_url_deepseek() {
        assert_eq!(
            ProviderType::from_url_or_name("https://api.deepseek.com/v1", None),
            ProviderType::DeepSeek
        );
    }

    #[test]
    fn test_provider_from_url_mistral() {
        assert_eq!(
            ProviderType::from_url_or_name("https://api.mistral.ai/v1", None),
            ProviderType::Mistral
        );
    }

    #[test]
    fn test_provider_from_url_openrouter() {
        assert_eq!(
            ProviderType::from_url_or_name("https://openrouter.ai/api/v1", None),
            ProviderType::OpenRouter
        );
    }

    #[test]
    fn test_provider_from_url_together() {
        assert_eq!(
            ProviderType::from_url_or_name("https://api.together.xyz/v1", None),
            ProviderType::Together
        );
    }

    #[test]
    fn test_provider_from_url_fireworks() {
        assert_eq!(
            ProviderType::from_url_or_name("https://api.fireworks.ai/inference/v1", None),
            ProviderType::Fireworks
        );
    }

    // ── Name alias coverage (remaining aliases) ─────────────────────────

    #[test]
    fn test_provider_name_aliases_extended() {
        let cases = [
            ("ollama", ProviderType::Ollama),
            ("deepseek", ProviderType::DeepSeek),
            ("mistral", ProviderType::Mistral),
            ("minimax", ProviderType::MiniMax),
            ("openrouter", ProviderType::OpenRouter),
            ("together", ProviderType::Together),
            ("fireworks", ProviderType::Fireworks),
            ("vllm", ProviderType::Vllm),
            ("groq", ProviderType::Groq),
            ("mock", ProviderType::Mock),
        ];
        for (name, expected) in cases {
            assert_eq!(
                ProviderType::from_url_or_name("", Some(name)),
                expected,
                "alias '{name}' failed"
            );
        }
    }

    // ── requires_api_key ───────────────────────────────────────────────

    #[test]
    fn test_requires_api_key_local_providers() {
        // Local providers don't require an API key
        assert!(!ProviderType::LMStudio.requires_api_key());
        assert!(!ProviderType::Ollama.requires_api_key());
        assert!(!ProviderType::Mock.requires_api_key());
        assert!(!ProviderType::Vllm.requires_api_key());
    }

    #[test]
    fn test_requires_api_key_cloud_providers() {
        assert!(ProviderType::Anthropic.requires_api_key());
        assert!(ProviderType::OpenAI.requires_api_key());
        assert!(ProviderType::Gemini.requires_api_key());
        assert!(ProviderType::Groq.requires_api_key());
        assert!(ProviderType::Grok.requires_api_key());
    }

    // ── ModelSettings::defaults_for ─────────────────────────────────────

    #[test]
    fn test_model_settings_defaults_anthropic_has_max_tokens() {
        let s = ModelSettings::defaults_for("claude-opus-4-5", &ProviderType::Anthropic);
        assert_eq!(s.max_tokens, Some(16384));
        assert_eq!(s.model, "claude-opus-4-5");
        assert!(s.temperature.is_none());
    }

    #[test]
    fn test_model_settings_defaults_openai_no_max_tokens() {
        let s = ModelSettings::defaults_for("gpt-4o", &ProviderType::OpenAI);
        assert!(s.max_tokens.is_none(), "OpenAI should use provider default");
        assert_eq!(s.model, "gpt-4o");
    }

    // ── with_model_overrides ──────────────────────────────────────────

    #[test]
    fn test_with_model_overrides_all_fields() {
        let config = KodaConfig::default_for_testing(ProviderType::Anthropic).with_model_overrides(
            Some(8192),         // max_tokens
            Some(0.7),          // temperature
            Some(2000),         // thinking_budget
            Some("low".into()), // reasoning_effort
        );
        assert_eq!(config.model_settings.max_tokens, Some(8192));
        assert_eq!(config.model_settings.temperature, Some(0.7));
        assert_eq!(config.model_settings.thinking_budget, Some(2000));
        assert_eq!(
            config.model_settings.reasoning_effort,
            Some("low".to_string())
        );
    }

    #[test]
    fn test_with_model_overrides_none_changes_nothing() {
        let original = KodaConfig::default_for_testing(ProviderType::OpenAI);
        let original_tokens = original.model_settings.max_tokens;
        let config = original.with_model_overrides(None, None, None, None);
        assert_eq!(config.model_settings.max_tokens, original_tokens);
        assert!(config.model_settings.temperature.is_none());
    }

    // ── builtin_agents ─────────────────────────────────────────────────

    #[test]
    fn test_builtin_agents_is_not_empty() {
        let agents = KodaConfig::builtin_agents();
        assert!(!agents.is_empty(), "builtin_agents should not be empty");
    }

    #[test]
    fn test_builtin_agents_contains_core_agents() {
        let agents = KodaConfig::builtin_agents();
        let names: Vec<&str> = agents.iter().map(|(name, _)| name.as_str()).collect();
        assert!(names.contains(&"task"), "should have 'task' agent");
        assert!(names.contains(&"explore"), "should have 'explore' agent");
    }

    // ── load_agent_json ───────────────────────────────────────────────────

    #[test]
    fn test_load_agent_json_returns_raw_options() {
        // Built-in agents must not hardcode a model or provider so that
        // sub-agent dispatch can inherit these from the parent session.
        let tmp = tempfile::TempDir::new().unwrap();
        for name in ["explore", "plan", "verify", "task"] {
            let raw = KodaConfig::load_agent_json(tmp.path(), name)
                .unwrap_or_else(|e| panic!("load_agent_json({name}) failed: {e}"));
            assert!(
                raw.model.is_none(),
                "built-in agent '{name}' must not hardcode a model — \
                 set it in the agent JSON if you need a provider-specific default"
            );
            assert!(
                raw.provider.is_none(),
                "built-in agent '{name}' must not hardcode a provider"
            );
        }
    }

    #[test]
    fn test_load_agent_json_project_override_preserves_option() {
        // A project-local agent that explicitly sets a model must preserve it.
        let tmp = tempfile::TempDir::new().unwrap();
        let agents_dir = tmp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("myscout.json"),
            r#"{"name":"myscout","system_prompt":"scout","model":"claude-3-haiku"}"#,
        )
        .unwrap();
        let raw = KodaConfig::load_agent_json(tmp.path(), "myscout").unwrap();
        assert_eq!(raw.model.as_deref(), Some("claude-3-haiku"));
    }

    // ── sub-agent model inheritance ───────────────────────────────────────

    /// Simulate the dispatch logic: a parent on Gemini with a specific model
    /// should be fully inherited by a sub-agent that sets neither provider nor model.
    #[test]
    fn test_sub_agent_inherits_parent_provider_and_model() {
        let tmp = tempfile::TempDir::new().unwrap();

        // Parent is on Gemini with a specific model
        let parent = KodaConfig::default_for_testing(ProviderType::Gemini).with_overrides(
            None,
            Some("gemini-2.0-flash".to_string()),
            None,
        );

        // Load explore and apply the inheritance logic from sub_agent_dispatch
        let raw = KodaConfig::load_agent_json(tmp.path(), "explore").unwrap();
        let mut cfg = KodaConfig::load(tmp.path(), "explore").unwrap();

        // Mirrors sub_agent_dispatch: inherit everything when agent sets no provider
        let agent_has_own_provider = raw.provider.is_some() || raw.base_url.is_some();
        if !agent_has_own_provider {
            let model_override = raw.model.is_none().then(|| parent.model.clone());
            cfg = cfg.with_overrides(
                Some(parent.base_url.clone()),
                model_override,
                Some(parent.provider_type.to_string()),
            );
        }

        assert_eq!(
            cfg.provider_type,
            ProviderType::Gemini,
            "provider must be inherited"
        );
        assert_eq!(
            cfg.model, "gemini-2.0-flash",
            "model must be inherited from parent"
        );
    }

    /// A sub-agent with its own provider must keep its routing even when the
    /// parent uses a different provider — the JSON opt-in wins.
    #[test]
    fn test_sub_agent_own_provider_is_not_overridden() {
        let tmp = tempfile::TempDir::new().unwrap();
        let agents_dir = tmp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        // Agent explicitly sets its own provider (e.g. a mock / local specialist)
        std::fs::write(
            agents_dir.join("local-scout.json"),
            r#"{"name":"local-scout","system_prompt":"s","provider":"lmstudio","base_url":"http://localhost:1234/v1"}"#,
        )
        .unwrap();

        let parent = KodaConfig::default_for_testing(ProviderType::Gemini).with_overrides(
            None,
            Some("gemini-2.0-flash".to_string()),
            None,
        );

        let raw = KodaConfig::load_agent_json(tmp.path(), "local-scout").unwrap();
        let mut cfg = KodaConfig::load(tmp.path(), "local-scout").unwrap();

        let agent_has_own_provider = raw.provider.is_some() || raw.base_url.is_some();
        if !agent_has_own_provider {
            let model_override = raw.model.is_none().then(|| parent.model.clone());
            cfg = cfg.with_overrides(
                Some(parent.base_url.clone()),
                model_override,
                Some(parent.provider_type.to_string()),
            );
        }

        // Agent's own provider must be preserved
        assert_eq!(cfg.provider_type, ProviderType::LMStudio);
        assert_ne!(
            cfg.provider_type,
            ProviderType::Gemini,
            "parent provider must not bleed into agent with explicit provider"
        );
    }

    /// A sub-agent that explicitly sets its own model must keep it even when
    /// the parent has a different model — the JSON preference wins.
    #[test]
    fn test_sub_agent_explicit_model_is_not_overridden() {
        let tmp = tempfile::TempDir::new().unwrap();
        let agents_dir = tmp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        // Agent sets model preference but no explicit provider
        std::fs::write(
            agents_dir.join("specialist.json"),
            r#"{"name":"specialist","system_prompt":"s","model":"gemini-2.5-flash"}"#,
        )
        .unwrap();

        let parent = KodaConfig::default_for_testing(ProviderType::Gemini).with_overrides(
            None,
            Some("gemini-2.0-flash-lite".to_string()),
            None,
        );

        let raw = KodaConfig::load_agent_json(tmp.path(), "specialist").unwrap();
        let mut cfg = KodaConfig::load(tmp.path(), "specialist").unwrap();

        let agent_has_own_provider = raw.provider.is_some() || raw.base_url.is_some();
        if !agent_has_own_provider {
            let model_override = raw.model.is_none().then(|| parent.model.clone());
            cfg = cfg.with_overrides(
                Some(parent.base_url.clone()),
                model_override,
                Some(parent.provider_type.to_string()),
            );
        }

        // Provider inherited, but agent's own model is kept
        assert_eq!(cfg.provider_type, ProviderType::Gemini);
        assert_eq!(
            cfg.model, "gemini-2.5-flash",
            "agent's explicit model must not be overridden by parent"
        );
    }
}
