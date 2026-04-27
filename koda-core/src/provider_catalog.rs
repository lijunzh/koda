//! Provider catalog — static metadata for every supported LLM provider.
//!
//! Spun out of `config.rs` in #1082 to separate two unrelated concerns:
//!
//! - **This file (`provider_catalog.rs`)** — static, compile-time data.
//!   `ProviderMeta` struct + `ProviderType` enum + the `meta()` lookup
//!   table + URL/name auto-detection. No I/O, no env-var reads, no
//!   file-system access. Pure data.
//!
//! - **`config.rs`** — runtime concerns. `ModelSettings`, `AgentConfig`,
//!   `KodaConfig`, file loading, env merging, validation. Reads
//!   `ProviderMeta` from here but owns nothing about it.
//!
//! ## Adding a new provider
//!
//! 1. Add a variant to `ProviderType`.
//! 2. Add the corresponding arm in `ProviderType::meta()`.
//! 3. (Optional) Add a name alias in `from_url_or_name()` if the
//!    provider has common alternate spellings (e.g. `claude` →
//!    `Anthropic`).
//! 4. (Optional) Add a URL fingerprint in `from_url_or_name()` if
//!    the provider's base URL is auto-detectable.
//! 5. Add a row to `koda-core/tests/snapshot_test.rs` so the
//!    metadata gets pinned by regression test.
//!
//! That's the entire surface — no separate registration, no
//! plugin system, no factory. Per `DESIGN.md § P1: Personal`,
//! a `match` arm is the canonical extension point.

use serde::Deserialize;

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
