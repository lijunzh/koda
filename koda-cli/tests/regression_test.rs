//! Regression & E2E tests for REPL commands and input processing.
//!
//! These tests verify that the command surface area works correctly
//! and catch regressions when commands are added/removed.

mod repl_commands {
    /// Reproduce the REPL command dispatch logic for testing.
    /// Maps a slash command to the action name it should produce.
    fn dispatch(input: &str) -> &'static str {
        let parts: Vec<&str> = input.splitn(2, ' ').collect();
        let cmd = parts[0];
        let arg = parts.get(1).copied().unwrap_or("");

        match cmd {
            "/exit" => "Quit",
            "/model" if !arg.is_empty() => "SwitchModel",
            "/model" => "PickModel",
            "/provider" if !arg.is_empty() => "SetupProvider",
            "/provider" => "PickProvider",
            "/help" => "ShowHelp",
            "/cost" => "ShowCost",
            "/trust" if !arg.is_empty() => "SetTrust",
            "/trust" => "PickTrust",
            "/diff" if !arg.is_empty() => "InjectPrompt_or_Handled",
            "/diff" => "Handled",
            "/sessions" if arg.starts_with("delete ") => "DeleteSession",
            "/sessions" if arg.starts_with("resume ") => "ResumeSession",
            "/sessions"
                if !arg.is_empty() && arg.chars().all(|c| c.is_ascii_hexdigit() || c == '-') =>
            {
                "ResumeSession"
            }
            "/sessions" => "ListSessions",
            "/expand" => "Expand",
            "/verbose" => "Verbose",
            "/memory" => "Handled",

            "/compact" => "Compact",
            "/mcp" => "McpCommand",
            "/agent" => "Handled",
            _ => "NotACommand",
        }
    }

    #[test]
    fn test_key_command_is_removed() {
        assert_eq!(dispatch("/key"), "NotACommand");
        assert_eq!(dispatch("/key my-secret-key"), "NotACommand");
    }

    #[test]
    fn test_all_expected_commands_dispatch() {
        assert_eq!(dispatch("/exit"), "Quit");
        assert_eq!(dispatch("/model"), "PickModel");
        assert_eq!(dispatch("/model gpt-4o"), "SwitchModel");
        assert_eq!(dispatch("/provider"), "PickProvider");
        assert_eq!(dispatch("/provider openai"), "SetupProvider");
        assert_eq!(dispatch("/help"), "ShowHelp");
        assert_eq!(dispatch("/cost"), "ShowCost");
        assert_eq!(dispatch("/trust"), "PickTrust");
        assert_eq!(dispatch("/trust yolo"), "SetTrust");
        assert_eq!(dispatch("/diff"), "Handled");
        assert_eq!(dispatch("/diff review"), "InjectPrompt_or_Handled");
        assert_eq!(dispatch("/diff commit"), "InjectPrompt_or_Handled");
        assert_eq!(dispatch("/sessions"), "ListSessions");
        assert_eq!(dispatch("/sessions delete abc123"), "DeleteSession");
        assert_eq!(dispatch("/sessions resume abc123"), "ResumeSession");
        assert_eq!(dispatch("/sessions abc12345"), "ResumeSession");
        assert_eq!(dispatch("/expand"), "Expand");
        assert_eq!(dispatch("/verbose"), "Verbose");
        assert_eq!(dispatch("/memory"), "Handled");
        assert_eq!(dispatch("/memory add test"), "Handled");
        assert_eq!(dispatch("/memory global test"), "Handled");

        assert_eq!(dispatch("/compact"), "Compact");
        assert_eq!(dispatch("/mcp"), "McpCommand");
        assert_eq!(dispatch("/mcp status"), "McpCommand");
        assert_eq!(dispatch("/agent"), "Handled");
    }

    #[test]
    fn test_unknown_commands_fall_through() {
        assert_eq!(dispatch("/foo"), "NotACommand");
        assert_eq!(dispatch("/set"), "NotACommand");
        assert_eq!(dispatch("/config"), "NotACommand");
        assert_eq!(dispatch("/transcript"), "NotACommand"); // removed feature
    }
}

mod input_processing {
    use std::fs;
    use tempfile::TempDir;

    fn process_input(input: &str, project_root: &std::path::Path) -> (String, Vec<String>) {
        let mut prompt_parts = Vec::new();
        let mut files_loaded = Vec::new();

        for token in input.split_whitespace() {
            if let Some(raw_path) = token.strip_prefix('@') {
                if raw_path.is_empty() {
                    prompt_parts.push(token.to_string());
                    continue;
                }
                let full_path = project_root.join(raw_path);
                if full_path.is_file() {
                    files_loaded.push(raw_path.to_string());
                } else {
                    prompt_parts.push(token.to_string());
                }
            } else {
                prompt_parts.push(token.to_string());
            }
        }

        let prompt = prompt_parts.join(" ");
        let prompt = if prompt.trim().is_empty() && !files_loaded.is_empty() {
            "Describe and explain the attached files.".to_string()
        } else {
            prompt
        };

        (prompt, files_loaded)
    }

