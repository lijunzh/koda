//! Models and providers a client can offer: `model/list` and
//! `modelProvider/list`, as Codex names them.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `model/list`: the models a provider offers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelListParams {
    /// The provider, by id. Without one, the host's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_provider: Option<String>,
    /// The provider's endpoint. Without one, its default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

/// The answer to `model/list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelListResponse {
    /// The models, aliases first. For Anthropic, the newest model of each
    /// line, not every model it lists.
    pub data: Vec<Model>,
}

/// One model a provider offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    /// The id to send, or `auto-detect` for `local`, whose model the server
    /// names.
    pub id: String,
    /// The alias this entry is, such as `claude-opus`, which follows its
    /// line. A pick by alias should be made by it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// The reasoning efforts it takes (`low` … `max`), as Codex's
    /// `supportedReasoningEfforts`; empty where Koda doesn't set one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_reasoning_efforts: Vec<String>,
    /// The effort sent when none is chosen, as Codex's
    /// `defaultReasoningEffort`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reasoning_effort: Option<String>,
    /// The model a new session of this provider gets without one, as
    /// Codex's `isDefault`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_default: bool,
}

impl Model {
    /// A model known only by its id.
    pub fn named(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            alias: None,
            supported_reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
            is_default: false,
        }
    }
}

/// `modelProvider/list`: the providers Koda offers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelProviderListParams {}

/// The answer to `modelProvider/list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelProviderListResponse {
    /// The providers, in the order a picker shows them.
    pub data: Vec<ModelProviderSummary>,
    /// The host's provider, which a new session uses by default.
    pub default_provider_id: String,
}

/// One provider. Codex's `ModelProviderSummary`, with Koda's key status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelProviderSummary {
    /// The id, such as `lm-studio`.
    pub id: String,
    /// The name a person reads, such as `LM Studio`.
    pub name: String,
    /// The default endpoint.
    pub base_url: String,
    /// Whether it needs an API key.
    pub requires_api_key: bool,
    /// Whether a key is set for it. Never the key itself.
    pub has_api_key: bool,
    /// The environment variable its key comes from, such as
    /// `ANTHROPIC_API_KEY`, as Codex's `ModelProviderInfo.env_key`.
    pub env_key: String,
}
