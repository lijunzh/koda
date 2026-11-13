//! `state/read`: what a new session starts with.
//!
//! Sessions aren't on the protocol yet. When they are, this request takes a
//! session id and reads that session; without one it keeps reading the
//! defaults a new session gets.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// No parameters yet.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StateReadParams {}

/// The model, provider, context window, mode and sandbox a new session
/// starts with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StateReadResponse {
    /// The model id, such as `claude-sonnet-5-5`.
    pub model: String,
    /// The provider, such as `lm-studio` or `anthropic`.
    pub provider: String,
    /// The address requests go to.
    pub base_url: String,
    /// The context window in tokens.
    pub context_window: u64,
    /// What Koda may do without asking.
    pub mode: TrustMode,
    /// Whether commands can run sandboxed.
    pub sandbox: SandboxState,
}

/// What Koda may do without asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TrustMode {
    /// Read only; anything else is refused.
    Plan,
    /// Anything with a side effect needs approval.
    Safe,
    /// Everything runs without asking, inside the sandbox.
    Auto,
}

/// The sandbox commands run in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SandboxState {
    /// The backend: `seatbelt`, `bwrap` or `none`.
    pub backend: String,
    /// Whether it works on this machine. Auto mode needs it.
    pub available: bool,
    /// Why not, when it doesn't.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}
