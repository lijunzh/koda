//! Verify every built-in tool is properly wired through all layers.
//!
//! If you add a new tool, these tests will fail until you wire it
//! through the dispatcher and approval system.

use std::path::PathBuf;

/// Get all built-in tool names from the registry.
fn all_tool_names() -> Vec<String> {
    let registry = koda_core::tools::ToolRegistry::new(PathBuf::from("/tmp/test"), 100_000);
    registry.all_builtin_tool_names()
}

/// Every tool must be routable in the dispatcher.
/// Tools handled externally (InvokeAgent) return sentinel strings.
/// Auto-provisioned tools (from capability registry) return install hints
/// when the server binary isn't available.
/// None should return "Unknown tool".
#[tokio::test]
async fn test_all_tools_routable_in_dispatcher() {
    let registry = koda_core::tools::ToolRegistry::new(PathBuf::from("/tmp/test"), 100_000);
    for name in all_tool_names() {
        let result = registry.execute(&name, "{}").await;
        assert!(
            !result.output.contains("Unknown tool"),
            "Tool '{name}' is not routed in the dispatcher (tools/mod.rs execute()). \
             Got: {}",
            result.output
        );
    }
}

/// Every tool must be classified in the approval system.
/// It should be either in READ_ONLY_TOOLS (auto-approved) or
/// return NeedsConfirmation/AutoApproved — never panic or crash.
#[test]
fn test_all_tools_handled_by_approval() {
    use koda_core::approval::{ApprovalMode, ToolApproval, check_tool};

    let empty_args = serde_json::json!({});
    for name in all_tool_names() {
        // Should not panic in any mode
        let result = check_tool(&name, &empty_args, ApprovalMode::Confirm, None, None, None);
        // Verify it returns a valid variant (not a crash)
        match result {
            ToolApproval::AutoApprove | ToolApproval::NeedsConfirmation | ToolApproval::Blocked => {
            }
        }
    }
}
