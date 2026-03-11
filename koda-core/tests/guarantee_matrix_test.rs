//! Guarantee matrix verification tests (#307, #293 Phase E).
//!
//! Tests every row × column in the approval mode guarantee matrix.
//! Each test verifies the ToolApproval returned by check_tool() for
//! a specific (action, mode) pair.
//!
//! Matrix rows (actions):
//!  1. Read files inside project
//!  2. Read files outside project
//!  3. Write files inside project
//!  4. Write files outside project
//!  5. Delete files
//!  6. Safe bash (git status, grep)
//!  7. Bash with write side-effect (echo > file)
//!  8. Destructive bash (rm -rf, git push --force)
//!  9. Bash with path escape (cd /tmp)
//! 10. Sub-agent invocation (InvokeAgent)
//! 11. MCP tool (readOnly: true) — via mcp_effect override
//! 12. MCP tool (readOnly: false) — via mcp_effect override
//! 13. MemoryWrite
//! 14. WebFetch (GET)
//! 15. gh issue create (RemoteAction bash)
//!
//! Columns: Auto, Strict, Safe

use koda_core::approval::{ApprovalMode, ToolApproval, check_tool};
use koda_core::tools::ToolEffect;
use std::path::Path;

fn root() -> &'static Path {
    Path::new("/home/user/project")
}

/// Helper: check a tool in all three modes and return (auto, strict, safe).
fn check_all(
    tool: &str,
    args: &serde_json::Value,
    mcp_effect: Option<ToolEffect>,
) -> (ToolApproval, ToolApproval, ToolApproval) {
    let auto = check_tool(
        tool,
        args,
        ApprovalMode::Auto,
        Some(root()),
        mcp_effect,
        None,
    );
    let strict = check_tool(
        tool,
        args,
        ApprovalMode::Strict,
        Some(root()),
        mcp_effect,
        None,
    );
    let safe = check_tool(
        tool,
        args,
        ApprovalMode::Safe,
        Some(root()),
        mcp_effect,
        None,
    );
    (auto, strict, safe)
}

// ── Row 1: Read files inside project ──

#[test]
fn matrix_read_inside_project() {
    let args = serde_json::json!({"path": "src/main.rs"});
    let (auto, strict, safe) = check_all("Read", &args, None);
    assert_eq!(auto, ToolApproval::AutoApprove);
    assert_eq!(strict, ToolApproval::AutoApprove);
    assert_eq!(safe, ToolApproval::AutoApprove);
}

// ── Row 2: Read files outside project ──
// Note: Read is ReadOnly → auto-approved in check_tool.
// Path scoping is enforced at execution time by safe_resolve_path.

#[test]
fn matrix_read_outside_project() {
    let args = serde_json::json!({"path": "/etc/passwd"});
    let (auto, strict, safe) = check_all("Read", &args, None);
    // Read is always ReadOnly at the approval level
    assert_eq!(auto, ToolApproval::AutoApprove);
    assert_eq!(strict, ToolApproval::AutoApprove);
    assert_eq!(safe, ToolApproval::AutoApprove);
}

// ── Row 3: Write files inside project ──

#[test]
fn matrix_write_inside_project() {
    let args = serde_json::json!({"path": "src/main.rs"});
    let (auto, strict, safe) = check_all("Write", &args, None);
    assert_eq!(auto, ToolApproval::AutoApprove); // plan approved
    assert_eq!(strict, ToolApproval::NeedsConfirmation);
    assert_eq!(safe, ToolApproval::Blocked);
}

// ── Row 4: Write files outside project ──

#[test]
fn matrix_write_outside_project() {
    let args = serde_json::json!({"path": "/etc/hosts"});
    let (auto, strict, safe) = check_all("Write", &args, None);
    // Hardcoded floor: outside project → NeedsConfirmation
    // Note: Safe mode also gets NeedsConfirmation here (floor overrides Blocked).
    // This is conservative — the user still can't approve in Safe mode.
    assert_eq!(auto, ToolApproval::NeedsConfirmation);
    assert_eq!(strict, ToolApproval::NeedsConfirmation);
    assert_eq!(safe, ToolApproval::NeedsConfirmation);
}

