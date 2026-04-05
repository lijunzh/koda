//! Verify that generated capabilities and the user guide stay in sync.
//!
//! The capabilities.md file was deleted in #674 — capabilities are now
//! generated from code in `prompt::build_system_prompt()`. These tests
//! verify the user guide still covers key commands and sections.

/// Every slash command that exists in the REPL must be mentioned in the user guide.
const EXPECTED_COMMANDS: &[&str] = &[
    "/agent",
    "/compact",
    "/diff",
    "/exit",
    "/expand",
    "/key",
    "/memory",
    "/model",
    "/provider",
    "/purge",
    "/sessions",
    "/skills",
    "/undo",
    "/verbose",
];

/// Verify the user guide covers the same commands as the REPL.
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
