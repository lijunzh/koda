//! Approval modes and tool confirmation.
//!
//! Three modes control how Koda handles tool confirmations:
//! - **Auto** (default): Auto-approve everything. Full trust in the model.
//! - **Strict**: Every non-read action requires explicit confirmation.
//! - **Safe**: Read-only. Mutations blocked, safe bash allowed.
//!
//! Bash commands are classified by parsing pipelines and checking each segment
//! against a built-in safe list.

use crate::bash_safety::is_command_safe;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

// ── Approval Mode ─────────────────────────────────────────

/// The three approval modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ApprovalMode {
    /// Read-only: safe bash allowed, mutations blocked.
    Safe = 0,
    /// Every non-read action needs explicit confirmation.
    Strict = 1,
    /// Full auto: approve everything without confirmation.
    Auto = 2,
}

impl ApprovalMode {
    /// Cycle to the next mode: Auto → Strict → Safe → Auto.
    pub fn next(self) -> Self {
        match self {
            Self::Auto => Self::Strict,
            Self::Strict => Self::Safe,
            Self::Safe => Self::Auto,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::Strict => "strict",
            Self::Auto => "auto",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Safe => "read-only, mutations blocked",
            Self::Strict => "confirm every non-read action",
            Self::Auto => "auto-approve everything",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "auto" | "yolo" | "accept" => Some(Self::Auto),
            "strict" | "normal" => Some(Self::Strict),
            "safe" | "plan" | "readonly" => Some(Self::Safe),
            _ => None,
        }
    }
}

impl From<u8> for ApprovalMode {
    fn from(v: u8) -> Self {
        match v {
            0 => Self::Safe,
            1 => Self::Strict,
            _ => Self::Auto, // default is now Auto
        }
    }
}

/// Thread-safe shared mode, readable from prompt formatter and input handlers.
pub type SharedMode = Arc<AtomicU8>;

pub fn new_shared_mode(mode: ApprovalMode) -> SharedMode {
    Arc::new(AtomicU8::new(mode as u8))
}

pub fn read_mode(shared: &SharedMode) -> ApprovalMode {
    ApprovalMode::from(shared.load(Ordering::Relaxed))
}

pub fn set_mode(shared: &SharedMode, mode: ApprovalMode) {
    shared.store(mode as u8, Ordering::Relaxed);
}

pub fn cycle_mode(shared: &SharedMode) -> ApprovalMode {
    let current = read_mode(shared);
    let next = current.next();
    set_mode(shared, next);
    next
}

// ── Tool Approval Decision ──────────────────────────────────

/// What the approval system decides for a given tool call.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolApproval {
    /// Execute without asking.
    AutoApprove,
    /// Show confirmation dialog.
    NeedsConfirmation,
    /// Safe mode: show what would happen, don't execute.
    Blocked,
}

/// Read-only tools that auto-approve in all modes (including Safe).
/// These never modify the filesystem or have destructive side effects.
const READ_ONLY_TOOLS: &[&str] = &[
    "Read",
    "List",
    "Grep",
    "Glob",
    "MemoryRead",
    "ListAgents",
    "InvokeAgent",   // sub-agents inherit parent's approval mode
    "WebFetch",      // GET-only URL fetch
    "ListSkills",    // read-only skill listing
    "ActivateSkill", // read-only skill activation (context injection)
];