// ── Row 5: Delete files ──

#[test]
fn matrix_delete_files() {
    let args = serde_json::json!({"file_path": "old.rs"});
    let (auto, strict, safe) = check_all("Delete", &args, None);
    // Delete is Destructive
    assert_eq!(auto, ToolApproval::NeedsConfirmation);
    assert_eq!(strict, ToolApproval::NeedsConfirmation);
    assert_eq!(safe, ToolApproval::Blocked);
}

// ── Row 6: Safe bash (read-only commands) ──

#[test]
fn matrix_safe_bash() {
    let args = serde_json::json!({"command": "git status"});
    let (auto, strict, safe) = check_all("Bash", &args, None);
    assert_eq!(auto, ToolApproval::AutoApprove);
    assert_eq!(strict, ToolApproval::AutoApprove);
    assert_eq!(safe, ToolApproval::AutoApprove);
}

// ── Row 7: Bash with write side-effect ──

#[test]
fn matrix_bash_write_side_effect() {
    let args = serde_json::json!({"command": "echo hello > output.txt"});
    let (auto, strict, safe) = check_all("Bash", &args, None);
    assert_eq!(auto, ToolApproval::AutoApprove); // phase-gated, plan approved
    assert_eq!(strict, ToolApproval::NeedsConfirmation);
    assert_eq!(safe, ToolApproval::Blocked);
}

// ── Row 8: Destructive bash ──

#[test]
fn matrix_destructive_bash() {
    let args = serde_json::json!({"command": "rm -rf target/"});
    let (auto, strict, safe) = check_all("Bash", &args, None);
    assert_eq!(auto, ToolApproval::NeedsConfirmation); // destructive floor
    assert_eq!(strict, ToolApproval::NeedsConfirmation);
    assert_eq!(safe, ToolApproval::Blocked);
}

// ── Row 9: Bash with path escape ──

#[test]
fn matrix_bash_path_escape() {
    let args = serde_json::json!({"command": "cd /tmp && ls"});
    let (auto, strict, safe) = check_all("Bash", &args, None);
    // Path lint triggers NeedsConfirmation floor (same note as write outside)
    assert_eq!(auto, ToolApproval::NeedsConfirmation);
    assert_eq!(strict, ToolApproval::NeedsConfirmation);
    assert_eq!(safe, ToolApproval::NeedsConfirmation);
}

// ── Row 10: Sub-agent invocation ──

#[test]
fn matrix_invoke_agent() {
    let args = serde_json::json!({"agent_name": "reviewer", "prompt": "review"});
    let (auto, strict, safe) = check_all("InvokeAgent", &args, None);
    // InvokeAgent is ReadOnly (sub-agents inherit parent's mode)
    assert_eq!(auto, ToolApproval::AutoApprove);
    assert_eq!(strict, ToolApproval::AutoApprove);
    assert_eq!(safe, ToolApproval::AutoApprove);
}

// ── Row 11: MCP tool (readOnly: true) ──

#[test]
fn matrix_mcp_readonly_true() {
    let args = serde_json::json!({});
    let (auto, strict, safe) = check_all("github.list_issues", &args, Some(ToolEffect::ReadOnly));
    assert_eq!(auto, ToolApproval::AutoApprove);
    assert_eq!(strict, ToolApproval::AutoApprove);
    assert_eq!(safe, ToolApproval::AutoApprove);
}

// ── Row 12: MCP tool (readOnly: false/unset) ──

#[test]
fn matrix_mcp_readonly_false() {
    let args = serde_json::json!({});
    let (auto, strict, safe) =
        check_all("filesystem.write", &args, Some(ToolEffect::LocalMutation));
    assert_eq!(auto, ToolApproval::AutoApprove); // phase-gated, plan approved
    assert_eq!(strict, ToolApproval::NeedsConfirmation);
    assert_eq!(safe, ToolApproval::Blocked);
}

