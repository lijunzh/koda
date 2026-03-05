//! Verify that capabilities.md stays in sync with actual commands.

const CAPABILITIES_MD: &str = include_str!("../src/capabilities.md");

const EXPECTED_COMMANDS: &[&str] = &[
    "/help",
    "/agent",
    "/compact",
    "/cost",
    "/diff",
    "/mcp",
    "/memory",
    "/model",
    "/provider",
    "/sessions",
    "/trust",
    "/exit",
];

#[test]
fn test_all_commands_documented_in_capabilities() {
    for cmd in EXPECTED_COMMANDS {
        assert!(
            CAPABILITIES_MD.contains(cmd),
            "Command '{cmd}' is missing from capabilities.md"
        );
    }
}

#[test]
fn test_capabilities_mentions_key_features() {
    let must_mention = ["MCP", "Memory", "Agents", "@file", ".mcp.json", "MEMORY.md", "TodoRead", "TodoWrite"];
    for feature in must_mention {
        assert!(
            CAPABILITIES_MD.contains(feature),
            "Feature '{feature}' is missing from capabilities.md"
        );
    }
}
