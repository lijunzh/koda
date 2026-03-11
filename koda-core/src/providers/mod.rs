//! LLM provider abstraction layer.
//!
//! Defines a common trait for all providers and re-exports the default.

pub mod anthropic;
pub mod gemini;
pub mod openai_compat;
pub mod stream_tag_filter;
/// Deprecated: use `stream_tag_filter` instead.
pub mod think_tag_filter;

pub mod mock;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// A tool call requested by the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub function_name: String,
    pub arguments: String, // Raw JSON string
    /// Gemini-specific: thought signature that must be echoed back in history.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub thought_signature: Option<String>,
}

/// Token usage from an LLM response.
#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    /// Tokens read from provider cache (e.g. Anthropic prompt caching, Gemini cached content).
    pub cache_read_tokens: i64,
    /// Tokens written to provider cache on this request.
    pub cache_creation_tokens: i64,
    /// Tokens used for reasoning/thinking (e.g. OpenAI reasoning_tokens, Anthropic thinking).
    pub thinking_tokens: i64,
    /// Why the model stopped: "end_turn", "max_tokens", "stop_sequence", etc.
    /// Empty string means unknown (provider didn't report it).
    pub stop_reason: String,
}

/// The LLM's response: either text, tool calls, or both.
#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: TokenUsage,
}

/// Base64-encoded image data for multi-modal messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageData {
    /// MIME type (e.g. "image/png", "image/jpeg").
    pub media_type: String,
    /// Base64-encoded image bytes.
    pub base64: String,
}

/// A single message in the conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Attached images (only used in-flight, not persisted to DB).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub images: Option<Vec<ImageData>>,
}

impl ChatMessage {
    /// Create a simple text message (convenience for the common case).
    pub fn text(role: &str, content: &str) -> Self {
        Self {
            role: role.to_string(),
            content: Some(content.to_string()),
            tool_calls: None,
            tool_call_id: None,
            images: None,
        }
    }
}

/// Tool definition sent to the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value, // JSON Schema
}

/// A discovered model from a provider.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub id: String,
    #[allow(dead_code)]
    pub owned_by: Option<String>,
}

/// Model capabilities queried from the provider API.
#[derive(Debug, Clone, Default)]
pub struct ModelCapabilities {
    /// Maximum context window in tokens (input + output).
    pub context_window: Option<usize>,
    /// Maximum output tokens the model supports.
    pub max_output_tokens: Option<usize>,
}

/// Is this URL pointing to a local address?
fn is_localhost_url(url: &str) -> bool {
    let lower = url.to_lowercase();
    lower.contains("://localhost") || lower.contains("://127.0.0.1") || lower.contains("://[::1]")
}

/// Build a reqwest client with proper proxy configuration.
///
/// - Reads HTTPS_PROXY / HTTP_PROXY from env
/// - Supports proxy auth via URL (http://user:pass@proxy:port)
/// - Supports separate PROXY_USER / PROXY_PASS env vars
/// - Bypasses proxy for localhost (LM Studio)
pub fn build_http_client(base_url: Option<&str>) -> reqwest::Client {
    let mut builder = reqwest::Client::builder();

    let proxy_url = crate::runtime_env::get("HTTPS_PROXY")
        .or_else(|| crate::runtime_env::get("HTTP_PROXY"))
        .or_else(|| crate::runtime_env::get("https_proxy"))
        .or_else(|| crate::runtime_env::get("http_proxy"));

    if let Some(ref url) = proxy_url
        && !url.is_empty()
    {
        match reqwest::Proxy::all(url) {
            Ok(mut proxy) => {
                // Bypass proxy for local addresses
                proxy = proxy.no_proxy(reqwest::NoProxy::from_string("localhost,127.0.0.1,::1"));

                // If URL doesn't contain creds, check env vars
                if !url.contains('@') {
                    let user = crate::runtime_env::get("PROXY_USER");
                    let pass = crate::runtime_env::get("PROXY_PASS");
                    if let (Some(u), Some(p)) = (user, pass) {
                        proxy = proxy.basic_auth(&u, &p);
                        tracing::debug!("Using proxy with basic auth (credentials redacted)");
                    }
                }

                builder = builder.proxy(proxy);
                tracing::debug!("Using proxy: {}", redact_url_credentials(url));
            }
            Err(e) => {
                tracing::warn!("Invalid proxy URL '{}': {e}", redact_url_credentials(url));
            }
        }
    }

    // Accept self-signed certs only for localhost (LM Studio, Ollama, vLLM).
    // The env var is still required, but it's now scoped to local addresses.
    let wants_skip_tls = crate::runtime_env::get("KODA_ACCEPT_INVALID_CERTS")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);
    let is_local = base_url.is_some_and(is_localhost_url);
    if wants_skip_tls && is_local {
        tracing::info!("TLS certificate validation disabled for local provider.");
        builder = builder.danger_accept_invalid_certs(true);
    } else if wants_skip_tls {
        tracing::warn!(
            "KODA_ACCEPT_INVALID_CERTS is set but provider URL is not localhost — ignoring. \
             TLS bypass is only allowed for local providers (localhost/127.0.0.1)."
        );
    }

    builder.build().unwrap_or_else(|_| reqwest::Client::new())
}

