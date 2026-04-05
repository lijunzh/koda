//! Approval modes and tool confirmation.
//!
//! Two modes control how Koda handles tool confirmations:
//! - **Auto** (default): Auto-approve everything. Destructive ops need confirmation.
//! - **Confirm**: Every non-read action requires explicit confirmation.
//!
//! Tool effects are classified via [`crate::tools::ToolEffect`] and bash commands are
//! further refined by [`crate::bash_safety::classify_bash_command`].
//!
//! ## Design (DESIGN.md)
//!
//! - **Security Model (P2)**: Two modes + hardcoded floors. Hardcoded floors
//!   override mode settings for destructive ops — this is not configurable.
//! - **File Lifecycle Tracking (P2)**: Auto-approve deleting files koda
//!   created in the same turn. See [`crate::file_tracker::FileTracker`].

use crate::bash_safety::classify_bash_command;
use crate::file_tracker::FileTracker;
use crate::tools::ToolEffect;
use path_clean::PathClean;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

// ── Approval Mode ─────────────────────────────────────────

/// The two approval modes.
///
/// # Examples
///
/// ```
/// use koda_core::approval::ApprovalMode;
///
/// let mode = ApprovalMode::Auto;
/// assert_eq!(mode.as_str(), "auto");
/// assert_eq!(mode.next(), ApprovalMode::Confirm);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ApprovalMode {
    /// Every non-read action needs explicit confirmation.
    Confirm = 0,
    /// Full auto: approve everything except destructive ops.
    Auto = 1,
}

impl ApprovalMode {
    /// Toggle between the two modes.
    pub fn next(self) -> Self {
        match self {
            Self::Auto => Self::Confirm,
            Self::Confirm => Self::Auto,
        }
    }

    /// Stable string representation for persistence.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Confirm => "confirm",
        }
    }

    /// Short label for display.
    pub fn label(self) -> &'static str {
        match self {
            Self::Confirm => "confirm",
            Self::Auto => "auto",
        }
    }

    /// Human-readable description of this mode.
    pub fn description(self) -> &'static str {
        match self {
            Self::Confirm => "confirm every non-read action",
            Self::Auto => "auto-approve, confirm destructive only",
        }
    }

    /// Parse an approval mode from a user-provided string.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "auto" | "yolo" | "accept" => Some(Self::Auto),
            "confirm" | "strict" | "normal" => Some(Self::Confirm),
            // Legacy: "safe" and "plan" map to Confirm (closest equivalent)
            "safe" | "plan" | "readonly" => Some(Self::Confirm),
            _ => None,
        }
    }
}

impl From<u8> for ApprovalMode {
    fn from(v: u8) -> Self {
        match v {
            0 => Self::Confirm,
            _ => Self::Auto, // default is Auto
        }
    }
}

/// Thread-safe shared mode, readable from prompt formatter and input handlers.
pub type SharedMode = Arc<AtomicU8>;

/// Create a new atomic shared mode initialized to `mode`.
pub fn new_shared_mode(mode: ApprovalMode) -> SharedMode {
    Arc::new(AtomicU8::new(mode as u8))
}

/// Read the current approval mode from shared state.
pub fn read_mode(shared: &SharedMode) -> ApprovalMode {
    ApprovalMode::from(shared.load(Ordering::Relaxed))
}

/// Atomically set the approval mode.
pub fn set_mode(shared: &SharedMode, mode: ApprovalMode) {
    shared.store(mode as u8, Ordering::Relaxed);
}

/// Cycle to the next approval mode and return it.
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
    /// Blocked (delegation scope violation).
    Blocked,
}

/// Decide whether a tool call should be auto-approved, confirmed, or blocked.
///
/// Decision matrix:
///
/// | ToolEffect     | Auto          | Confirm       |
/// |----------------|---------------|---------------|
/// | ReadOnly       | ✅ auto        | ✅ auto        |
/// | RemoteAction   | ✅ auto        | ✅ auto        |
/// | LocalMutation  | ✅ auto        | ⚠️ confirm     |
/// | Destructive    | ⚠️ confirm    | ⚠️ confirm     |
///
/// Additional hardcoded floors:
/// - Writes outside project root → NeedsConfirmation (#218)
/// - Bash path escapes → NeedsConfirmation
/// - Delete of Koda-owned file → AutoApprove (#465)
/// - EmailSend → LocalMutation (not RemoteAction) to prevent
///   prompt-injection data exfiltration (#525)
pub fn check_tool(
    tool_name: &str,
    args: &serde_json::Value,
    mode: ApprovalMode,
    project_root: Option<&Path>,
) -> ToolApproval {
    check_tool_with_tracker(tool_name, args, mode, project_root, None)
}

