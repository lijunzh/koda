//! Koda's config between client and host: `config/read` and
//! `config/batchWrite`, as Codex's TUI and app-server use them. The host
//! keeps it in `~/.config/koda/config.toml`, which a person can read and
//! edit too; writes keep their comments. It holds the model choice a new
//! session starts with, each provider's endpoint, and the MCP servers
//! sessions start ([`crate::mcp`]).

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The top-level config keys that hold a single value. Besides these,
/// Koda takes `model_providers.<id>.<key>` ([`PROVIDERS_KEY`]),
/// `mcp_servers.<name>`, `tui.<key>` and `sandbox.<key>`; any other is
/// refused.
pub const KEYS: [&str; 6] = [
    "model",
    "model_provider",
    "model_reasoning_effort",
    "model_context_window",
    "model_post_turn_compact_threshold_percent",
    "mode",
];

/// The config key that holds per-provider settings:
/// `model_providers.<id>.base_url`.
pub const PROVIDERS_KEY: &str = "model_providers";

/// `config/read`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfigReadParams {
    /// Also return each layer and its values.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_layers: bool,
}

/// The answer to `config/read`: the settings, where each came from, and,
/// when asked, the layers. A key nothing sets is absent, and a new session
/// then uses the host's own.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigReadResponse {
    /// The settings, by key.
    pub config: KodaConfigValues,
    /// The layer each value comes from, by dotted key path
    /// (`model_providers.lm-studio.base_url`).
    pub origins: BTreeMap<String, ConfigLayerMetadata>,
    /// The layers, lowest first, with `includeLayers`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layers: Option<Vec<ConfigLayer>>,
}

/// Where a value comes from: the user's file, or this run's `-c` values
/// over it. Codex also has managed and project layers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ConfigLayerSource {
    /// `~/.config/koda/config.toml`.
    #[serde(rename_all = "camelCase")]
    User {
        /// The file's path.
        file: String,
    },
    /// `-c key=value` on the command line that started the host, for this
    /// run only.
    SessionFlags,
}

/// A layer and its version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigLayerMetadata {
    /// The layer.
    pub name: ConfigLayerSource,
    /// Its version, `sha256:` of its values; pass it as
    /// `expectedVersion` to write only if nothing changed since.
    pub version: String,
}

/// A layer, its version and its values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigLayer {
    /// The layer.
    pub name: ConfigLayerSource,
    /// Its version.
    pub version: String,
    /// Every value it holds, Koda's keys or not.
    pub config: Value,
}

/// Koda's settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct KodaConfigValues {
    /// The model a new session starts with: an alias such as `claude-opus`,
    /// which follows its line, or an id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The provider's id, such as `anthropic`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_provider: Option<String>,
    /// The reasoning effort a new session asks for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_reasoning_effort: Option<String>,
    /// The context window to assume, in tokens, for a server that doesn't
    /// report its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_context_window: Option<u64>,
    /// Context used, in percent, past which a turn compacts after its
    /// final reply; 0 or none turns it off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_post_turn_compact_threshold_percent: Option<u8>,
    /// The mode a new session starts in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<crate::state::TrustMode>,
    /// Settings per provider, by provider id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub model_providers: BTreeMap<String, ModelProviderConfigValue>,
    /// The MCP servers every new session starts, by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp_servers: BTreeMap<String, crate::mcp::McpServerConfigValue>,
    /// The TUI's own settings.
    #[serde(default, skip_serializing_if = "TuiConfigValue::is_empty")]
    pub tui: TuiConfigValue,
    /// What sandboxed commands may do beyond Koda's defaults.
    #[serde(default, skip_serializing_if = "SandboxConfigValue::is_empty")]
    pub sandbox: SandboxConfigValue,
}

/// `[tui]`: settings a TUI reads from its own machine's file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TuiConfigValue {
    /// Colour code blocks and diffs; `false` for plain text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub syntax_highlight: Option<bool>,
}

impl TuiConfigValue {
    fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// `[sandbox]`: what sandboxed commands may do beyond Koda's defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SandboxConfigValue {
    /// macOS: let commands use any local port, not only the servers Koda
    /// started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_local_binding: Option<bool>,
}

impl SandboxConfigValue {
    fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// One provider's settings: `[model_providers.<id>]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ModelProviderConfigValue {
    /// The provider's endpoint, used whenever a session runs on it and
    /// names none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Retries a failed request gets after the first try.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_max_retries: Option<u64>,
    /// Retries for a reply whose stream failed partway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_max_retries: Option<u64>,
    /// Milliseconds a reply may go silent before the connection counts as
    /// lost; 0 turns the limit off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_idle_timeout_ms: Option<u64>,
}

/// `configWarning`: something in a config file needs a look, such as a key
/// Koda doesn't know, which it loads past. In Codex's shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigWarningNotification {
    /// One sentence for a person.
    pub summary: String,
    /// More detail, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    /// The file it concerns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// `config/batchWrite`: change several keys at once, as Codex's does.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfigBatchWriteParams {
    /// The changes, applied in order; all or none.
    pub edits: Vec<ConfigEdit>,
    /// Write only if the file is still at this version (from `config/read`
    /// or an earlier write); otherwise refused with
    /// [`ConfigWriteErrorCode::ConfigVersionConflict`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<String>,
}

/// One change: a key, its new value (`null` removes it) and how to merge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfigEdit {
    /// The key: one of [`KEYS`], `model_providers.<id>.base_url`, or
    /// `mcp_servers.<name>`.
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
    /// The file's version now.
    pub version: String,
    /// The file written.
    pub file_path: String,
    /// The settings as saved now, `-c` values included.
    pub config: KodaConfigValues,
    /// With `okOverridden`: a `-c` value wins over what was written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overridden_metadata: Option<OverriddenMetadata>,
}

/// What wins over a write, as Codex reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OverriddenMetadata {
    /// One sentence for a person.
    pub message: String,
    /// The layer whose value is in force.
    pub overriding_layer: ConfigLayerMetadata,
    /// The value in force, for the first overridden key.
    pub effective_value: Value,
}

/// Why `config/batchWrite` was refused, in the error's `data` as
/// `{"configWriteErrorCode": ...}`, as Codex reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ConfigWriteErrorCode {
    /// The file changed since `expectedVersion`.
    ConfigVersionConflict,
    /// A value doesn't fit its key, or the result wouldn't load.
    ConfigValidationError,
    /// A key Koda doesn't take.
    ConfigSchemaUnknownKey,
}

/// How a write went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum WriteStatus {
    /// Saved.
    Ok,
    /// Saved, but a `-c` value for this run wins over it until Koda
    /// restarts.
    OkOverridden,
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