/// Redact embedded credentials from a URL.
///
/// `http://user:pass@proxy:8080` → `http://***:***@proxy:8080`
fn redact_url_credentials(url: &str) -> String {
    // Pattern: scheme://user:pass@host...
    if let Some(at_pos) = url.find('@')
        && let Some(scheme_end) = url.find("://")
    {
        let prefix = &url[..scheme_end + 3]; // "http://"
        let host_part = &url[at_pos..]; // "@proxy:8080/..."
        return format!("{prefix}***:***{host_part}");
    }
    url.to_string()
}

/// A streaming chunk from the LLM.
#[derive(Debug, Clone)]
pub enum StreamChunk {
    /// A text delta (partial content).
    TextDelta(String),
    /// A thinking/reasoning delta from native API (Anthropic extended thinking, OpenAI reasoning).
    ThinkingDelta(String),
    /// A tool call was returned (streaming ends, need full response).
    ToolCalls(Vec<ToolCall>),
    /// Stream finished with usage info.
    Done(TokenUsage),
}

/// Trait for LLM provider backends.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Send a chat completion request (non-streaming).
    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        settings: &crate::config::ModelSettings,
    ) -> Result<LlmResponse>;

    /// Send a streaming chat completion request.
    /// Returns a channel receiver that yields chunks as they arrive.
    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        settings: &crate::config::ModelSettings,
    ) -> Result<tokio::sync::mpsc::Receiver<StreamChunk>>;

    /// List available models from the provider.
    async fn list_models(&self) -> Result<Vec<ModelInfo>>;

    /// Query model capabilities (context window, max output tokens) from the API.
    ///
    /// Returns `Ok(caps)` with whatever the provider reports. Fields are `None`
    /// when the API doesn't expose them. Callers should fall back to the
    /// hardcoded lookup table for any `None` fields.
    async fn model_capabilities(&self, _model: &str) -> Result<ModelCapabilities> {
        Ok(ModelCapabilities::default())
    }

    /// Provider display name (for UI).
    fn provider_name(&self) -> &str;
}

// ── Provider factory ──────────────────────────────────────────

use crate::config::{KodaConfig, ProviderType};

/// Create an LLM provider from the given configuration.
pub fn create_provider(config: &KodaConfig) -> Box<dyn LlmProvider> {
    let api_key = crate::runtime_env::get(config.provider_type.env_key_name());
    match config.provider_type {
        ProviderType::Anthropic => {
            let key = api_key.unwrap_or_else(|| {
                tracing::warn!("No ANTHROPIC_API_KEY set");
                String::new()
            });
            Box::new(anthropic::AnthropicProvider::new(
                key,
                Some(&config.base_url),
            ))
        }
        ProviderType::Gemini => {
            let key = api_key.unwrap_or_else(|| {
                tracing::warn!("No GEMINI_API_KEY set");
                String::new()
            });
            Box::new(gemini::GeminiProvider::new(key, Some(&config.base_url)))
        }
        ProviderType::Mock => Box::new(mock::MockProvider::from_env()),
        _ => Box::new(openai_compat::OpenAiCompatProvider::new(
            &config.base_url,
            api_key,
        )),
    }
}
