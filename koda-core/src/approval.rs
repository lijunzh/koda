//! Approval modes and tool confirmation.
//!
//! Three modes control how Koda handles tool confirmations:
//! - **Auto** (default): Auto-approve everything. Full trust in the model.
//! - **Strict**: Every non-read action requires explicit confirmation.
//! - **Safe**: Local-read-only, remote actions allowed. No filesystem mutations.
//!
//! Tool effects are classified via [`ToolEffect`] and bash commands are
//! further refined by [`crate::bash_safety::classify_bash_command`].

use crate::bash_safety::classify_bash_command;
use crate::tools::ToolEffect;
use path_clean::PathClean;
use std::path::Path;
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
            Self::Safe => "local-read-only, remote actions allowed",
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
    /// Execute but display what's happening (de-escalation).
    /// Agent continues automatically; user's next input is implicit consent.
    Notify,
    /// Show confirmation dialog.
    NeedsConfirmation,
    /// Safe mode: show what would happen, don't execute.
    Blocked,
}

/// Read-only tools that auto-approve in all modes (including Safe).
/// These never modify the filesystem or have destructive side effects.
///
/// **Superseded by `classify_tool()`** — kept for reference in tests.
#[cfg(test)]
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
///
/// Uses the [`ToolEffect`] classification to apply a per-mode decision matrix:
///
/// | ToolEffect     | Auto          | Strict        | Safe        |
/// |----------------|---------------|---------------|-------------|
/// | ReadOnly       | ✅ auto        | ✅ auto        | ✅ auto      |
/// | RemoteAction   | ✅ auto        | ✅ auto        | ✅ auto      |
/// | LocalMutation  | ✅ auto        | ⚠️ confirm     | ❌ blocked   |
/// | Destructive    | ⚠️ confirm    | ⚠️ confirm     | ❌ blocked   |
///
/// Additional hardcoded floors:
/// - Writes outside project root → NeedsConfirmation (#218)
/// - Bash path escapes → NeedsConfirmation
pub fn check_tool(
    tool_name: &str,
    args: &serde_json::Value,
    mode: ApprovalMode,
    project_root: Option<&Path>,
    mcp_effect: Option<ToolEffect>,
    delegation: Option<&crate::delegation::DelegationScope>,
) -> ToolApproval {
    // Delegation scope: tool allowlist check
    if let Some(scope) = delegation
        && !scope.is_tool_allowed(tool_name)
    {
        return ToolApproval::Blocked;
    }

    // Classify the tool's effect (MCP override takes precedence)
    let effect = mcp_effect.unwrap_or_else(|| resolve_effect(tool_name, args));

    // Read-only tools always auto-approve in every mode
    if effect == ToolEffect::ReadOnly {
        return ToolApproval::AutoApprove;
    }

    // Delegation scope: filesystem write check
    if let (Some(scope), Some(root)) = (delegation, project_root)
        && matches!(effect, ToolEffect::LocalMutation | ToolEffect::Destructive)
        && let Some(p) = extract_write_path(tool_name, args)
        && !scope.can_write(Path::new(p), root)
    {
        return ToolApproval::Blocked;
    }

    // Hardcoded floor: writes outside project root always need confirmation (#218)
    if let Some(root) = project_root {
        if is_outside_project(tool_name, args, root) {
            return ToolApproval::NeedsConfirmation;
        }
        // Bash path lint: check for cd/path escapes
        if tool_name == "Bash" {
            let command = args
                .get("command")
                .or(args.get("cmd"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let lint = crate::bash_path_lint::lint_bash_paths(command, root);
            if lint.has_warnings() {
                return ToolApproval::NeedsConfirmation;
            }
        }
    }

    // Apply the ToolEffect × ApprovalMode matrix
    match mode {
        ApprovalMode::Auto => match effect {
            ToolEffect::ReadOnly => ToolApproval::AutoApprove,
            ToolEffect::RemoteAction => ToolApproval::AutoApprove,
            ToolEffect::LocalMutation => ToolApproval::AutoApprove,
            ToolEffect::Destructive => ToolApproval::NeedsConfirmation,
        },
        ApprovalMode::Strict => match effect {
            ToolEffect::ReadOnly => ToolApproval::AutoApprove,
            ToolEffect::RemoteAction => ToolApproval::AutoApprove,
            ToolEffect::LocalMutation => ToolApproval::NeedsConfirmation,
            ToolEffect::Destructive => ToolApproval::NeedsConfirmation,
        },
        ApprovalMode::Safe => match effect {
            ToolEffect::ReadOnly => ToolApproval::AutoApprove,
            ToolEffect::RemoteAction => ToolApproval::AutoApprove,
            ToolEffect::LocalMutation | ToolEffect::Destructive => ToolApproval::Blocked,
        },
    }
}

/// Resolve the effective [`ToolEffect`] for a tool call.
///
/// For Bash, refines the generic `LocalMutation` classification by
/// parsing the actual command string.
fn resolve_effect(tool_name: &str, args: &serde_json::Value) -> ToolEffect {
    let base = crate::tools::classify_tool(tool_name);

    if tool_name == "Bash" {
        let command = args
            .get("command")
            .or(args.get("cmd"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        return classify_bash_command(command);
    }

    base
}

/// Phase-aware gating for Auto mode.
/// Extract the file path that a write tool targets.
fn extract_write_path<'a>(tool_name: &str, args: &'a serde_json::Value) -> Option<&'a str> {
    match tool_name {
        "Write" | "Edit" | "Delete" => args
            .get("path")
            .or(args.get("file_path"))
            .and_then(|v| v.as_str()),
        _ => None,
    }
}

/// Whether a file tool targets a path outside the project root (#218).
/// Hardcoded floor: always NeedsConfirmation regardless of mode or phase.
fn is_outside_project(tool_name: &str, args: &serde_json::Value, project_root: &Path) -> bool {
    let path_arg = match tool_name {
        "Write" | "Edit" | "Delete" => args
            .get("path")
            .or(args.get("file_path"))
            .and_then(|v| v.as_str()),
        _ => None,
    };
    match path_arg {
        Some(p) => {
            let requested = Path::new(p);
            let abs_path = if requested.is_absolute() {
                requested.to_path_buf()
            } else {
                project_root.join(requested)
            };
            // Try canonicalize (follows symlinks, resolves ..).
            // Falls back to lexical clean for new files that don't exist yet.
            let resolved = abs_path.canonicalize().unwrap_or_else(|_| abs_path.clean());
            // Also canonicalize project_root for consistent comparison
            let canon_root = project_root
                .canonicalize()
                .unwrap_or_else(|_| project_root.to_path_buf());
            !resolved.starts_with(&canon_root)
        }
        None => false,
    }
}

// ── Settings persistence ──────────────────────────────────

pub use crate::settings::{LastProvider, Settings};

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
                check_tool(
                    tool,
                    &serde_json::json!({}),
                    ApprovalMode::Safe,
                    None,
                    None,
                    None,
                ),
                ToolApproval::AutoApprove,
                "{tool} should auto-approve even in Safe mode"
            );
        }
    }

    #[test]
    fn test_write_tools_blocked_in_safe() {
        for tool in ["Write", "Edit", "Delete", "CreateAgent", "MemoryWrite"] {
            assert_eq!(
                check_tool(
                    tool,
                    &serde_json::json!({}),
                    ApprovalMode::Safe,
                    None,
                    None,
                    None,
                ),
                ToolApproval::Blocked,
                "{tool} should be blocked in Safe mode"
            );
        }
    }

    #[test]
    fn test_auto_approves_non_destructive_in_executing() {
        // In Auto mode during Executing with plan_approved, non-destructive
        // mutating tools are auto-approved.
        for tool in ["Write", "Edit", "Bash", "WebFetch"] {
            assert_eq!(
                check_tool(
                    tool,
                    &serde_json::json!({}),
                    ApprovalMode::Auto,
                    None,
                    None,
                    None,
                ),
                ToolApproval::AutoApprove,
            );
        }
    }

    #[test]
    fn test_auto_confirms_destructive_ops() {
        // Delete is always destructive → NeedsConfirmation even in Auto + Executing
        assert_eq!(
            check_tool(
                "Delete",
                &serde_json::json!({}),
                ApprovalMode::Auto,
                None,
                None,
                None,
            ),
            ToolApproval::NeedsConfirmation,
        );
    }

    #[test]
    fn test_safe_bash_auto_approved_in_strict() {
        // Read-only bash: auto-approved in Strict
        let args = serde_json::json!({"command": "git status"});
        assert_eq!(
            check_tool("Bash", &args, ApprovalMode::Strict, None, None, None,),
            ToolApproval::AutoApprove,
        );
    }

    #[test]
    fn test_dev_workflow_bash_needs_confirmation_in_strict() {
        // cargo test is LocalMutation → NeedsConfirmation in Strict
        let args = serde_json::json!({"command": "cargo test --release"});
        assert_eq!(
            check_tool("Bash", &args, ApprovalMode::Strict, None, None, None,),
            ToolApproval::NeedsConfirmation,
        );
    }

    #[test]
    fn test_dangerous_bash_needs_confirmation_in_strict() {
        // Destructive bash: NeedsConfirmation in Strict
        let args = serde_json::json!({"command": "rm -rf target/"});
        assert_eq!(
            check_tool("Bash", &args, ApprovalMode::Strict, None, None, None,),
            ToolApproval::NeedsConfirmation,
        );
    }

    #[test]
    fn test_write_needs_confirmation_in_strict() {
        assert_eq!(
            check_tool(
                "Write",
                &serde_json::json!({}),
                ApprovalMode::Strict,
                None,
                None,
                None,
            ),
            ToolApproval::NeedsConfirmation,
        );
    }

    #[test]
    fn test_invoke_agent_auto_approved() {
        let args = serde_json::json!({"agent_name": "reviewer", "prompt": "review this"});
        for mode in [ApprovalMode::Auto, ApprovalMode::Strict, ApprovalMode::Safe] {
            assert_eq!(
                check_tool("InvokeAgent", &args, mode, None, None, None,),
                ToolApproval::AutoApprove,
            );
        }
    }

    #[test]
    fn test_safe_mode_allows_safe_bash() {
        // Read-only bash: git status is ReadOnly → auto-approved in Safe
        let args = serde_json::json!({"command": "git status"});
        assert_eq!(
            check_tool("Bash", &args, ApprovalMode::Safe, None, None, None,),
            ToolApproval::AutoApprove,
        );
    }

    #[test]
    fn test_safe_mode_allows_remote_action_bash() {
        // gh issue create is RemoteAction → auto-approved in Safe
        let args = serde_json::json!({"command": "gh issue create --title 'bug'"});
        assert_eq!(
            check_tool("Bash", &args, ApprovalMode::Safe, None, None, None,),
            ToolApproval::AutoApprove,
        );
    }

    #[test]
    fn test_safe_mode_blocks_dev_workflow_bash() {
        // cargo test is LocalMutation (dev-workflow) → blocked in Safe
        let args = serde_json::json!({"command": "cargo test --release"});
        assert_eq!(
            check_tool("Bash", &args, ApprovalMode::Safe, None, None, None,),
            ToolApproval::Blocked,
        );
    }

    #[test]
    fn test_safe_mode_blocks_dangerous_bash() {
        // Destructive bash: blocked in Safe
        let args = serde_json::json!({"command": "rm -rf target/"});
        assert_eq!(
            check_tool("Bash", &args, ApprovalMode::Safe, None, None, None,),
            ToolApproval::Blocked,
        );
    }

    #[test]
    fn test_safe_mode_allows_web_fetch() {
        let args = serde_json::json!({"url": "https://example.com"});
        assert_eq!(
            check_tool("WebFetch", &args, ApprovalMode::Safe, None, None, None,),
            ToolApproval::AutoApprove,
        );
    }

    // ── Path scoping tests (#218) ──────────────────────────

    #[test]
    fn test_write_outside_project_needs_confirmation() {
        let root = Path::new("/home/user/project");
        let args = serde_json::json!({"path": "/etc/hosts"});
        assert_eq!(
            check_tool("Write", &args, ApprovalMode::Auto, Some(root), None, None,),
            ToolApproval::NeedsConfirmation,
        );
    }

    #[test]
    fn test_write_inside_project_auto_approved() {
        let root = Path::new("/home/user/project");
        let args = serde_json::json!({"path": "src/main.rs"});
        assert_eq!(
            check_tool("Write", &args, ApprovalMode::Auto, Some(root), None, None,),
            ToolApproval::AutoApprove,
        );
    }

    #[test]
    fn test_edit_with_dotdot_escape_needs_confirmation() {
        let root = Path::new("/home/user/project");
        let args = serde_json::json!({"path": "../../../etc/passwd"});
        assert_eq!(
            check_tool("Edit", &args, ApprovalMode::Auto, Some(root), None, None,),
            ToolApproval::NeedsConfirmation,
        );
    }

    #[test]
    fn test_bash_cd_outside_needs_confirmation() {
        let root = Path::new("/home/user/project");
        let args = serde_json::json!({"command": "cd /tmp && ls"});
        assert_eq!(
            check_tool("Bash", &args, ApprovalMode::Auto, Some(root), None, None,),
            ToolApproval::NeedsConfirmation,
        );
    }

    #[test]
    fn test_bash_cd_inside_auto_approved() {
        let root = Path::new("/home/user/project");
        let args = serde_json::json!({"command": "cd src && ls"});
        assert_eq!(
            check_tool("Bash", &args, ApprovalMode::Auto, Some(root), None, None,),
            ToolApproval::AutoApprove,
        );
    }

    #[test]
    fn test_no_project_root_skips_path_check() {
        let args = serde_json::json!({"path": "/etc/hosts"});
        assert_eq!(
            check_tool("Write", &args, ApprovalMode::Auto, None, None, None,),
            ToolApproval::AutoApprove,
        );
    }
}