// ── Row 13: MemoryWrite ──

#[test]
fn matrix_memory_write() {
    let args = serde_json::json!({"content": "remember this"});
    let (auto, strict, safe) = check_all("MemoryWrite", &args, None);
    assert_eq!(auto, ToolApproval::AutoApprove); // phase-gated
    assert_eq!(strict, ToolApproval::NeedsConfirmation);
    assert_eq!(safe, ToolApproval::Blocked);
}

// ── Row 14: WebFetch (GET) ──

#[test]
fn matrix_web_fetch() {
    let args = serde_json::json!({"url": "https://example.com"});
    let (auto, strict, safe) = check_all("WebFetch", &args, None);
    assert_eq!(auto, ToolApproval::AutoApprove);
    assert_eq!(strict, ToolApproval::AutoApprove);
    assert_eq!(safe, ToolApproval::AutoApprove);
}

// ── Row 15: gh issue create (RemoteAction bash) ──

#[test]
fn matrix_gh_issue_create() {
    let args = serde_json::json!({"command": "gh issue create --title 'bug'"});
    let (auto, strict, safe) = check_all("Bash", &args, None);
    // gh CLI is RemoteAction
    assert_eq!(auto, ToolApproval::AutoApprove);
    assert_eq!(strict, ToolApproval::AutoApprove);
    assert_eq!(safe, ToolApproval::AutoApprove);
}

// ── Row 16: MCP tool (config override: RemoteAction) ──

#[test]
fn matrix_mcp_config_override_remote_action() {
    let args = serde_json::json!({});
    let (auto, strict, safe) =
        check_all("github.create_issue", &args, Some(ToolEffect::RemoteAction));
    assert_eq!(auto, ToolApproval::AutoApprove);
    assert_eq!(strict, ToolApproval::AutoApprove);
    assert_eq!(safe, ToolApproval::AutoApprove);
}

// ── Delegation scope tests ──

#[test]
fn matrix_delegation_blocks_unauthorized_tool() {
    use koda_core::delegation::{DelegationScope, FsGrant};
    let scope = DelegationScope {
        mode: ApprovalMode::Auto,
        fs_grant: FsGrant::FullProject,
        allowed_tools: Some(vec!["Read".to_string(), "Grep".to_string()]),
        can_delegate: false,
    };
    let args = serde_json::json!({"path": "src/main.rs"});
    let result = check_tool(
        "Write",
        &args,
        ApprovalMode::Auto,
        Some(root()),
        None,
        Some(&scope),
    );
    assert_eq!(result, ToolApproval::Blocked);
}

#[test]
fn matrix_delegation_blocks_write_outside_grant() {
    use koda_core::delegation::{DelegationScope, FsGrant};
    let scope = DelegationScope {
        mode: ApprovalMode::Auto,
        fs_grant: FsGrant::Scoped {
            read_paths: vec![std::path::PathBuf::from(".")],
            write_paths: vec![std::path::PathBuf::from("src/")],
        },
        allowed_tools: None,
        can_delegate: false,
    };
    let args = serde_json::json!({"path": "tests/test.rs"});
    let result = check_tool(
        "Write",
        &args,
        ApprovalMode::Auto,
        Some(root()),
        None,
        Some(&scope),
    );
    assert_eq!(result, ToolApproval::Blocked);
}

#[test]
fn matrix_delegation_allows_write_inside_grant() {
    use koda_core::delegation::{DelegationScope, FsGrant};
    let scope = DelegationScope {
        mode: ApprovalMode::Auto,
        fs_grant: FsGrant::Scoped {
            read_paths: vec![std::path::PathBuf::from(".")],
            write_paths: vec![std::path::PathBuf::from("src/")],
        },
        allowed_tools: None,
        can_delegate: false,
    };
    let args = serde_json::json!({"path": "src/main.rs"});
    let result = check_tool(
        "Write",
        &args,
        ApprovalMode::Auto,
        Some(root()),
        None,
        Some(&scope),
    );
    assert_eq!(result, ToolApproval::AutoApprove);
}