/// Like [`check_tool`] but with an optional file tracker for ownership checks.
///
/// When a `FileTracker` is provided and the tool is `Delete` targeting a file
/// that Koda created in this session, the destructive classification is
/// downgraded to auto-approve (net-zero effect: Koda created it, Koda removes it).
pub fn check_tool_with_tracker(
    tool_name: &str,
    args: &serde_json::Value,
    mode: ApprovalMode,
    project_root: Option<&Path>,
    file_tracker: Option<&FileTracker>,
) -> ToolApproval {
    // Classify the tool's effect
    let effect = resolve_effect(tool_name, args);

    // Read-only tools always auto-approve in every mode
    if effect == ToolEffect::ReadOnly {
        return ToolApproval::AutoApprove;
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

    // File lifecycle: Koda-owned files bypass destructive gate (#465)
    if tool_name == "Delete"
        && let Some(tracker) = file_tracker
        && let Some(root) = project_root
        && let Some(abs_path) = crate::file_tracker::resolve_file_path_from_args(args, root)
        && tracker.is_owned(&abs_path)
    {
        return ToolApproval::AutoApprove;
    }

    // Apply the ToolEffect × ApprovalMode matrix
    match mode {
        ApprovalMode::Auto => match effect {
            ToolEffect::ReadOnly | ToolEffect::RemoteAction | ToolEffect::LocalMutation => {
                ToolApproval::AutoApprove
            }
            ToolEffect::Destructive => ToolApproval::NeedsConfirmation,
        },
        ApprovalMode::Confirm => match effect {
            ToolEffect::ReadOnly | ToolEffect::RemoteAction => ToolApproval::AutoApprove,
            ToolEffect::LocalMutation | ToolEffect::Destructive => ToolApproval::NeedsConfirmation,
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

/// Whether a file tool targets a path outside the project root (#218).
/// Hardcoded floor: always NeedsConfirmation regardless of mode.
///
/// Temp directories (`/tmp`, `$TMPDIR`) are explicitly allowed (#560).
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
            // Canonicalize for symlink resolution (macOS /var → /private/var).
            // For new files, canonicalize the parent dir and append the filename.
            let resolved = abs_path.canonicalize().unwrap_or_else(|_| {
                if let Some(parent) = abs_path.parent()
                    && let Ok(canon_parent) = parent.canonicalize()
                    && let Some(name) = abs_path.file_name()
                {
                    return canon_parent.join(name);
                }
                abs_path.clean()
            });
            let canon_root = project_root
                .canonicalize()
                .unwrap_or_else(|_| project_root.to_path_buf());
            let outside = !resolved.starts_with(&canon_root);
            // Allow temp directories (#560)
            if outside && crate::bash_path_lint::is_safe_external_path(&resolved) {
                return false;
            }
            outside
        }
        None => false,
    }
}

// ── Settings persistence ──────────────────────────────────

/// Re-export settings types used by approval persistence.
pub use crate::settings::{LastProvider, Settings};

// ── Tests ─────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Mode tests ──

    #[test]
    fn test_mode_cycle() {
        assert_eq!(ApprovalMode::Auto.next(), ApprovalMode::Confirm);
        assert_eq!(ApprovalMode::Confirm.next(), ApprovalMode::Auto);
    }

    #[test]
    fn test_mode_from_str() {
        // New names
        assert_eq!(ApprovalMode::parse("auto"), Some(ApprovalMode::Auto));
        assert_eq!(ApprovalMode::parse("confirm"), Some(ApprovalMode::Confirm));
        // Legacy aliases
        assert_eq!(ApprovalMode::parse("yolo"), Some(ApprovalMode::Auto));
        assert_eq!(ApprovalMode::parse("strict"), Some(ApprovalMode::Confirm));
        assert_eq!(ApprovalMode::parse("normal"), Some(ApprovalMode::Confirm));
        assert_eq!(ApprovalMode::parse("safe"), Some(ApprovalMode::Confirm));
        assert_eq!(ApprovalMode::parse("plan"), Some(ApprovalMode::Confirm));
        assert_eq!(ApprovalMode::parse("readonly"), Some(ApprovalMode::Confirm));
        assert_eq!(ApprovalMode::parse("accept"), Some(ApprovalMode::Auto));
        assert_eq!(ApprovalMode::parse("nope"), None);
    }

    #[test]
    fn test_mode_from_u8() {
        assert_eq!(ApprovalMode::from(0), ApprovalMode::Confirm);
        assert_eq!(ApprovalMode::from(1), ApprovalMode::Auto);
        assert_eq!(ApprovalMode::from(99), ApprovalMode::Auto); // default is Auto
    }

    #[test]
    fn test_shared_mode_cycle() {
        let shared = new_shared_mode(ApprovalMode::Auto);
        assert_eq!(read_mode(&shared), ApprovalMode::Auto);
        let next = cycle_mode(&shared);
        assert_eq!(next, ApprovalMode::Confirm);
        assert_eq!(read_mode(&shared), ApprovalMode::Confirm);
    }

    // ── Tool approval tests ──

    /// Read-only tools auto-approve in every mode.
    const READ_ONLY_TOOLS: &[&str] = &[
        "Read",
        "List",
        "Grep",
        "Glob",
        "MemoryRead",
        "ListAgents",
        "InvokeAgent",
        "WebFetch",
        "WebSearch",
        "ListSkills",
        "ActivateSkill",
    ];

    #[test]
    fn test_read_tools_always_approved() {
        for tool in READ_ONLY_TOOLS {
            assert_eq!(
                check_tool(tool, &serde_json::json!({}), ApprovalMode::Confirm, None),
                ToolApproval::AutoApprove,
                "{tool} should auto-approve even in Confirm mode"
            );
        }
    }

    #[test]
    fn test_write_tools_need_confirmation_in_confirm() {
        for tool in [
            "Write",
            "Edit",
            "Delete",
            "MemoryWrite",
            "EmailSend",
            "TodoWrite",
        ] {
            assert_eq!(
                check_tool(tool, &serde_json::json!({}), ApprovalMode::Confirm, None),
                ToolApproval::NeedsConfirmation,
                "{tool} should need confirmation in Confirm mode"
            );
        }
    }

    #[test]
    fn test_auto_approves_non_destructive() {
        for tool in ["Write", "Edit", "Bash", "WebFetch", "TodoWrite"] {
            assert_eq!(
                check_tool(tool, &serde_json::json!({}), ApprovalMode::Auto, None),
                ToolApproval::AutoApprove,
            );
        }
    }

    #[test]
    fn test_auto_confirms_destructive_ops() {
        assert_eq!(
            check_tool("Delete", &serde_json::json!({}), ApprovalMode::Auto, None,),
            ToolApproval::NeedsConfirmation,
        );
    }

    #[test]
    fn test_safe_bash_auto_approved_in_confirm() {
        let args = serde_json::json!({"command": "git status"});
        assert_eq!(
            check_tool("Bash", &args, ApprovalMode::Confirm, None),
            ToolApproval::AutoApprove,
        );
    }

    /// gh read-only commands should auto-approve even in Confirm mode (#518).
    #[test]
    fn test_gh_read_only_auto_approved() {
        for cmd in [
            "gh issue view 42",
            "gh pr view 99",
            "gh pr list",
            "gh issue list",
        ] {
            let args = serde_json::json!({"command": cmd});
            assert_eq!(
                check_tool("Bash", &args, ApprovalMode::Confirm, None),
                ToolApproval::AutoApprove,
                "{cmd} should auto-approve even in Confirm mode"
            );
        }
    }

    /// gh destructive commands need confirmation even in Auto mode (#518).
    #[test]
    fn test_gh_destructive_needs_confirmation() {
        for cmd in [
            "gh pr merge 42 --squash",
            "gh issue delete 42",
            "gh repo delete owner/repo",
        ] {
            let args = serde_json::json!({"command": cmd});
            assert_eq!(
                check_tool("Bash", &args, ApprovalMode::Auto, None),
                ToolApproval::NeedsConfirmation,
                "{cmd} should need confirmation even in Auto mode"
            );
        }
    }

    /// gh mutation commands (create/edit/close) auto-approve in Auto, confirm in Confirm (#518).
    #[test]
    fn test_gh_mutation_auto_approved_in_auto() {
        for cmd in [
            "gh issue create --title 'bug'",
            "gh issue edit 42",
            "gh pr create",
        ] {
            let args = serde_json::json!({"command": cmd});
            assert_eq!(
                check_tool("Bash", &args, ApprovalMode::Auto, None),
                ToolApproval::AutoApprove,
                "{cmd} should auto-approve in Auto mode"
            );
            assert_eq!(
                check_tool("Bash", &args, ApprovalMode::Confirm, None),
                ToolApproval::NeedsConfirmation,
                "{cmd} should need confirmation in Confirm mode"
            );
        }
    }

    #[test]
    fn test_dev_workflow_bash_needs_confirmation_in_confirm() {
        let args = serde_json::json!({"command": "cargo test --release"});
        assert_eq!(
            check_tool("Bash", &args, ApprovalMode::Confirm, None),
            ToolApproval::NeedsConfirmation,
        );
    }

    #[test]
    fn test_dangerous_bash_needs_confirmation() {
        let args = serde_json::json!({"command": "rm -rf target/"});
        for mode in [ApprovalMode::Auto, ApprovalMode::Confirm] {
            assert_eq!(
                check_tool("Bash", &args, mode, None),
                ToolApproval::NeedsConfirmation,
            );
        }
    }

    #[test]
    fn test_write_needs_confirmation_in_confirm() {
        assert_eq!(
            check_tool("Write", &serde_json::json!({}), ApprovalMode::Confirm, None,),
            ToolApproval::NeedsConfirmation,
        );
    }

    #[test]
    fn test_invoke_agent_auto_approved() {
        let args = serde_json::json!({"agent_name": "reviewer", "prompt": "review this"});
        for mode in [ApprovalMode::Auto, ApprovalMode::Confirm] {
            assert_eq!(
                check_tool("InvokeAgent", &args, mode, None),
                ToolApproval::AutoApprove,
            );
        }
    }

    // ── Path scoping tests (#218) ──────────────────────────

    #[test]
    fn test_write_outside_project_needs_confirmation() {
        let root = Path::new("/home/user/project");
        let args = serde_json::json!({"path": "/etc/hosts"});
        assert_eq!(
            check_tool("Write", &args, ApprovalMode::Auto, Some(root),),
            ToolApproval::NeedsConfirmation,
        );
    }

    #[test]
    fn test_write_inside_project_auto_approved() {
        let root = Path::new("/home/user/project");
        let args = serde_json::json!({"path": "src/main.rs"});
        assert_eq!(
            check_tool("Write", &args, ApprovalMode::Auto, Some(root),),
            ToolApproval::AutoApprove,
        );
    }

    #[test]
    fn test_edit_with_dotdot_escape_needs_confirmation() {
        let root = Path::new("/home/user/project");
        let args = serde_json::json!({"path": "../../../etc/passwd"});
        assert_eq!(
            check_tool("Edit", &args, ApprovalMode::Auto, Some(root),),
            ToolApproval::NeedsConfirmation,
        );
    }

    #[test]
    fn test_bash_cd_outside_needs_confirmation() {
        let root = Path::new("/home/user/project");
        let args = serde_json::json!({"command": "cd /etc && ls"});
        assert_eq!(
            check_tool("Bash", &args, ApprovalMode::Auto, Some(root),),
            ToolApproval::NeedsConfirmation,
        );
    }

    #[test]
    fn test_bash_cd_inside_auto_approved() {
        let root = Path::new("/home/user/project");
        let args = serde_json::json!({"command": "cd src && ls"});
        assert_eq!(
            check_tool("Bash", &args, ApprovalMode::Auto, Some(root),),
            ToolApproval::AutoApprove,
        );
    }

    #[test]
    fn test_no_project_root_skips_path_check() {
        let args = serde_json::json!({"path": "/etc/hosts"});
        assert_eq!(
            check_tool("Write", &args, ApprovalMode::Auto, None),
            ToolApproval::AutoApprove,
        );
    }

    // ── Temp path allowlist (#560) ──

    #[test]
    fn test_write_to_tmp_auto_approved() {
        let root = Path::new("/home/user/project");
        let args = serde_json::json!({"path": "/tmp/issue-draft.md"});
        assert_eq!(
            check_tool("Write", &args, ApprovalMode::Auto, Some(root)),
            ToolApproval::AutoApprove,
            "/tmp writes should auto-approve"
        );
    }

    #[test]
    fn test_bash_cd_tmp_auto_approved() {
        let root = Path::new("/home/user/project");
        let args = serde_json::json!({"command": "cd /tmp && ls"});
        assert_eq!(
            check_tool("Bash", &args, ApprovalMode::Auto, Some(root)),
            ToolApproval::AutoApprove,
            "cd /tmp should auto-approve"
        );
    }

    #[test]
    fn test_write_to_etc_still_blocked() {
        let root = Path::new("/home/user/project");
        let args = serde_json::json!({"path": "/etc/hosts"});
        assert_eq!(
            check_tool("Write", &args, ApprovalMode::Auto, Some(root)),
            ToolApproval::NeedsConfirmation,
            "/etc writes should still need confirmation"
        );
    }

    // ── File lifecycle (#465) tests ──

    #[tokio::test]
    async fn test_delete_owned_file_auto_approved() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = crate::db::Database::open(&dir.path().join("test.db"))
            .await
            .unwrap();
        let mut tracker = FileTracker::new("test-sess", db).await;
        let root = Path::new("/home/user/project");
        let owned_path = root.join("temp_output.md");
        tracker.track_created(owned_path).await;

        let args = serde_json::json!({"path": "temp_output.md"});
        assert_eq!(
            check_tool_with_tracker(
                "Delete",
                &args,
                ApprovalMode::Auto,
                Some(root),
                Some(&tracker),
            ),
            ToolApproval::AutoApprove,
            "Delete of Koda-owned file should auto-approve"
        );
    }

    #[tokio::test]
    async fn test_delete_unowned_file_needs_confirmation() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = crate::db::Database::open(&dir.path().join("test.db"))
            .await
            .unwrap();
        let tracker = FileTracker::new("test-sess", db).await;
        let root = Path::new("/home/user/project");

        let args = serde_json::json!({"path": "user_file.rs"});
        assert_eq!(
            check_tool_with_tracker(
                "Delete",
                &args,
                ApprovalMode::Auto,
                Some(root),
                Some(&tracker),
            ),
            ToolApproval::NeedsConfirmation,
            "Delete of unowned file should still need confirmation"
        );
    }

    #[tokio::test]
    async fn test_delete_owned_file_confirm_mode_auto_approved() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = crate::db::Database::open(&dir.path().join("test.db"))
            .await
            .unwrap();
        let mut tracker = FileTracker::new("test-sess", db).await;
        let root = Path::new("/home/user/project");
        let owned_path = root.join("scratch.txt");
        tracker.track_created(owned_path).await;

        let args = serde_json::json!({"path": "scratch.txt"});
        assert_eq!(
            check_tool_with_tracker(
                "Delete",
                &args,
                ApprovalMode::Confirm,
                Some(root),
                Some(&tracker),
            ),
            ToolApproval::AutoApprove,
            "Delete of Koda-owned file should auto-approve even in Confirm mode"
        );
    }

    #[test]
    fn test_no_tracker_falls_back_to_normal() {
        let root = Path::new("/home/user/project");
        let args = serde_json::json!({"path": "some_file.rs"});
        assert_eq!(
            check_tool_with_tracker("Delete", &args, ApprovalMode::Auto, Some(root), None,),
            ToolApproval::NeedsConfirmation,
            "Without tracker, Delete should still need confirmation"
        );
    }
}
