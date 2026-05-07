//! Verify every built-in tool is properly wired through all layers.
//!
//! If you add a new tool, these tests will fail until you wire it
//! through the dispatcher and approval system.

use std::path::PathBuf;

/// Get all built-in tool names from the registry.
///
/// Migrated to `ToolCatalog` in #1265 item 5 PR-2 — we only need
/// names, not the full registry. Avoids constructing FS / undo /
/// caps state we'd never touch.
fn all_tool_names() -> Vec<String> {
    koda_core::tools::ToolCatalog::new().all_builtin_tool_names()
}

/// Every tool must be routable in the dispatcher.
/// Tools handled externally (InvokeAgent) return sentinel strings.
/// None should return "Unknown tool".
///
/// Exception (#996 Layer 2): the background-task management tools
/// (`ListBackgroundTasks`, `CancelTask`, `WaitTask`) are intentionally
/// not routed through `ToolRegistry::execute()` — they need an
/// `Arc<BgAgentRegistry>` plus the caller's spawner identity, neither
/// of which the `ToolRegistry` holds. They're routed one layer up in
/// `tool_dispatch::execute_one_tool` (see the early `if matches!(...)
/// branch there). Skipping them here keeps the test honest about what
/// `ToolRegistry::execute()` is responsible for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_all_tools_routable_in_dispatcher() {
    const HIGHER_LAYER_DISPATCH: &[&str] = &["ListBackgroundTasks", "CancelTask", "WaitTask"];
    let registry = koda_core::tools::ToolRegistry::new(PathBuf::from("/tmp/test"), 100_000);
    for name in all_tool_names() {
        if HIGHER_LAYER_DISPATCH.contains(&name.as_str()) {
            continue;
        }
        let result = registry
            .execute(
                &name,
                "{}",
                None,
                None,
                &koda_core::agent::AgentPath::root(),
            )
            .await;
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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_empty_args_default_to_empty_object() {
    let registry = koda_core::tools::ToolRegistry::new(PathBuf::from("/tmp/test"), 100_000);
    for input in ["", "  ", "\n", "\t "] {
        let result = registry
            .execute(
                "List",
                input,
                None,
                None,
                &koda_core::agent::AgentPath::root(),
            )
            .await;
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
    use koda_core::tools::{ToolCatalog, ToolEffect};

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
        ("WebFetch", ToolEffect::ReadOnly),
        ("WebSearch", ToolEffect::ReadOnly),
        ("InvokeAgent", ToolEffect::ReadOnly),
        // TodoWrite mutates Koda-owned session state (the in-memory todo
        // list), not the user's filesystem. From a user-impact view it's
        // ReadOnly; classifying it as a mutation broke Plan-mode planning
        // entirely (#1212).
        ("TodoWrite", ToolEffect::ReadOnly),
        // Background-task management (#996 Layer 2). All three are
        // ReadOnly: List is a pure read; Cancel/Wait signal but don't
        // write files — idempotent control of work the model already
        // started. See `bg_task_tools::{ListBackgroundTasksTool,
        // CancelTaskTool, WaitTaskTool}` (PR-9 placeholder cohort).
        ("ListBackgroundTasks", ToolEffect::ReadOnly),
        ("CancelTask", ToolEffect::ReadOnly),
        ("WaitTask", ToolEffect::ReadOnly),
        // Peer-messaging tools (#1325 Phase 3). WaitForMail is
        // ReadOnly: it observes the mailbox sequence counter
        // without mutating state. SendMessage is LocalMutation:
        // it mutates the recipient's mailbox state. See module
        // docs on each for the full rationale.
        ("WaitForMail", ToolEffect::ReadOnly),
        // Local mutations
        ("Write", ToolEffect::LocalMutation),
        ("Edit", ToolEffect::LocalMutation),
        ("MemoryWrite", ToolEffect::LocalMutation),
        ("SendMessage", ToolEffect::LocalMutation),
        // Bash with no args is the *defensive default* under per-call
        // classification (#1265 PR-6). Real call sites always pass a
        // command, which `BashTool::classify` then routes through
        // `bash_safety::classify_bash_command` for ReadOnly /
        // LocalMutation / Destructive. Strictly more conservative
        // than the pre-#1265 `LocalMutation` name-only default.
        ("Bash", ToolEffect::Destructive),
        // Destructive
        ("Delete", ToolEffect::Destructive),
    ]
    .into_iter()
    .collect();

    let registered = all_tool_names();
    let catalog = ToolCatalog::default_static();
    let null = serde_json::Value::Null;

    // Every registered tool must appear in our expected map.
    for name in &registered {
        assert!(
            expected.contains_key(name.as_str()),
            "Tool '{name}' is registered but missing from the classify sync test. \
             Add it to the `expected` map in test_classify_tool_covers_all_tools_explicitly()."
        );
    }

    // Every expected tool must return the documented name-only
    // classification (i.e. with `Value::Null` args). Bash falls into
    // its defensive default here — see the args-aware tests on
    // `BashTool` for the per-call behavior.
    for (name, effect) in &expected {
        assert_eq!(
            catalog.classify_call(name, &null),
            *effect,
            "catalog.classify_call(\"{name}\", Null) returned wrong effect. \
             Update either the tool's `Tool::classify` impl or the expected map."
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
    use koda_core::tools::{ToolCatalog, ToolEffect, describe_action};

    let empty_args = serde_json::json!({});
    let catalog = ToolCatalog::default_static();

    for name in all_tool_names() {
        if matches!(
            catalog.classify_call(&name, &serde_json::Value::Null),
            ToolEffect::ReadOnly
        ) {
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
