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
//! 5. Add a row to `koda-core/tests/suite/snapshot_test.rs` so the
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
    /// llama.cpp's `llama-server` (local, OpenAI-compatible).
    LlamaCpp,
}

impl ProviderType {
    /// Consolidated provider metadata.
    pub fn meta(&self) -> ProviderMeta {
        match self {
            Self::OpenAI => ProviderMeta {
                name: "openai",
                url: "https://api.openai.com/v1",
                model: "gpt-6-astra",
                env_key: "OPENAI_API_KEY",
                api_key: true,
            },
            Self::Anthropic => ProviderMeta {
                name: "anthropic",
                url: "https://api.anthropic.com",
                model: "claude-sonnet-5-5",
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
                model: "grok-4.7",
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
                model: "anthropic/claude-sonnet-5.5",
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
            Self::LlamaCpp => ProviderMeta {
                name: "llama-cpp",
                url: "http://localhost:8080/v1",
                model: "auto-detect",
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

    /// Whether this kind of provider is a server the user runs (LM Studio,
    /// Ollama, vLLM, llama.cpp), not a company's API. The kind alone: where
    /// a session's endpoint points is [`Self::runs_locally`]'s to say.
    pub fn is_local(&self) -> bool {
        match self {
            Self::LMStudio | Self::Ollama | Self::Vllm | Self::LlamaCpp => true,
            Self::OpenAI
            | Self::Anthropic
            | Self::Gemini
            | Self::Groq
            | Self::Grok
            | Self::DeepSeek
            | Self::Mistral
            | Self::MiniMax
            | Self::OpenRouter
            | Self::Together
            | Self::Fireworks => false,
        }
    }

    /// Whether a model of this provider at `base_url` runs on the user's
    /// own machine or network: a local kind ([`Self::is_local`]) whose
    /// endpoint is a local one ([`is_local_endpoint`]). Without a
    /// `base_url`, the kind's own endpoint. A confidential session's text
    /// may be given to such a model and to no other.
    pub fn runs_locally(&self, base_url: Option<&str>) -> bool {
        self.is_local() && is_local_endpoint(base_url.unwrap_or(self.default_base_url()))
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
                "llama-cpp" | "llamacpp" | "llama.cpp" => Self::LlamaCpp,
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
        } else if url.contains("localhost:8080") || url.contains("127.0.0.1:8080") {
            Self::LlamaCpp
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

impl ProviderType {
    /// The providers Koda offers, in the order a picker shows them.
    pub const OFFERED: [Self; 15] = [
        Self::LMStudio,
        Self::Ollama,
        Self::LlamaCpp,
        Self::OpenAI,
        Self::Anthropic,
        Self::DeepSeek,
        Self::Gemini,
        Self::Groq,
        Self::Grok,
        Self::Mistral,
        Self::MiniMax,
        Self::OpenRouter,
        Self::Together,
        Self::Fireworks,
        Self::Vllm,
    ];

    /// The name a person reads, such as `LM Studio`. The id is
    /// its `to_string()` (`lm-studio`), which [`Self::from_url_or_name`]
    /// reads back.
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::OpenAI => "OpenAI",
            Self::Anthropic => "Anthropic",
            Self::LMStudio => "LM Studio",
            Self::Gemini => "Google Gemini",
            Self::Groq => "Groq",
            Self::Grok => "Grok (xAI)",
            Self::Ollama => "Ollama",
            Self::DeepSeek => "DeepSeek",
            Self::Mistral => "Mistral",
            Self::MiniMax => "MiniMax",
            Self::OpenRouter => "OpenRouter",
            Self::Together => "Together",
            Self::Fireworks => "Fireworks",
            Self::Vllm => "vLLM",
            Self::LlamaCpp => "llama.cpp",
        }
    }
}

/// Host names that stay on the user's own network: mDNS (`.local`), the
/// loopback names, the home-network names (`.lan`, RFC 8375's
/// `.home.arpa`) and Tailscale's (`.ts.net`).
pub const LOCAL_NAME_ENDINGS: [&str; 5] = [".local", ".localhost", ".lan", ".home.arpa", ".ts.net"];

/// Whether `base_url` points at the user's own machine or network, told
/// from its host alone, with no lookup:
///
/// - a loopback address, or `localhost`;
/// - a private (10/8, 172.16/12, 192.168/16), link-local (169.254/16,
///   `fe80::/10`), unique-local (`fc00::/7`) or carrier-grade-NAT
///   (100.64/10, which Tailscale uses) address;
/// - a host name of one label (`studio`);
/// - a name ending in one of [`LOCAL_NAME_ENDINGS`].
///
/// Anything else is not, and neither is a URL that doesn't parse, has no
/// host or isn't `http` or `https`. A name can still resolve anywhere: this says what the user wrote,
/// which is what they chose.
pub fn is_local_endpoint(base_url: &str) -> bool {
    use reqwest::Url;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    fn v4(ip: Ipv4Addr) -> bool {
        let [a, b, ..] = ip.octets();
        ip.is_loopback() || ip.is_private() || ip.is_link_local() || (a == 100 && b & 0xc0 == 64)
    }
    fn v6(ip: Ipv6Addr) -> bool {
        if let Some(mapped) = ip.to_ipv4_mapped() {
            return v4(mapped);
        }
        let first = ip.segments()[0];
        ip.is_loopback() || first & 0xfe00 == 0xfc00 || first & 0xffc0 == 0xfe80
    }
    let Ok(url) = Url::parse(base_url.trim()) else {
        return false;
    };
    // For `http` and `https` the parser reads an address however it is
    // written (`0x7f.1`, `2130706433`), so a number can't pass for a
    // one-label name; another scheme's host is left as written.
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let address = host.trim_start_matches('[').trim_end_matches(']');
    match address.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => v4(ip),
        Ok(IpAddr::V6(ip)) => v6(ip),
        Err(_) => {
            !host.is_empty()
                && (!host.contains('.')
                    || LOCAL_NAME_ENDINGS
                        .iter()
                        .any(|ending| host.ends_with(ending)))
        }
    }
}

/// Whether two endpoints are the same address, a trailing `/` aside.
pub fn same_endpoint(a: &str, b: &str) -> bool {
    a.trim().trim_end_matches('/') == b.trim().trim_end_matches('/')
}

impl std::fmt::Display for ProviderType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.meta().name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A provider's kind is local exactly when its own endpoint is on
    /// this machine: the two are written apart, and a new provider must
    /// agree. Without an endpoint of its own a session runs where its kind
    /// does.
    #[test]
    fn a_provider_is_local_when_its_default_endpoint_is_this_machine() {
        for provider in ProviderType::OFFERED {
            let on_this_machine = provider.default_base_url().starts_with("http://localhost:");
            assert_eq!(provider.is_local(), on_this_machine, "{provider}");
            assert_eq!(provider.runs_locally(None), on_this_machine, "{provider}");
        }
    }

    /// An endpoint is local by its host as written: this machine, an
    /// address of a private network, a one-label name or one of the
    /// home-network endings. Everything else is remote, including what
    /// only looks local.
    #[test]
    fn an_endpoint_is_local_by_its_host() {
        for (url, local) in [
            ("http://localhost:1234/v1", true),
            ("http://LOCALHOST:1234/v1", true),
            ("http://127.0.0.1:8080/v1", true),
            ("http://127.9.9.9", true),
            ("http://[::1]:11434", true),
            ("http://10.0.0.5:1234/v1", true),
            ("http://172.16.0.1/v1", true),
            ("http://172.31.255.255/v1", true),
            ("http://192.168.10.50:1234/v1", true),
            ("http://169.254.7.7/v1", true),
            ("http://100.64.0.1:1234/v1", true),
            ("http://100.127.255.254/v1", true),
            ("http://[fd12:3456::1]:1234/v1", true),
            ("http://[fe80::1]/v1", true),
            ("http://[::ffff:192.168.1.2]/v1", true),
            ("http://studio:1234/v1", true),
            ("http://studio.:1234/v1", true),
            ("http://studio.local:1234/v1", true),
            ("https://api.localhost/v1", true),
            ("http://nas.lan/v1", true),
            ("http://nas.home.arpa/v1", true),
            ("https://studio.tail1234.ts.net/v1", true),
            ("  http://localhost:1234/v1  ", true),
            // An address outside those ranges, however it is written.
            ("http://172.32.0.1/v1", false),
            ("http://172.15.0.1/v1", false),
            ("http://100.63.255.255/v1", false),
            ("http://100.128.0.1/v1", false),
            ("http://192.169.0.1/v1", false),
            ("http://8.8.8.8/v1", false),
            ("http://0.0.0.0:1234/v1", false),
            ("http://134744072/v1", false),
            ("http://0x8.0x8.0x8.0x8/v1", false),
            ("http://[2001:db8::1]/v1", false),
            ("http://[::ffff:8.8.8.8]/v1", false),
            // A name that only looks local.
            ("https://api.openai.com/v1", false),
            ("https://localhost.example.com/v1", false),
            ("https://example.local.evil.com/v1", false),
            ("https://example.notlan/v1", false),
            ("https://my.notts.net.example/v1", false),
            ("https://examplets.net/v1", false),
            ("https://example.com/localhost", false),
            ("http://localhost@example.com/v1", false),
            ("http://example.com#.local", false),
            ("http://example.com?x=.lan", false),
            // Not a URL with a host.
            ("localhost:1234/v1", false),
            ("studio.local/v1", false),
            ("", false),
            ("http://", false),
            ("file:///tmp/sock", false),
            // Another scheme's host isn't read as an address, so it isn't read.
            ("grpc://134744072/v1", false),
            ("grpc://studio/v1", false),
        ] {
            assert_eq!(is_local_endpoint(url), local, "{url:?}");
        }
        // The written decimal and hex forms of a loopback address are one.
        assert!(is_local_endpoint("http://2130706433/v1"));
        assert!(is_local_endpoint("http://0x7f.1/v1"));
    }

    /// A local kind runs locally only at a local endpoint; a remote kind,
    /// nowhere.
    #[test]
    fn a_local_kind_at_a_remote_endpoint_is_remote() {
        for (provider, url, local) in [
            (ProviderType::LMStudio, "http://192.168.10.50:1234/v1", true),
            (ProviderType::Ollama, "https://ollama.example.com", false),
            (ProviderType::LlamaCpp, "not a url", false),
            (ProviderType::OpenAI, "http://localhost:1234/v1", false),
            (ProviderType::Anthropic, "http://127.0.0.1:9", false),
        ] {
            assert_eq!(provider.runs_locally(Some(url)), local, "{provider} {url}");
        }
    }

    #[test]
    fn endpoints_are_the_same_but_for_a_trailing_slash() {
        assert!(same_endpoint("http://a/v1/", " http://a/v1"));
        assert!(!same_endpoint("http://a/v1", "http://b/v1"));
    }
}
