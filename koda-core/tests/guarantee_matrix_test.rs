//! Guarantee matrix verification tests (#307, #293 Phase E, updated #855).
//!
//! Tests every row × column in the TrustMode guarantee matrix.
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
//!  9. Bash with path escape (cd /etc)
//! 10. Sub-agent invocation (InvokeAgent)
//! 11. MemoryWrite
//! 12. WebFetch (GET)
//! 13. gh issue create (LocalMutation bash)
//!
//! Columns: Auto, Safe
//!
//! Key design change in #855: Auto mode auto-approves destructive ops
//! (sandbox enforces the perimeter). Only outside-project writes and
//! path escapes still need confirmation in Auto mode.

use koda_core::trust::{ToolApproval, TrustMode, check_tool};
use std::path::Path;

fn root() -> &'static Path {
    Path::new("/home/user/project")
}

/// Expected Auto-mode approval for mutation/destructive ops.
/// When sandbox is available, Auto auto-approves. Without sandbox,
/// mutations downgrade to NeedsConfirmation (#860).
fn auto_mutation_expected() -> ToolApproval {
    if koda_core::sandbox::is_available() {
        ToolApproval::AutoApprove
    } else {
        ToolApproval::NeedsConfirmation
    }
}

/// Helper: check a tool in both modes and return (auto, confirm).
fn check_both(tool: &str, args: &serde_json::Value) -> (ToolApproval, ToolApproval) {
    let auto = check_tool(tool, args, TrustMode::Auto, Some(root()));
    let confirm = check_tool(tool, args, TrustMode::Safe, Some(root()));
    (auto, confirm)
}

// ── Row 1: Read files inside project ──

#[test]
fn matrix_read_inside_project() {
    let args = serde_json::json!({"path": "src/main.rs"});
    let (auto, confirm) = check_both("Read", &args);
    assert_eq!(auto, ToolApproval::AutoApprove);
    assert_eq!(confirm, ToolApproval::AutoApprove);
}

// ── Row 2: Read files outside project ──
// Note: Read is ReadOnly → auto-approved in check_tool.
// Path scoping is enforced at execution time by safe_resolve_path.

#[test]
fn matrix_read_outside_project() {
    let args = serde_json::json!({"path": "/etc/passwd"});
    let (auto, confirm) = check_both("Read", &args);
    assert_eq!(auto, ToolApproval::AutoApprove);
    assert_eq!(confirm, ToolApproval::AutoApprove);
}

// ── Row 3: Write files inside project ──

#[test]
fn matrix_write_inside_project() {
    let args = serde_json::json!({"path": "src/main.rs"});
    let (auto, confirm) = check_both("Write", &args);
    assert_eq!(auto, auto_mutation_expected());
    assert_eq!(confirm, ToolApproval::NeedsConfirmation);
}

// ── Row 4: Write files outside project ──

#[test]
fn matrix_write_outside_project() {
    let args = serde_json::json!({"path": "/etc/hosts"});
    let (auto, confirm) = check_both("Write", &args);
    assert_eq!(auto, ToolApproval::NeedsConfirmation);
    assert_eq!(confirm, ToolApproval::NeedsConfirmation);
}

// ── Row 5: Delete files ──

#[test]
fn matrix_delete_files() {
    let args = serde_json::json!({"file_path": "old.rs"});
    let (auto, confirm) = check_both("Delete", &args);
    // Auto: sandbox enforces perimeter, destructive ops auto-approved
    // (downgrades to NeedsConfirmation if sandbox unavailable, #860)
    assert_eq!(auto, auto_mutation_expected());
    assert_eq!(confirm, ToolApproval::NeedsConfirmation);
}

// ── Row 6: Safe bash (read-only commands) ──

#[test]
fn matrix_safe_bash() {
    let args = serde_json::json!({"command": "git status"});
    let (auto, confirm) = check_both("Bash", &args);
    assert_eq!(auto, ToolApproval::AutoApprove);
    assert_eq!(confirm, ToolApproval::AutoApprove);
}

// ── Row 7: Bash with write side-effect ──

#[test]
fn matrix_bash_write_side_effect() {
    let args = serde_json::json!({"command": "echo hello > output.txt"});
    let (auto, confirm) = check_both("Bash", &args);
    assert_eq!(auto, auto_mutation_expected());
    assert_eq!(confirm, ToolApproval::NeedsConfirmation);
}

// ── Row 8: Destructive bash ──

#[test]
fn matrix_destructive_bash() {
    let args = serde_json::json!({"command": "rm -rf target/"});
    let (auto, confirm) = check_both("Bash", &args);
    // Auto: sandbox enforces perimeter, destructive ops auto-approved
    // (downgrades to NeedsConfirmation if sandbox unavailable, #860)
    assert_eq!(auto, auto_mutation_expected());
    assert_eq!(confirm, ToolApproval::NeedsConfirmation);
}

// ── Row 9: Bash with path escape ──

#[test]
fn matrix_bash_path_escape() {
    // /etc is still blocked; /tmp is now allowed (#560)
    let args = serde_json::json!({"command": "cd /etc && ls"});
    let (auto, confirm) = check_both("Bash", &args);
    assert_eq!(auto, ToolApproval::NeedsConfirmation);
    assert_eq!(confirm, ToolApproval::NeedsConfirmation);
}

// ── Row 10: Sub-agent invocation ──

#[test]
fn matrix_invoke_agent() {
    let args = serde_json::json!({"agent_name": "reviewer", "prompt": "review"});
    let (auto, confirm) = check_both("InvokeAgent", &args);
    assert_eq!(auto, ToolApproval::AutoApprove);
    assert_eq!(confirm, ToolApproval::AutoApprove);
}

// ── Row 11: MemoryWrite ──

#[test]
fn matrix_memory_write() {
    let args = serde_json::json!({"content": "remember this"});
    let (auto, confirm) = check_both("MemoryWrite", &args);
    assert_eq!(auto, auto_mutation_expected());
    assert_eq!(confirm, ToolApproval::NeedsConfirmation);
}

// ── Row 12: WebFetch (GET) ──

#[test]
fn matrix_web_fetch() {
    let args = serde_json::json!({"url": "https://example.com"});
    let (auto, confirm) = check_both("WebFetch", &args);
    assert_eq!(auto, ToolApproval::AutoApprove);
    assert_eq!(confirm, ToolApproval::AutoApprove);
}

// ── Row 13: gh issue create (LocalMutation bash) ──

#[test]
fn matrix_gh_issue_create() {
    let args = serde_json::json!({"command": "gh issue create --title 'bug'"});
    let (auto, confirm) = check_both("Bash", &args);
    assert_eq!(auto, auto_mutation_expected());
    assert_eq!(confirm, ToolApproval::NeedsConfirmation);
}
