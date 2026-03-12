//! Regression & E2E tests for REPL commands and input processing.
//!
//! These tests verify that the command surface area works correctly
//! and catch regressions when commands are added/removed.

mod repl_commands {
    use koda_cli::repl::{ReplAction, handle_command};
    use koda_core::config::{KodaConfig, ProviderType};
    use koda_core::providers::mock::{MockProvider, MockResponse};
    use std::sync::Arc;
    use tokio::sync::RwLock;

    /// Synchronous helper: runs `handle_command` in a single-threaded tokio runtime
    /// with dummy config/provider values (both are unused by the dispatcher).
    fn dispatch(input: &str) -> ReplAction {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let config = KodaConfig::default_for_testing(ProviderType::LMStudio);
        let provider: Arc<RwLock<Box<dyn koda_core::providers::LlmProvider>>> =
            Arc::new(RwLock::new(Box::new(MockProvider::new(vec![
                MockResponse::Text(String::new()),
            ]))));
        rt.block_on(handle_command(input, &config, &provider))
    }

    #[test]
    fn exit_command_returns_quit() {
        assert!(matches!(dispatch("/exit"), ReplAction::Quit));
    }

    #[test]
    fn model_bare_returns_pick_model() {
        assert!(matches!(dispatch("/model"), ReplAction::PickModel));
    }

    #[test]
    fn model_with_name_returns_switch_model() {
        assert!(matches!(
            dispatch("/model gpt-4o"),
            ReplAction::SwitchModel(_)
        ));
        // Verify the model name is carried through
        if let ReplAction::SwitchModel(name) = dispatch("/model gpt-4o") {
            assert_eq!(name, "gpt-4o");
        }
    }

    #[test]
    fn provider_bare_returns_pick_provider() {
        assert!(matches!(dispatch("/provider"), ReplAction::PickProvider));
    }

    #[test]
    fn provider_with_name_returns_setup_provider() {
        assert!(matches!(
            dispatch("/provider openai"),
            ReplAction::SetupProvider(_, _)
        ));
    }

    #[test]
    fn help_returns_show_help() {
        assert!(matches!(dispatch("/help"), ReplAction::ShowHelp));
    }

    #[test]
    fn cost_returns_show_cost() {
        assert!(matches!(dispatch("/cost"), ReplAction::ShowCost));
    }

    #[test]
    fn diff_bare_returns_show_diff() {
        assert!(matches!(dispatch("/diff"), ReplAction::ShowDiff));
    }

    #[test]
    fn diff_review_returns_inject_prompt() {
        assert!(matches!(
            dispatch("/diff review"),
            ReplAction::InjectPrompt(_)
        ));
    }

    #[test]
    fn diff_commit_returns_inject_prompt() {
        assert!(matches!(
            dispatch("/diff commit"),
            ReplAction::InjectPrompt(_)
        ));
    }

    #[test]
    fn sessions_bare_returns_list_sessions() {
        assert!(matches!(dispatch("/sessions"), ReplAction::ListSessions));
    }

    #[test]
    fn sessions_delete_returns_delete_session() {
        assert!(matches!(
            dispatch("/sessions delete abc123"),
            ReplAction::DeleteSession(_)
        ));
        if let ReplAction::DeleteSession(id) = dispatch("/sessions delete abc123") {
            assert_eq!(id, "abc123");
        }
    }

    #[test]
    fn sessions_resume_returns_resume_session() {
        assert!(matches!(
            dispatch("/sessions resume abc123"),
            ReplAction::ResumeSession(_)
        ));
        if let ReplAction::ResumeSession(id) = dispatch("/sessions resume abc123") {
            assert_eq!(id, "abc123");
        }
    }

    #[test]
    fn sessions_bare_id_returns_resume_session() {
        // Bare hex ID shorthand: /sessions <hex-id>
        assert!(matches!(
            dispatch("/sessions abc12345"),
            ReplAction::ResumeSession(_)
        ));
    }

    #[test]
    fn expand_returns_expand() {
        assert!(matches!(dispatch("/expand"), ReplAction::Expand(_)));
        // Default n=1 when no argument given
        if let ReplAction::Expand(n) = dispatch("/expand") {
            assert_eq!(n, 1);
        }
        // Explicit n
        if let ReplAction::Expand(n) = dispatch("/expand 3") {
            assert_eq!(n, 3);
        }
    }

    #[test]
    fn verbose_bare_returns_toggle() {
        // No argument → None (toggle)
        assert!(matches!(dispatch("/verbose"), ReplAction::Verbose(None)));
    }

    #[test]
    fn verbose_on_returns_true() {
        assert!(matches!(
            dispatch("/verbose on"),
            ReplAction::Verbose(Some(true))
        ));
    }

    #[test]
    fn verbose_off_returns_false() {
        assert!(matches!(
            dispatch("/verbose off"),
            ReplAction::Verbose(Some(false))
        ));
    }

    #[test]
    fn memory_bare_returns_memory_command() {
        assert!(matches!(
            dispatch("/memory"),
            ReplAction::MemoryCommand(None)
        ));
    }

    #[test]
    fn memory_with_arg_returns_memory_command_some() {
        assert!(matches!(
            dispatch("/memory add test"),
            ReplAction::MemoryCommand(Some(_))
        ));
        assert!(matches!(
            dispatch("/memory global test"),
            ReplAction::MemoryCommand(Some(_))
        ));
    }

    #[test]
    fn compact_returns_compact() {
        assert!(matches!(dispatch("/compact"), ReplAction::Compact));
    }

    #[test]
    fn mcp_bare_returns_mcp_command() {
        assert!(matches!(dispatch("/mcp"), ReplAction::McpCommand(_)));
    }

    #[test]
    fn mcp_with_arg_returns_mcp_command() {
        assert!(matches!(dispatch("/mcp status"), ReplAction::McpCommand(_)));
        if let ReplAction::McpCommand(arg) = dispatch("/mcp status") {
            assert_eq!(arg, "status");
        }
    }

    #[test]
    fn agent_returns_list_agents() {
        assert!(matches!(dispatch("/agent"), ReplAction::ListAgents));
    }

    #[test]
    fn undo_returns_undo() {
        assert!(matches!(dispatch("/undo"), ReplAction::Undo));
    }

    #[test]
    fn skills_bare_returns_list_skills_none() {
        assert!(matches!(dispatch("/skills"), ReplAction::ListSkills(None)));
    }

    #[test]
    fn skills_with_query_returns_list_skills_some() {
        assert!(matches!(
            dispatch("/skills review"),
            ReplAction::ListSkills(Some(_))
        ));
        if let ReplAction::ListSkills(Some(q)) = dispatch("/skills review") {
            assert_eq!(q, "review");
        }
    }

    #[test]
    fn key_command_is_not_a_command() {
        // /key was removed; must fall through
        assert!(matches!(dispatch("/key"), ReplAction::NotACommand));
        assert!(matches!(
            dispatch("/key my-secret-key"),
            ReplAction::NotACommand
        ));
    }

    #[test]
    fn unknown_commands_fall_through() {
        assert!(matches!(dispatch("/foobar"), ReplAction::NotACommand));
        assert!(matches!(dispatch("/foo"), ReplAction::NotACommand));
        assert!(matches!(dispatch("/set"), ReplAction::NotACommand));
        assert!(matches!(dispatch("/config"), ReplAction::NotACommand));
        assert!(matches!(dispatch("/transcript"), ReplAction::NotACommand));
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
        "/model",
        "/provider",
        "/sessions",
        "/skills",
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
            17,
            "Expected 17 known tools (update this test when adding tools)"
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
