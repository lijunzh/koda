//! LLM provider abstraction layer.
//!
//! Defines a common trait for all providers and re-exports the default.

pub mod anthropic;
pub mod gemini;
pub mod openai_compat;
pub mod think_tag_filter;

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

/// Build a reqwest client with proper proxy configuration.
///
/// - Reads HTTPS_PROXY / HTTP_PROXY from env
/// - Supports proxy auth via URL (http://user:pass@proxy:port)
/// - Supports separate PROXY_USER / PROXY_PASS env vars
/// - Bypasses proxy for localhost (LM Studio)
pub fn build_http_client() -> reqwest::Client {
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
                        tracing::debug!("Using proxy with basic auth: {url}");
                    }
                }

                builder = builder.proxy(proxy);
                tracing::debug!("Using proxy: {url}");
            }
            Err(e) => {
                tracing::warn!("Invalid proxy URL '{url}': {e}");
            }
        }
    }

    // Accept self-signed certs if needed
    let accept_invalid_certs = crate::runtime_env::get("KODA_ACCEPT_INVALID_CERTS")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);
    if accept_invalid_certs {
        eprintln!(
            "  \x1b[33m\u{26a0} WARNING: TLS certificate validation is DISABLED (KODA_ACCEPT_INVALID_CERTS=1)\x1b[0m"
        );
        eprintln!("    API keys and conversation data may be intercepted.");
        tracing::warn!("TLS certificate validation disabled!");
    }
    builder = builder.danger_accept_invalid_certs(accept_invalid_certs);

    builder.build().unwrap_or_else(|_| reqwest::Client::new())
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

    /// Provider display name (for UI).
    fn provider_name(&self) -> &str;
}
