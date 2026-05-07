//! Integration tests for tool name normalization (#548).
//!
//! Verifies that lowercase and snake_case tool names are properly
//! normalized before reaching the dispatcher.

use std::path::PathBuf;

/// After normalization, every canonical tool should be routable.
/// This catches regressions where a new tool is added to the registry
/// but forgotten in the normalizer's CANONICAL list.
#[tokio::test]
async fn normalized_lowercase_names_are_routable() {
    let registry = koda_core::tools::ToolRegistry::new(PathBuf::from("/tmp/test"), 100_000);

    // These are the exact names a small model might emit (the #548 repro).
    let lowercase_names = [
        "list",
        "read",
        "write",
        "edit",
        "delete",
        "bash",
        "grep",
        "glob",
        "webfetch",
        "memoryread",
        "memorywrite",
        "listagents",
        "invokeagent",
        "listskills",
        "activateskill",
        "recallcontext",
    ];

    for raw_name in lowercase_names {
        let canonical = koda_core::tool_normalize::normalize_tool_name(raw_name);
        assert!(
            registry.has_tool(&canonical),
            "Normalized '{raw_name}' -> '{canonical}' is not in the registry"
        );
        let result = registry.execute(&canonical, "{}", None, None, &koda_core::agent::AgentPath::root()).await;
        assert!(
            !result.output.contains("Unknown tool"),
            "'{raw_name}' normalized to '{canonical}' but dispatcher rejected it: {}",
            result.output
        );
    }
}

/// Snake_case names (common in OpenAI-compatible model output) should resolve.
#[tokio::test]
async fn normalized_snake_case_names_are_routable() {
    let registry = koda_core::tools::ToolRegistry::new(PathBuf::from("/tmp/test"), 100_000);

    let snake_names = [
        ("list_files", "List"),
        ("read_file", "Read"),
        ("write_file", "Write"),
        ("edit_file", "Edit"),
        ("delete_file", "Delete"),
        ("run_shell_command", "Bash"),
        ("grep_search", "Grep"),
        ("web_fetch", "WebFetch"),
        ("list_agents", "ListAgents"),
        ("invoke_agent", "InvokeAgent"),
        ("memory_read", "MemoryRead"),
        ("memory_write", "MemoryWrite"),
        ("recall_context", "RecallContext"),
    ];

    for (raw, expected) in snake_names {
        let canonical = koda_core::tool_normalize::normalize_tool_name(raw);
        assert_eq!(
            canonical, expected,
            "'{raw}' should normalize to '{expected}', got '{canonical}'"
        );
        let result = registry.execute(&canonical, "{}", None, None, &koda_core::agent::AgentPath::root()).await;
        assert!(
            !result.output.contains("Unknown tool"),
            "'{raw}' -> '{canonical}' rejected by dispatcher: {}",
            result.output
        );
    }
}

/// The normalizer's CANONICAL list must stay in sync with the actual registry.
/// If this fails, a tool was added to the registry but not to the normalizer.
#[test]
fn normalizer_covers_all_registry_tools() {
    let registry = koda_core::tools::ToolRegistry::new(PathBuf::from("/tmp/test"), 100_000);
    let registry_names: std::collections::HashSet<String> =
        registry.all_builtin_tool_names().into_iter().collect();

    for name in &registry_names {
        let roundtripped = koda_core::tool_normalize::normalize_tool_name(name);
        assert_eq!(
            &roundtripped, name,
            "Registry tool '{name}' is not in normalizer CANONICAL list"
        );

        // Lowercase variant should also resolve
        let lower = name.to_lowercase();
        let from_lower = koda_core::tool_normalize::normalize_tool_name(&lower);
        assert_eq!(
            &from_lower, name,
            "Lowercase '{lower}' should normalize to '{name}'"
        );
    }
}
