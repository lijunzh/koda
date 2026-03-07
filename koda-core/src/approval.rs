//! Approval modes and bash command safety classification.
//!
//! Three modes control how Koda handles tool confirmations:
//! - **Plan**: Read-only. Write tools show what would happen but don't execute.
//! - **Normal**: Smart confirmation. Safe bash auto-approves, dangerous confirms.
//! - **Yolo**: Auto-approve everything. Full trust in the model.
//!
//! Bash commands are classified by parsing pipelines and checking each segment
//! against a built-in safe list + user-configurable whitelist.

use crate::bash_safety::is_command_safe;
// Re-export for use by inference.rs and other consumers
pub use crate::bash_safety::extract_whitelist_pattern;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

// ── Approval Mode ─────────────────────────────────────────────

/// The three approval modes, matching Claude Code's plan/normal/yolo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ApprovalMode {
    /// Read-only: describe actions without executing writes.
    Plan = 0,
    /// Smart: auto-approve safe ops, confirm dangerous ones.
    Normal = 1,
    /// Full auto: approve everything without confirmation.
    Yolo = 2,
}

impl ApprovalMode {
    /// Cycle to the next mode: Plan → Normal → Yolo → Plan.
    pub fn next(self) -> Self {
        match self {
            Self::Plan => Self::Normal,
            Self::Normal => Self::Yolo,
            Self::Yolo => Self::Plan,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Normal => "normal",
            Self::Yolo => "yolo",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Plan => "read-only, describe actions without executing",
            Self::Normal => "confirm dangerous actions, auto-approve safe ones",
            Self::Yolo => "auto-approve everything",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "plan" => Some(Self::Plan),
            "normal" => Some(Self::Normal),
            "yolo" | "auto" | "accept" => Some(Self::Yolo),
            _ => None,
        }
    }
}

