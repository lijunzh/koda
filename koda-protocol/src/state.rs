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
/// Ordered `Plan < Safe < Auto`, so [`TrustMode::clamp`] keeps a sub-agent
/// from exceeding the session that started it.
///
/// ```
/// use koda_protocol::state::TrustMode;
///
/// assert_eq!(TrustMode::Safe.as_str(), "safe");
/// assert_eq!(TrustMode::Safe.next(), TrustMode::Auto);
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
    /// Read only: reads, searches and read-only commands run; anything else
    /// is refused. For sub-agents and `koda exec` runs that should only
    /// look.
    Plan = 0,
    /// Every action with a side effect asks first.
    #[default]
    Safe = 1,
    /// Mutating actions run without asking, contained by the sandbox;
    /// destructive actions and writes outside the project still ask.
    Auto = 2,
}

impl TrustMode {
    /// What the keyboard toggle switches to: Safe and Auto in turn. Plan
    /// stays Plan; an interactive session never runs in it.
    pub fn next(self) -> Self {
        match self {
            Self::Plan => Self::Plan,
            Self::Safe => Self::Auto,
            Self::Auto => Self::Safe,
        }
    }

    /// The name in `config.toml`, on `--mode` and on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Safe => "safe",
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
            Self::Plan => "read-only, every write refused",
            Self::Safe => "confirm every side effect",
            Self::Auto => {
                "auto-approve in the sandbox; destructive and outside-project still confirm"
            }
        }
    }

    /// A mode from what a person typed, with the older names Koda took.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "auto" | "yolo" | "accept" => Some(Self::Auto),
            "safe" | "confirm" | "strict" | "normal" => Some(Self::Safe),
            "plan" | "readonly" | "read-only" => Some(Self::Plan),
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
            0 => Self::Plan,
            1 => Self::Safe,
            2 => Self::Auto,
            // Fail-safe: an unknown value is Safe, never Auto (#860).
            _ => Self::Safe,
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
    /// backend, or to use Safe mode. For a person, a few lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_hint: Option<String>,
}
