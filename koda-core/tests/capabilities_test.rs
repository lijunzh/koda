//! Verify that capabilities.md stays in sync with actual commands and features.

const CAPABILITIES_MD: &str = include_str!("../src/capabilities.md");

/// Every slash command that exists in the REPL must be mentioned in capabilities.md.
const EXPECTED_COMMANDS: &[&str] = &[
    "/agent",
    "/compact",
    "/cost",
    "/diff",
    "/exit",
    "/expand",
    "/mcp",
    "/memory",
    "/model",
    "/provider",
    "/sessions",
    "/skills",
    "/undo",
    "/verbose",
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

/// Key features that must be mentioned to keep the model's self-knowledge accurate.
#[test]
fn test_capabilities_mentions_key_features() {
    let must_mention = [
        "MCP",
        "Memory",
        "@file",
        ".mcp.json",
        "MEMORY.md",
        "CLAUDE.md",
        "auto",
        "strict",
        "safe",
        "Shift+Tab",
        "--skip-probe",
        "koda-ast",
        "koda-email",
        "Skills",
        "ActivateSkill",
        "ListSkills",
    ];
    for feature in must_mention {
        assert!(
            CAPABILITIES_MD.contains(feature),
            "Feature '{feature}' is missing from capabilities.md"
        );
    }
}

/// Verify the user guide covers the same commands as capabilities.md.
#[test]
fn test_user_guide_covers_slash_commands() {
    let guide = include_str!("../../docs/user-guide.md");
    for cmd in EXPECTED_COMMANDS {
        assert!(
            guide.contains(cmd),
            "Command '{cmd}' is missing from docs/user-guide.md"
        );
    }
}

/// Verify the user guide covers key workflow sections.
#[test]
fn test_user_guide_covers_key_sections() {
    let guide = include_str!("../../docs/user-guide.md");
    let required_sections = [
        "Approval Modes",
        "Slash Commands",
        "File References",
        "Memory System",
        "Agents",
        "MCP Servers",
        "Git Checkpointing",
        "Headless Mode",
        "Security Model",
    ];
    for section in required_sections {
        assert!(
            guide.contains(section),
            "Section '{section}' is missing from docs/user-guide.md"
        );
    }
}