/// Decide whether a tool call should be auto-approved, confirmed, or blocked.
pub fn check_tool(tool_name: &str, args: &serde_json::Value, mode: ApprovalMode) -> ToolApproval {
    // Read-only tools always execute in every mode
    if READ_ONLY_TOOLS.contains(&tool_name) {
        return ToolApproval::AutoApprove;
    }

    match mode {
        ApprovalMode::Auto => ToolApproval::AutoApprove,

        ApprovalMode::Safe => {
            // Safe mode: write tools blocked, bash uses safety classification
            if tool_name == "Bash" {
                let command = args
                    .get("command")
                    .or(args.get("cmd"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if is_command_safe(command) {
                    ToolApproval::AutoApprove
                } else {
                    ToolApproval::Blocked
                }
            } else {
                ToolApproval::Blocked
            }
        }

        ApprovalMode::Strict => {
            if tool_name == "Bash" {
                let command = args
                    .get("command")
                    .or(args.get("cmd"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if is_command_safe(command) {
                    ToolApproval::AutoApprove
                } else {
                    ToolApproval::NeedsConfirmation
                }
            } else {
                ToolApproval::NeedsConfirmation
            }
        }
    }
}

// ── Settings persistence ──────────────────────────────────

/// User settings stored in `~/.config/koda/settings.toml`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    /// Last-used provider/model, restored on next startup.
    #[serde(default)]
    pub last_provider: Option<LastProvider>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LastProvider {
    pub provider_type: String,
    pub base_url: String,
    pub model: String,
}

impl Settings {
    /// Load from `~/.config/koda/settings.toml`, returning defaults if missing.
    pub fn load() -> Self {
        Self::settings_path()
            .and_then(|path| std::fs::read_to_string(&path).ok())
            .and_then(|content| toml::from_str(&content).ok())
            .unwrap_or_default()
    }

    /// Save to `~/.config/koda/settings.toml`.
    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::settings_path()
            .ok_or_else(|| anyhow::anyhow!("Cannot determine config directory"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;
        std::fs::write(&path, content)?;
        Ok(())
    }

    /// Save the last-used provider/model for restoration on next startup.
    pub fn save_last_provider(
        &mut self,
        provider_type: &str,
        base_url: &str,
        model: &str,
    ) -> anyhow::Result<()> {
        self.last_provider = Some(LastProvider {
            provider_type: provider_type.to_string(),
            base_url: base_url.to_string(),
            model: model.to_string(),
        });
        self.save()
    }

    fn settings_path() -> Option<PathBuf> {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .ok()?;
        Some(
            Path::new(&home)
                .join(".config")
                .join("koda")
                .join("settings.toml"),
        )
    }
}

// ── Tests ─────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Mode tests ──

    #[test]
    fn test_mode_cycle() {
        assert_eq!(ApprovalMode::Auto.next(), ApprovalMode::Strict);
        assert_eq!(ApprovalMode::Strict.next(), ApprovalMode::Safe);
        assert_eq!(ApprovalMode::Safe.next(), ApprovalMode::Auto);
    }

    #[test]
    fn test_mode_from_str() {
        // New names
        assert_eq!(ApprovalMode::parse("auto"), Some(ApprovalMode::Auto));
        assert_eq!(ApprovalMode::parse("strict"), Some(ApprovalMode::Strict));
        assert_eq!(ApprovalMode::parse("safe"), Some(ApprovalMode::Safe));
        // Legacy aliases
        assert_eq!(ApprovalMode::parse("yolo"), Some(ApprovalMode::Auto));
        assert_eq!(ApprovalMode::parse("normal"), Some(ApprovalMode::Strict));
        assert_eq!(ApprovalMode::parse("plan"), Some(ApprovalMode::Safe));
        assert_eq!(ApprovalMode::parse("readonly"), Some(ApprovalMode::Safe));
        assert_eq!(ApprovalMode::parse("accept"), Some(ApprovalMode::Auto));
        assert_eq!(ApprovalMode::parse("nope"), None);
    }

    #[test]
    fn test_mode_from_u8() {
        assert_eq!(ApprovalMode::from(0), ApprovalMode::Safe);
        assert_eq!(ApprovalMode::from(1), ApprovalMode::Strict);
        assert_eq!(ApprovalMode::from(2), ApprovalMode::Auto);
        assert_eq!(ApprovalMode::from(99), ApprovalMode::Auto); // default is Auto
    }

    #[test]
    fn test_shared_mode_cycle() {
        let shared = new_shared_mode(ApprovalMode::Auto);
        assert_eq!(read_mode(&shared), ApprovalMode::Auto);
        let next = cycle_mode(&shared);
        assert_eq!(next, ApprovalMode::Strict);
        assert_eq!(read_mode(&shared), ApprovalMode::Strict);
    }

    // ── Tool approval tests ──

    #[test]
    fn test_read_tools_always_approved() {
        for tool in READ_ONLY_TOOLS {
            assert_eq!(
                check_tool(tool, &serde_json::json!({}), ApprovalMode::Safe),
                ToolApproval::AutoApprove,
                "{tool} should auto-approve even in Safe mode"
            );
        }
    }

    #[test]
    fn test_write_tools_blocked_in_safe() {
        for tool in ["Write", "Edit", "Delete", "CreateAgent", "MemoryWrite"] {
            assert_eq!(
                check_tool(tool, &serde_json::json!({}), ApprovalMode::Safe),
                ToolApproval::Blocked,
                "{tool} should be blocked in Safe mode"
            );
        }
    }

    #[test]
    fn test_auto_approves_everything() {
        for tool in ["Write", "Edit", "Delete", "Bash", "WebFetch"] {
            assert_eq!(
                check_tool(tool, &serde_json::json!({}), ApprovalMode::Auto),
                ToolApproval::AutoApprove,
            );
        }
    }

    #[test]
    fn test_safe_bash_auto_approved_in_strict() {
        let args = serde_json::json!({"command": "cargo test --release"});
        assert_eq!(
            check_tool("Bash", &args, ApprovalMode::Strict),
            ToolApproval::AutoApprove,
        );
    }

    #[test]
    fn test_dangerous_bash_needs_confirmation_in_strict() {
        let args = serde_json::json!({"command": "rm -rf target/"});
        assert_eq!(
            check_tool("Bash", &args, ApprovalMode::Strict),
            ToolApproval::NeedsConfirmation,
        );
    }

    #[test]
    fn test_write_needs_confirmation_in_strict() {
        assert_eq!(
            check_tool("Write", &serde_json::json!({}), ApprovalMode::Strict),
            ToolApproval::NeedsConfirmation,
        );
    }

    #[test]
    fn test_invoke_agent_auto_approved() {
        let args = serde_json::json!({"agent_name": "reviewer", "prompt": "review this"});
        for mode in [ApprovalMode::Auto, ApprovalMode::Strict, ApprovalMode::Safe] {
            assert_eq!(
                check_tool("InvokeAgent", &args, mode),
                ToolApproval::AutoApprove,
            );
        }
    }

    #[test]
    fn test_safe_mode_allows_safe_bash() {
        let args = serde_json::json!({"command": "cargo test --release"});
        assert_eq!(
            check_tool("Bash", &args, ApprovalMode::Safe),
            ToolApproval::AutoApprove,
        );
    }

    #[test]
    fn test_safe_mode_blocks_dangerous_bash() {
        let args = serde_json::json!({"command": "rm -rf target/"});
        assert_eq!(
            check_tool("Bash", &args, ApprovalMode::Safe),
            ToolApproval::Blocked,
        );
    }

    #[test]
    fn test_safe_mode_allows_web_fetch() {
        let args = serde_json::json!({"url": "https://example.com"});
        assert_eq!(
            check_tool("WebFetch", &args, ApprovalMode::Safe),
            ToolApproval::AutoApprove,
        );
    }
}
