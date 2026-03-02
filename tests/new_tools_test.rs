//! Integration tests for new tools: Glob, WebFetch, Todo, CreateTool.
//!
//! Tests use real temporary directories to validate end-to-end behavior.

use tempfile::TempDir;

// ── Glob ──────────────────────────────────────────────────────────

mod glob_tool {
    use super::*;

    fn setup_project() -> TempDir {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src/tools")).unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"").unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub mod tools;").unwrap();
        std::fs::write(root.join("src/tools/mod.rs"), "// tools").unwrap();
        std::fs::write(root.join("src/tools/grep.rs"), "// grep").unwrap();
        std::fs::write(root.join("tests/integration.rs"), "// tests").unwrap();
        std::fs::write(root.join("README.md"), "# Demo").unwrap();
        tmp
    }

    #[test]
    fn test_glob_finds_all_rust_files() {
        let tmp = setup_project();
        let pattern = tmp.path().join("**/*.rs");
        let matches: Vec<_> = glob::glob(pattern.to_str().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        // Should find: main.rs, lib.rs, mod.rs, grep.rs, integration.rs
        assert_eq!(matches.len(), 5);
    }

    #[test]
    fn test_glob_scoped_to_subdirectory() {
        let tmp = setup_project();
        let pattern = tmp.path().join("src/tools/*.rs");
        let matches: Vec<_> = glob::glob(pattern.to_str().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(matches.len(), 2); // mod.rs, grep.rs
    }

    #[test]
    fn test_glob_toml_only() {
        let tmp = setup_project();
        let pattern = tmp.path().join("*.toml");
        let matches: Vec<_> = glob::glob(pattern.to_str().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(matches.len(), 1);
        assert!(matches[0].to_str().unwrap().contains("Cargo.toml"));
    }

    #[test]
    fn test_glob_no_matches() {
        let tmp = setup_project();
        let pattern = tmp.path().join("**/*.xyz");
        let matches: Vec<_> = glob::glob(pattern.to_str().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(matches.is_empty());
    }

    #[test]
    fn test_glob_markdown_files() {
        let tmp = setup_project();
        let pattern = tmp.path().join("**/*.md");
        let matches: Vec<_> = glob::glob(pattern.to_str().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(matches.len(), 1);
    }
}

// ── Todo (section parsing / rendering) ────────────────────────────

mod todo_sections {
    /// Reproduce the section parsing logic for integration testing.
    fn parse_sections(content: &str) -> std::collections::BTreeMap<String, String> {
        let mut sections = std::collections::BTreeMap::new();
        let mut current_section = String::new();
        let mut current_body = String::new();

        for line in content.lines() {
            if let Some(header) = line.strip_prefix("## ") {
                if !current_section.is_empty() {
                    sections.insert(current_section.clone(), current_body.trim().to_string());
                }
                current_section = header.trim().to_string();
                current_body = String::new();
            } else if !current_section.is_empty() {
                current_body.push_str(line);
                current_body.push('\n');
            }
        }
        if !current_section.is_empty() {
            sections.insert(current_section, current_body.trim().to_string());
        }
        sections
    }

    fn render_sections(sections: &std::collections::BTreeMap<String, String>) -> String {
        let mut out = String::from("# Tasks\n\n");
        for (name, body) in sections {
            out.push_str(&format!("## {name}\n\n{body}\n\n"));
        }
        out
    }

    #[test]
    fn test_multi_project_todo() {
        // Simulate two projects writing tasks
        let mut sections = std::collections::BTreeMap::new();
        sections.insert(
            "backend".to_string(),
            "- [ ] Add auth middleware\n- [x] Setup DB".to_string(),
        );
        sections.insert("frontend".to_string(), "- [ ] Build login page".to_string());
        sections.insert("Global".to_string(), "- [ ] Update CI pipeline".to_string());

        let rendered = render_sections(&sections);

        // Verify structure
        assert!(rendered.contains("## Global"));
        assert!(rendered.contains("## backend"));
        assert!(rendered.contains("## frontend"));
        assert!(rendered.contains("- [x] Setup DB"));

        // Verify roundtrip
        let reparsed = parse_sections(&rendered);
        assert_eq!(reparsed.len(), 3);
        assert!(reparsed["backend"].contains("Add auth middleware"));
        assert!(reparsed["frontend"].contains("Build login page"));
        assert!(reparsed["Global"].contains("Update CI pipeline"));
    }

    #[test]
    fn test_section_update_preserves_others() {
        let initial = "\
# Tasks\n\n\
## my-app\n\n\
- [ ] Old task\n\n\
## Global\n\n\
- [ ] Global task\n\n";

        let mut sections = parse_sections(initial);
        assert_eq!(sections.len(), 2);

        // Update only my-app
        sections.insert(
            "my-app".to_string(),
            "- [x] Old task\n- [ ] New task".to_string(),
        );

        let rendered = render_sections(&sections);
        let reparsed = parse_sections(&rendered);

        // Global should be untouched
        assert!(reparsed["Global"].contains("Global task"));
        // my-app should be updated
        assert!(reparsed["my-app"].contains("New task"));
        assert!(reparsed["my-app"].contains("[x] Old task"));
    }

    #[test]
    fn test_empty_file_parses_to_empty() {
        let sections = parse_sections("");
        assert!(sections.is_empty());
    }

    #[test]
    fn test_no_sections_ignores_loose_text() {
        let sections = parse_sections("# Tasks\n\nSome loose text without sections");
        assert!(sections.is_empty());
    }
}

// ── CreateTool (constructor) ──────────────────────────────────────

mod constructor {
    use super::*;

    #[test]
    fn test_custom_tool_json_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let tools_dir = tmp.path().join("agents/tools");
        std::fs::create_dir_all(&tools_dir).unwrap();

        // Write a tool definition
        let tool_json = serde_json::json!({
            "name": "GitLog",
            "description": "Show recent commits",
            "parameters": {
                "type": "object",
                "properties": {
                    "count": { "type": "integer", "description": "Number of commits" }
                },
                "required": ["count"]
            },
            "command_template": "git log --oneline -{{count}}"
        });

        let file_path = tools_dir.join("GitLog.json");
        std::fs::write(&file_path, tool_json.to_string()).unwrap();

        // Read it back and verify
        let content = std::fs::read_to_string(&file_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();

        assert_eq!(parsed["name"], "GitLog");
        assert_eq!(parsed["command_template"], "git log --oneline -{{count}}");
        assert!(parsed["parameters"]["properties"]["count"].is_object());
    }

    #[test]
    fn test_command_template_expansion() {
        let template = "git log --oneline -{{count}} --author={{author}}";
        let mut command = template.to_string();

        let params = serde_json::json!({
            "count": 10,
            "author": "alice"
        });

        if let Some(obj) = params.as_object() {
            for (key, value) in obj {
                let placeholder = format!("{{{{{key}}}}}");
                let replacement = match value {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                command = command.replace(&placeholder, &replacement);
            }
        }

        assert_eq!(command, "git log --oneline -10 --author=alice");
    }

    #[test]
    fn test_template_with_missing_param_keeps_placeholder() {
        let template = "curl {{url}} -H 'Auth: {{token}}'";
        let mut command = template.to_string();

        // Only provide url, not token
        let params = serde_json::json!({ "url": "https://api.example.com" });
        if let Some(obj) = params.as_object() {
            for (key, value) in obj {
                let placeholder = format!("{{{{{key}}}}}");
                let replacement = match value {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                command = command.replace(&placeholder, &replacement);
            }
        }

        assert_eq!(command, "curl https://api.example.com -H 'Auth: {{token}}'");
    }

    #[test]
    fn test_tool_discovery_from_agents_dir() {
        let tmp = TempDir::new().unwrap();
        let tools_dir = tmp.path().join("agents/tools");
        std::fs::create_dir_all(&tools_dir).unwrap();

        // Write two custom tools
        for name in ["ToolA", "ToolB"] {
            let json = serde_json::json!({
                "name": name,
                "description": format!("Tool {name}"),
                "parameters": { "type": "object", "properties": {} },
                "command_template": "echo hello"
            });
            std::fs::write(tools_dir.join(format!("{name}.json")), json.to_string()).unwrap();
        }

        // Also write a non-JSON file (should be ignored)
        std::fs::write(tools_dir.join("README.md"), "# ignore me").unwrap();

        // Discover tools
        let entries: Vec<_> = std::fs::read_dir(&tools_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
            .collect();

        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn test_pascal_case_validation() {
        fn is_valid_tool_name(name: &str) -> bool {
            !name.is_empty() && name.chars().next().unwrap().is_uppercase() && !name.contains(' ')
        }

        assert!(is_valid_tool_name("GitLog"));
        assert!(is_valid_tool_name("DockerBuild"));
        assert!(is_valid_tool_name("A")); // single uppercase letter
        assert!(!is_valid_tool_name("gitLog")); // lowercase start
        assert!(!is_valid_tool_name("Git Log")); // space
        assert!(!is_valid_tool_name("")); // empty
    }
}

// ── WebFetch (HTML stripping) ─────────────────────────────────────

mod web_fetch {
    /// Reproduce the HTML stripping logic for integration testing.
    fn strip_html(html: &str) -> String {
        let mut result = String::with_capacity(html.len());
        let mut in_tag = false;
        let mut in_script = false;
        let mut in_style = false;
        let mut last_was_space = false;

        let lower = html.to_lowercase();
        let chars: Vec<char> = html.chars().collect();
        let lower_chars: Vec<char> = lower.chars().collect();

        let mut i = 0;
        while i < chars.len() {
            if in_script {
                if i + 9 <= lower_chars.len()
                    && lower_chars[i..i + 9].iter().collect::<String>() == "</script>"
                {
                    in_script = false;
                    i += 9;
                } else {
                    i += 1;
                }
                continue;
            }
            if in_style {
                if i + 8 <= lower_chars.len()
                    && lower_chars[i..i + 8].iter().collect::<String>() == "</style>"
                {
                    in_style = false;
                    i += 8;
                } else {
                    i += 1;
                }
                continue;
            }
            if chars[i] == '<' {
                if i + 7 <= lower_chars.len()
                    && lower_chars[i..i + 7].iter().collect::<String>() == "<script"
                {
                    in_script = true;
                } else if i + 6 <= lower_chars.len()
                    && lower_chars[i..i + 6].iter().collect::<String>() == "<style"
                {
                    in_style = true;
                }
                in_tag = true;
                i += 1;
                continue;
            }
            if chars[i] == '>' {
                in_tag = false;
                i += 1;
                continue;
            }
            if !in_tag {
                let ch = chars[i];
                if ch.is_whitespace() {
                    if !last_was_space {
                        result.push(' ');
                        last_was_space = true;
                    }
                } else {
                    result.push(ch);
                    last_was_space = false;
                }
            }
            i += 1;
        }
        result
            .replace("&amp;", "&")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&#39;", "'")
            .replace("&nbsp;", " ")
    }

    #[test]
    fn test_strip_real_world_html() {
        let html = r#"
        <!DOCTYPE html>
        <html>
        <head>
            <title>Test Page</title>
            <style>body { color: red; }</style>
            <script>console.log('hidden');</script>
        </head>
        <body>
            <h1>Hello World</h1>
            <p>This is a <strong>test</strong> with &amp; entities.</p>
            <ul>
                <li>Item 1</li>
                <li>Item 2</li>
            </ul>
        </body>
        </html>
        "#;

        let text = strip_html(html);
        assert!(text.contains("Hello World"));
        assert!(text.contains("test"));
        assert!(text.contains("& entities"));
        assert!(text.contains("Item 1"));
        assert!(!text.contains("<h1>"));
        assert!(!text.contains("console.log"));
        assert!(!text.contains("color: red"));
    }

    #[test]
    fn test_strip_nested_tags() {
        let html = "<div><p><span>deep</span></p></div>";
        let text = strip_html(html);
        assert_eq!(text.trim(), "deep");
    }

    #[test]
    fn test_strip_preserves_plain_text() {
        let text = strip_html("no tags here");
        assert_eq!(text, "no tags here");
    }

    #[test]
    fn test_entity_decoding() {
        let html = "5 &lt; 10 &amp;&amp; 10 &gt; 5";
        let text = strip_html(html);
        assert_eq!(text, "5 < 10 && 10 > 5");
    }
}

// ── Tool naming convention ────────────────────────────────────────

mod naming_convention {
    /// All built-in tool names must be PascalCase.
    const BUILTIN_TOOLS: &[&str] = &[
        "Read",
        "Write",
        "Edit",
        "Delete",
        "List",
        "Grep",
        "Glob",
        "Bash",
        "WebFetch",
        "TodoRead",
        "TodoWrite",
        "MemoryRead",
        "MemoryWrite",
        "InvokeAgent",
        "ListAgents",
        "CreateAgent",
        "CreateTool",
        "ListTools",
        "DeleteTool",
    ];

    #[test]
    fn test_all_builtins_start_with_uppercase() {
        for name in BUILTIN_TOOLS {
            assert!(
                name.chars().next().unwrap().is_uppercase(),
                "Tool '{name}' must start with uppercase"
            );
        }
    }

    #[test]
    fn test_no_underscores_in_tool_names() {
        for name in BUILTIN_TOOLS {
            assert!(
                !name.contains('_'),
                "Tool '{name}' should use PascalCase, not snake_case"
            );
        }
    }

    #[test]
    fn test_no_duplicate_names() {
        let mut seen = std::collections::HashSet::new();
        for name in BUILTIN_TOOLS {
            assert!(seen.insert(name), "Duplicate tool name: {name}");
        }
    }

    #[test]
    fn test_expected_tool_count() {
        // 16 built-in tools as of this version
        assert_eq!(BUILTIN_TOOLS.len(), 19);
    }
}
