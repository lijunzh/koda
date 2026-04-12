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
/// None should return "Unknown tool".
#[tokio::test]
async fn test_all_tools_routable_in_dispatcher() {
    let registry = koda_core::tools::ToolRegistry::new(PathBuf::from("/tmp/test"), 100_000);
    for name in all_tool_names() {
        let result = registry.execute(&name, "{}", None).await;
        assert!(
            !result.output.contains("Unknown tool"),
            "Tool '{name}' is not routed in the dispatcher (tools/mod.rs execute()). \
             Got: {}",
            result.output
        );
    }
}

/// Empty/whitespace-only arguments should be treated as `{}`, not error.
/// Regression test for #513.
#[tokio::test]
async fn test_empty_args_default_to_empty_object() {
    let registry = koda_core::tools::ToolRegistry::new(PathBuf::from("/tmp/test"), 100_000);
    for input in ["", "  ", "\n", "\t "] {
        let result = registry.execute("List", input, None).await;
        assert!(
            !result.output.contains("Invalid JSON"),
            "Empty args '{input:?}' should not produce a JSON parse error. Got: {}",
            result.output
        );
    }
}

/// Every tool must be classified in the approval system.
/// It should be either in READ_ONLY_TOOLS (auto-approved) or
/// return NeedsConfirmation/AutoApproved — never panic or crash.
#[test]
fn test_all_tools_handled_by_approval() {
    use koda_core::trust::{ToolApproval, TrustMode, check_tool};

    let empty_args = serde_json::json!({});
    for name in all_tool_names() {
        // Should not panic in any mode
        let result = check_tool(&name, &empty_args, TrustMode::Safe, None);
        // Verify it returns a valid variant (not a crash)
        match result {
            ToolApproval::AutoApprove | ToolApproval::NeedsConfirmation | ToolApproval::Blocked => {
            }
        }
    }
}

// ── Sync tests: verify the 3 match statements stay in sync ──────────
//
// DESIGN.md: "Match Statement, Not Trait Registry (P2)"
//
// The trade-off of match dispatch is that 3 locations must stay in sync:
//   1. classify_tool()   — tool name → ToolEffect
//   2. describe_action() — tool name → human-readable description
//   3. execute()         — tool name → handler
//
// These tests catch drift. If you add a tool, you'll get a compile error
// in execute() (missing match arm triggers "Unknown tool" in the wiring
// test above), but classify_tool and describe_action have catch-all arms
// that silently do the wrong thing. These tests catch that.

/// Every built-in tool must have an *explicit* entry in `classify_tool()`,
/// not just fall through to the `_ => LocalMutation` default.
///
/// We maintain the expected classification here. If you add a tool,
/// add it to this map — the test will fail until you do.
#[test]
fn test_classify_tool_covers_all_tools_explicitly() {
    use koda_core::tools::{ToolEffect, classify_tool};

    // Canonical expected classification for every built-in tool.
    // If you add a tool, add it here with its expected ToolEffect.
    let expected: std::collections::HashMap<&str, ToolEffect> = [
        // Pure reads
        ("Read", ToolEffect::ReadOnly),
        ("List", ToolEffect::ReadOnly),
        ("Grep", ToolEffect::ReadOnly),
        ("Glob", ToolEffect::ReadOnly),
        ("MemoryRead", ToolEffect::ReadOnly),
        ("ListAgents", ToolEffect::ReadOnly),
        ("ListSkills", ToolEffect::ReadOnly),
        ("ActivateSkill", ToolEffect::ReadOnly),
        ("RecallContext", ToolEffect::ReadOnly),
        ("AskUser", ToolEffect::ReadOnly),
        ("TodoRead", ToolEffect::ReadOnly),
        ("WebFetch", ToolEffect::ReadOnly),
        ("WebSearch", ToolEffect::ReadOnly),
        ("InvokeAgent", ToolEffect::ReadOnly),
        // Local mutations
        ("Write", ToolEffect::LocalMutation),
        ("Edit", ToolEffect::LocalMutation),
        ("Bash", ToolEffect::LocalMutation),
        ("MemoryWrite", ToolEffect::LocalMutation),
        ("TodoWrite", ToolEffect::LocalMutation),
        // Destructive
        ("Delete", ToolEffect::Destructive),
    ]
    .into_iter()
    .collect();

    let registered = all_tool_names();

    // Every registered tool must appear in our expected map.
    for name in &registered {
        assert!(
            expected.contains_key(name.as_str()),
            "Tool '{name}' is registered but missing from the classify_tool sync test. \
             Add it to the `expected` map in test_classify_tool_covers_all_tools_explicitly()."
        );
    }

    // Every expected tool must return the documented classification.
    for (name, effect) in &expected {
        assert_eq!(
            classify_tool(name),
            *effect,
            "classify_tool(\"{name}\") returned wrong effect. \
             Update either classify_tool() or the expected map."
        );
    }
}

/// Every mutating tool must have an explicit `describe_action()` entry,
/// not the generic fallback "Execute: {name}".
///
/// Read-only tools auto-approve and never show in the approval prompt,
/// so a generic description is fine for them.
#[test]
fn test_describe_action_covers_all_mutating_tools() {
    use koda_core::tools::{ToolEffect, classify_tool, describe_action};

    let empty_args = serde_json::json!({});

    for name in all_tool_names() {
        if matches!(classify_tool(&name), ToolEffect::ReadOnly) {
            continue; // read-only tools don't need custom descriptions
        }
        let desc = describe_action(&name, &empty_args);
        assert!(
            !desc.starts_with("Execute:"),
            "Mutating tool '{name}' has no explicit describe_action() entry — \
             it fell through to the generic 'Execute: {name}' fallback. \
             Add a match arm in describe_action()."
        );
    }
}
