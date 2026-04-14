//! Standalone LLM message types for the supervisor ↔ worker IPC protocol.
//!
//! These mirror the types in `koda_core::providers` but are defined here to
//! avoid a circular dependency (`koda-core` already depends on `koda-ipc`).
//!
//! Conversions between IPC types and koda-core types live in
//! `koda_core::providers::ipc` (the IPC provider implementation).

use serde::{Deserialize, Serialize};

// ── Request side ──────────────────────────────────────────────────────────────

/// Mirrors `koda_core::providers::ChatMessage`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcChatMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<IpcToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Base64-encoded images attached to this message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<IpcImageData>>,
}

/// Mirrors `koda_core::providers::ToolCall`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcToolCall {
    pub id: String,
    pub function_name: String,
    pub arguments: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

/// Mirrors `koda_core::providers::ToolDefinition`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Mirrors `koda_core::providers::ImageData`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcImageData {
    pub media_type: String,
    pub base64: String,
}

/// Mirrors the relevant fields of `koda_core::config::ModelSettings`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcModelSettings {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_budget: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    pub max_context_tokens: usize,
}

/// A complete LLM chat request the worker wants the supervisor to execute.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcLlmRequest {
    pub messages: Vec<IpcChatMessage>,
    pub tools: Vec<IpcToolDefinition>,
    pub settings: IpcModelSettings,
}

// ── Response side ─────────────────────────────────────────────────────────────

/// Mirrors `koda_core::providers::TokenUsage`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IpcTokenUsage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub thinking_tokens: i64,
    pub stop_reason: String,
}

/// A complete LLM response the supervisor returns to the worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcLlmResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    pub tool_calls: Vec<IpcToolCall>,
    pub usage: IpcTokenUsage,
}
