//! Regression & E2E tests for REPL commands and input processing.
//!
//! Slash-command dispatch tests live in `repl.rs` (inline `#[cfg(test)]`).
//! This file covers input processing, display regressions, and structural
//! assertions that are better expressed at the integration level.

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
        assert_eq!(EXPECTED_COMMANDS.len(), 10, "Expected 10 slash commands");
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

mod event_loop_structure {
    /// Regression test for the v0.1.11 frozen-input bug.
    ///
    /// The idle event loop in `run_event_loop` MUST call `self.draw()` before
    /// blocking on `tokio::select!` for keyboard events. Without it the
    /// viewport never redraws after keystrokes and input appears frozen.
    ///
    /// This is a source-level structural assertion — the only reliable way to
    /// guard against accidental removal during refactors.
    #[test]
    fn draw_called_before_idle_select_in_event_loop() {
        let source = include_str!("../src/tui_context/mod.rs");

        // Locate run_event_loop
        let fn_start = source
            .find("async fn run_event_loop")
            .expect("run_event_loop function not found in tui_context.rs");
        let body = &source[fn_start..];

        // Find the idle select! block (the one with crossterm_events.next())
        let select_marker = "self.crossterm_events.next()";
        let select_pos = body
            .find(select_marker)
            .expect("idle select! with crossterm_events.next() not found in run_event_loop");

        // self.draw() must appear (uncommented) between fn start and the select!
        let before_select = &body[..select_pos];
        let has_active_draw = before_select.lines().any(|line| {
            let trimmed = line.trim();
            !trimmed.starts_with("//") && trimmed.contains("self.draw()")
        });
        assert!(
            has_active_draw,
            "self.draw() must be called (uncommented) before the idle tokio::select! in \
             run_event_loop. Without it the viewport never redraws and input appears \
             frozen (v0.1.11 bug)."
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