impl From<u8> for ApprovalMode {
    fn from(v: u8) -> Self {
        match v {
            0 => Self::Plan,
            2 => Self::Yolo,
            _ => Self::Normal,
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

// ── Tool Approval Decision ────────────────────────────────────

/// What the approval system decides for a given tool call.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolApproval {
    /// Execute without asking.
    AutoApprove,
    /// Show confirmation dialog.
    NeedsConfirmation,
    /// Plan mode: show what would happen, don't execute.
    Blocked,
}

/// Read-only tools that auto-approve in all modes (including Plan).
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
    "TodoWrite",     // internal checklist, no file changes
    "TodoRead",      // read-only checklist access
    "ListSkills",    // read-only skill listing
    "ActivateSkill", // read-only skill activation (context injection)
];

/// Decide whether a tool call should be auto-approved, confirmed, or blocked.
///
/// Plan mode is read-only: all analysis tools work (read, grep, sub-agents,
/// safe bash) but write tools are blocked. This lets the agent build a
/// comprehensive plan by actually reading code and running checks.
pub fn check_tool(
    tool_name: &str,
    args: &serde_json::Value,
    mode: ApprovalMode,
    user_whitelist: &[String],
) -> ToolApproval {
    // Read-only tools always execute in every mode
    if READ_ONLY_TOOLS.contains(&tool_name) {
        return ToolApproval::AutoApprove;
    }

    match mode {
        ApprovalMode::Yolo => ToolApproval::AutoApprove,

        ApprovalMode::Plan => {
            // Plan mode: write tools are blocked, bash uses safety classification
            if tool_name == "Bash" {
                let command = args
                    .get("command")
                    .or(args.get("cmd"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if is_command_safe(command, user_whitelist) {
                    ToolApproval::AutoApprove
                } else {
                    ToolApproval::Blocked
                }
            } else {
                // Write, Edit, Delete, CreateAgent, MemoryWrite, unknown
                ToolApproval::Blocked
            }
        }

        ApprovalMode::Normal => {
            if tool_name == "Bash" {
                let command = args
                    .get("command")
                    .or(args.get("cmd"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if is_command_safe(command, user_whitelist) {
                    ToolApproval::AutoApprove
                } else {
                    ToolApproval::NeedsConfirmation
                }
            } else {
                // Write, Edit, Delete, CreateAgent, MemoryWrite, unknown
                ToolApproval::NeedsConfirmation
            }
        }
    }
}

// ── Settings persistence ──────────────────────────────────────

/// User settings stored in `~/.config/koda/settings.toml`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub approval: ApprovalSettings,
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

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ApprovalSettings {
    /// User-defined bash commands to auto-approve.
    #[serde(default)]
    pub allowed_commands: Vec<String>,
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

    /// Add a command pattern to the whitelist and persist.
    pub fn add_allowed_command(&mut self, pattern: &str) -> anyhow::Result<()> {
        let pattern = pattern.trim().to_string();
        if !self.approval.allowed_commands.contains(&pattern) {
            self.approval.allowed_commands.push(pattern);
            self.save()?;
        }
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

// ── Tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Mode tests ──

    #[test]
    fn test_mode_cycle() {
        assert_eq!(ApprovalMode::Plan.next(), ApprovalMode::Normal);
        assert_eq!(ApprovalMode::Normal.next(), ApprovalMode::Yolo);
        assert_eq!(ApprovalMode::Yolo.next(), ApprovalMode::Plan);
    }

    #[test]
    fn test_mode_from_str() {
        assert_eq!(ApprovalMode::parse("plan"), Some(ApprovalMode::Plan));
        assert_eq!(ApprovalMode::parse("YOLO"), Some(ApprovalMode::Yolo));
        assert_eq!(ApprovalMode::parse("auto"), Some(ApprovalMode::Yolo));
        assert_eq!(ApprovalMode::parse("accept"), Some(ApprovalMode::Yolo));
        assert_eq!(ApprovalMode::parse("nope"), None);
    }

    #[test]
    fn test_mode_from_u8() {
        assert_eq!(ApprovalMode::from(0), ApprovalMode::Plan);
        assert_eq!(ApprovalMode::from(1), ApprovalMode::Normal);
        assert_eq!(ApprovalMode::from(2), ApprovalMode::Yolo);
        assert_eq!(ApprovalMode::from(99), ApprovalMode::Normal); // fallback
    }

    #[test]
    fn test_shared_mode_cycle() {
        let shared = new_shared_mode(ApprovalMode::Normal);
        assert_eq!(read_mode(&shared), ApprovalMode::Normal);
        let next = cycle_mode(&shared);
        assert_eq!(next, ApprovalMode::Yolo);
        assert_eq!(read_mode(&shared), ApprovalMode::Yolo);
    }

    // ── Tool approval tests ──

    #[test]
    fn test_read_tools_always_approved() {
        for tool in READ_ONLY_TOOLS {
            assert_eq!(
                check_tool(tool, &serde_json::json!({}), ApprovalMode::Plan, &[]),
                ToolApproval::AutoApprove,
                "{tool} should auto-approve even in Plan mode"
            );
        }
    }

    #[test]
    fn test_write_tools_blocked_in_plan() {
        for tool in ["Write", "Edit", "Delete", "CreateAgent", "MemoryWrite"] {
            assert_eq!(
                check_tool(tool, &serde_json::json!({}), ApprovalMode::Plan, &[]),
                ToolApproval::Blocked,
                "{tool} should be blocked in Plan mode"
            );
        }
    }

    #[test]
    fn test_yolo_approves_everything() {
        for tool in ["Write", "Edit", "Delete", "Bash", "WebFetch"] {
            assert_eq!(
                check_tool(tool, &serde_json::json!({}), ApprovalMode::Yolo, &[]),
                ToolApproval::AutoApprove,
            );
        }
    }

    #[test]
    fn test_safe_bash_auto_approved_in_normal() {
        let args = serde_json::json!({"command": "cargo test --release"});
        assert_eq!(
            check_tool("Bash", &args, ApprovalMode::Normal, &[]),
            ToolApproval::AutoApprove,
        );
    }

    #[test]
    fn test_dangerous_bash_needs_confirmation() {
        let args = serde_json::json!({"command": "rm -rf target/"});
        assert_eq!(
            check_tool("Bash", &args, ApprovalMode::Normal, &[]),
            ToolApproval::NeedsConfirmation,
        );
    }

    #[test]
    fn test_write_needs_confirmation_in_normal() {
        assert_eq!(
            check_tool("Write", &serde_json::json!({}), ApprovalMode::Normal, &[]),
            ToolApproval::NeedsConfirmation,
        );
    }

    #[test]
    fn test_invoke_agent_auto_approved_in_normal() {
        let args = serde_json::json!({"agent_name": "reviewer", "prompt": "review this"});
        assert_eq!(
            check_tool("InvokeAgent", &args, ApprovalMode::Normal, &[]),
            ToolApproval::AutoApprove,
        );
    }

    #[test]
    fn test_invoke_agent_auto_approved_in_yolo() {
        let args = serde_json::json!({"agent_name": "reviewer", "prompt": "review this"});
        assert_eq!(
            check_tool("InvokeAgent", &args, ApprovalMode::Yolo, &[]),
            ToolApproval::AutoApprove,
        );
    }

    #[test]
    fn test_invoke_agent_blocked_in_plan() {
        // InvokeAgent is read-only — sub-agents inherit Plan mode
        // so they can read but not write. No need to block invocation.
        let args = serde_json::json!({"agent_name": "reviewer", "prompt": "review this"});
        assert_eq!(
            check_tool("InvokeAgent", &args, ApprovalMode::Plan, &[]),
            ToolApproval::AutoApprove,
        );
    }

    #[test]
    fn test_plan_mode_allows_safe_bash() {
        let args = serde_json::json!({"command": "cargo test --release"});
        assert_eq!(
            check_tool("Bash", &args, ApprovalMode::Plan, &[]),
            ToolApproval::AutoApprove,
        );
    }

    #[test]
    fn test_plan_mode_blocks_dangerous_bash() {
        let args = serde_json::json!({"command": "rm -rf target/"});
        assert_eq!(
            check_tool("Bash", &args, ApprovalMode::Plan, &[]),
            ToolApproval::Blocked,
        );
    }

    #[test]
    fn test_plan_mode_blocks_write_tools() {
        assert_eq!(
            check_tool("Write", &serde_json::json!({}), ApprovalMode::Plan, &[]),
            ToolApproval::Blocked,
        );
        assert_eq!(
            check_tool("Edit", &serde_json::json!({}), ApprovalMode::Plan, &[]),
            ToolApproval::Blocked,
        );
        assert_eq!(
            check_tool("Delete", &serde_json::json!({}), ApprovalMode::Plan, &[]),
            ToolApproval::Blocked,
        );
    }

    #[test]
    fn test_plan_mode_allows_web_fetch() {
        let args = serde_json::json!({"url": "https://example.com"});
        assert_eq!(
            check_tool("WebFetch", &args, ApprovalMode::Plan, &[]),
            ToolApproval::AutoApprove,
        );
    }

    // ── Bash classifier tests ──

    // ── Settings tests ──

    #[test]
    fn test_settings_default() {
        let s = Settings::default();
        assert!(s.approval.allowed_commands.is_empty());
    }

    #[test]
    fn test_settings_roundtrip() {
        let mut s = Settings::default();
        s.approval.allowed_commands.push("docker compose up".into());
        let toml_str = toml::to_string_pretty(&s).unwrap();
        let parsed: Settings = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.approval.allowed_commands.len(), 1);
        assert_eq!(parsed.approval.allowed_commands[0], "docker compose up");
    }
}
