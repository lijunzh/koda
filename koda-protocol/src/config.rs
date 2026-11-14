//! Koda's config between client and host: `config/read` and
//! `config/batchWrite`, as Codex's TUI and app-server use them. The host
//! owns the config and keeps it; nobody edits it by hand. It holds the
//! model choice a new session starts with and the MCP servers sessions
//! start ([`crate::mcp`]).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The config keys Koda takes, besides `mcp_servers.<name>`. Any other is
/// refused.
pub const KEYS: [&str; 3] = ["model", "model_provider", "base_url"];

/// `config/read`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfigReadParams {}

/// The answer to `config/read`: the saved settings. A key nothing has
/// saved is absent, and a new session then uses the host's own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigReadResponse {
    /// The settings, by key.
    pub config: KodaConfigValues,
}

/// Koda's saved settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct KodaConfigValues {
    /// The model a new session starts with: an alias such as `claude-opus`,
    /// which follows its line, or an id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The provider's id, such as `anthropic`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_provider: Option<String>,
    /// The provider's endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// The MCP servers every new session starts, by name.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub mcp_servers: std::collections::BTreeMap<String, crate::mcp::McpServerConfigValue>,
}

/// `config/batchWrite`: change several keys at once, as Codex's does.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfigBatchWriteParams {
    /// The changes, applied in order; all or none.
    pub edits: Vec<ConfigEdit>,
}

/// One change: a key, its new value (`null` removes it) and how to merge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfigEdit {
    /// The key: one of [`KEYS`], or `mcp_servers.<name>`.
    pub key_path: String,
    /// The value: a string, or for `mcp_servers.<name>` a server
    /// ([`crate::mcp::McpServerConfigValue`]); `null` removes the key.
    pub value: Value,
    /// How the value merges; for Koda's keys, both replace it.
    pub merge_strategy: MergeStrategy,
}

/// How an edit's value merges with what's there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum MergeStrategy {
    /// Replace the value.
    Replace,
    /// Merge into it; for a string, the same as replace.
    Upsert,
}

/// The answer to `config/batchWrite`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigWriteResponse {
    /// How the write went.
    pub status: WriteStatus,
    /// The settings as saved now.
    pub config: KodaConfigValues,
}

/// How a write went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum WriteStatus {
    /// Saved.
    Ok,
}

/// `modelProvider/apiKey/set`: store a provider's API key. Koda keeps a key
/// per provider, which Codex, whose `account/login/start` signs in one
/// OpenAI account, doesn't. The key never comes back in any response.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelProviderApiKeySetParams {
    /// The provider's id.
    pub provider_id: String,
    /// The key.
    pub api_key: String,
}

impl std::fmt::Debug for ModelProviderApiKeySetParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelProviderApiKeySetParams")
            .field("provider_id", &self.provider_id)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

/// `modelProvider/apiKey/remove`: forget a provider's stored key. One set
/// in the environment Koda was started with stays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelProviderApiKeyRemoveParams {
    /// The provider's id.
    pub provider_id: String,
}

/// The answer to `modelProvider/apiKey/set` and `.../remove`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelProviderApiKeyResponse {}
