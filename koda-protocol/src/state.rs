//! `state/read`: what a new session starts with.
//!
//! It reads the defaults a new session gets. Reading an open session's own
//! state comes with the request areas that change it (model, provider,
//! mode).

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
    /// The provider, such as `lm-studio` or `anthropic`. Codex's
    /// `modelProvider`.
    pub model_provider: String,
    /// The address requests go to.
    pub base_url: String,
    /// The context window in tokens.
    pub context_window: u64,
    /// What Koda may do without asking.
    pub mode: TrustMode,
    /// Whether commands can run sandboxed.
    pub sandbox: SandboxState,
}

/// What Koda may do without asking: Koda's one trust mode, used by the
/// engine, `config.toml`'s `mode` and the protocol alike, as Codex keeps
/// its approval policy in `codex-protocol`.
///
/// As Codex's sandbox modes: Plan runs every command in a read-only
/// sandbox, Auto in one that can write the project. What needs more asks to
/// leave the sandbox. Ordered `Plan < Auto`, so [`TrustMode::clamp`] keeps
/// a sub-agent from exceeding the session that started it.
///
/// ```
/// use koda_protocol::state::TrustMode;
///
/// assert_eq!(TrustMode::Auto.as_str(), "auto");
/// assert_eq!(TrustMode::Auto.next(), TrustMode::Plan);
/// assert_eq!(TrustMode::clamp(TrustMode::Plan, TrustMode::Auto), TrustMode::Plan);
/// ```
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    Serialize,
    Deserialize,
    JsonSchema,
)]
#[serde(rename_all = "lowercase")]
#[repr(u8)]
pub enum TrustMode {
    /// Read only: every command runs in a read-only sandbox, and edits are
    /// refused. For planning, read-only sub-agents and `koda exec` runs that
    /// should only look.
    Plan = 0,
    /// Commands run without asking in a sandbox that can write the project;
    /// a command that would leave it, a dangerous one, or a file outside the
    /// project asks first.
    #[default]
    Auto = 1,
}

impl TrustMode {
    /// What the keyboard toggle switches to: Auto and Plan in turn.
    pub fn next(self) -> Self {
        match self {
            Self::Plan => Self::Auto,
            Self::Auto => Self::Plan,
        }
    }

    /// The name in `config.toml`, on `--mode` and on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Auto => "auto",
        }
    }

    /// Short label for display (the same as [`TrustMode::as_str`]).
    pub fn label(self) -> &'static str {
        self.as_str()
    }

    /// One line on what the mode does.
    pub fn description(self) -> &'static str {
        match self {
            Self::Plan => "read-only sandbox, every edit refused",
            Self::Auto => {
                "runs in the sandbox; leaving it, dangerous commands and outside-project files ask"
            }
        }
    }

    /// A mode from what a person typed.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "plan" => Some(Self::Plan),
            _ => None,
        }
    }

    /// A child's mode, never above its parent's: `min(parent, child)`.
    pub fn clamp(parent: TrustMode, child: TrustMode) -> TrustMode {
        std::cmp::min(parent, child)
    }
}

impl From<u8> for TrustMode {
    fn from(v: u8) -> Self {
        match v {
            1 => Self::Auto,
            // Fail-safe: an unknown value is Plan, never more than asked.
            _ => Self::Plan,
        }
    }
}

impl std::fmt::Display for TrustMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
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
    /// What to do about it, when it doesn't: how to install or enable the
    /// backend. For a person, a few lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_hint: Option<String>,
}