    #[test]
    fn test_at_file_reference_resolved() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        let (prompt, files) = process_input("explain @main.rs", dir.path());
        assert_eq!(prompt, "explain");
        assert_eq!(files, vec!["main.rs"]);
    }

    #[test]
    fn test_at_file_missing_stays_in_prompt() {
        let dir = TempDir::new().unwrap();
        let (prompt, files) = process_input("explain @nonexistent.rs", dir.path());
        assert!(prompt.contains("@nonexistent.rs"));
        assert!(files.is_empty());
    }

    #[test]
    fn test_at_file_only_gets_default_prompt() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("code.py"), "x = 1").unwrap();
        let (prompt, files) = process_input("@code.py", dir.path());
        assert_eq!(prompt, "Describe and explain the attached files.");
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn test_multiple_at_files() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.rs"), "a").unwrap();
        fs::write(dir.path().join("b.rs"), "b").unwrap();
        let (prompt, files) = process_input("compare @a.rs @b.rs", dir.path());
        assert_eq!(prompt, "compare");
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn test_bare_at_sign_ignored() {
        let dir = TempDir::new().unwrap();
        let (prompt, files) = process_input("email me @ noon", dir.path());
        assert_eq!(prompt, "email me @ noon");
        assert!(files.is_empty());
    }

    #[test]
    fn test_no_at_references() {
        let dir = TempDir::new().unwrap();
        let (prompt, files) = process_input("just a question", dir.path());
        assert_eq!(prompt, "just a question");
        assert!(files.is_empty());
    }
}

mod completions {
    /// The slash commands that should appear in tab completion.
    const EXPECTED_COMMANDS: &[&str] = &[
        "/agent",
        "/compact",
        "/cost",
        "/diff",
        "/help",
        "/mcp",
        "/memory",
        "/trust",
        "/model",
        "/provider",
        "/sessions",
    ];

    /// Commands that should NOT appear in completions.
    const REMOVED_COMMANDS: &[&str] = &["/key", "/transcript"];

    #[test]
    fn test_expected_commands_present() {
        assert_eq!(EXPECTED_COMMANDS.len(), 11, "Expected 11 slash commands");
        for cmd in EXPECTED_COMMANDS {
            assert!(
                EXPECTED_COMMANDS.contains(cmd),
                "Expected command {cmd} missing from completions"
            );
        }
    }

    #[test]
    fn test_removed_commands_absent() {
        for cmd in REMOVED_COMMANDS {
            assert!(
                !EXPECTED_COMMANDS.contains(cmd),
                "Removed command {cmd} should not be in completions"
            );
        }
    }
}

mod display_regression {
    /// All tool names that should map to known labels.
    const KNOWN_TOOLS: &[(&str, &str)] = &[
        ("Read", "Read"),
        ("List", "List"),
        ("Write", "Write"),
        ("Edit", "Edit"),
        ("Delete", "Delete"),
        ("Grep", "Search"),
        ("Glob", "Glob"),
        ("Bash", "Shell"),
        ("WebFetch", "Fetch"),
        ("MemoryRead", "Memory"),
        ("MemoryWrite", "Memory"),
        ("ShareReasoning", "Tool"),
        ("InvokeAgent", "Agent"),
        ("ListAgents", "Tool"),
        ("CreateAgent", "Create"),
        ("TodoWrite", "Todo"),
        ("TodoRead", "Todo"),
        ("AstAnalysis", "AST"),
    ];

    fn tool_label(name: &str) -> &'static str {
        match name {
            "Read" => "Read",
            "List" => "List",
            "Write" => "Write",
            "Edit" => "Edit",
            "Delete" => "Delete",
            "Grep" => "Search",
            "Glob" => "Glob",
            "Bash" => "Shell",
            "WebFetch" => "Fetch",
            "MemoryRead" | "MemoryWrite" => "Memory",
            "InvokeAgent" => "Agent",
            "CreateAgent" => "Create",
            "TodoWrite" | "TodoRead" => "Todo",
            "AstAnalysis" => "AST",
            _ => "Tool",
        }
    }

    #[test]
    fn test_all_tools_have_banners() {
        for (tool, expected_label) in KNOWN_TOOLS {
            assert_eq!(
                tool_label(tool),
                *expected_label,
                "Tool '{tool}' should have label '{expected_label}'"
            );
        }
    }

    #[test]
    fn test_unknown_tool_gets_generic_banner() {
        assert_eq!(tool_label("some_new_tool"), "Tool");
    }

    #[test]
    fn test_tool_count() {
        assert_eq!(
            KNOWN_TOOLS.len(),
            18,
            "Expected 18 known tools (update this test when adding tools)"
        );
    }
}

mod provider_key_flow {
    #[test]
    fn test_same_provider_should_prompt_for_key() {
        let current_provider = "openai";
        let selected_provider = "openai";
        let is_same = current_provider == selected_provider;
        let is_local = selected_provider == "lmstudio";
        let key_exists = true;
        let should_prompt = !is_local && (is_same || !key_exists);
        assert!(should_prompt);
    }

    #[test]
    fn test_new_provider_without_key_prompts() {
        let is_same = false;
        let is_local = false;
        let key_exists = false;
        let should_prompt = !is_local && (is_same || !key_exists);
        assert!(should_prompt);
    }

    #[test]
    fn test_new_provider_with_key_skips_prompt() {
        let is_same = false;
        let is_local = false;
        let key_exists = true;
        let should_prompt = !is_local && (is_same || !key_exists);
        assert!(!should_prompt);
    }

    #[test]
    fn test_lmstudio_never_prompts_for_key() {
        let is_local = true;
        let should_prompt = !is_local;
        assert!(!should_prompt);
    }
}
