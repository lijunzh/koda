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
        "ActivateSkill",
        "AstAnalysis",
        "Bash",
        "Delete",
        "Edit",
        "EmailRead",
        "EmailSearch",
        "EmailSend",
        "Glob",
        "Grep",
        "InvokeAgent",
        "List",
        "ListAgents",
        "ListSkills",
        "MemoryRead",
        "MemoryWrite",
        "Read",
        "RecallContext",
        "WebFetch",
        "Write",
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
        // 20 built-in tools as of v0.1.9+
        assert_eq!(BUILTIN_TOOLS.len(), 20);
    }

    /// Ensure BUILTIN_TOOLS stays in sync with the actual registry.
    /// If this fails, a tool was added/removed without updating BUILTIN_TOOLS above.
    #[test]
    fn test_builtin_list_matches_registry() {
        let registry =
            koda_core::tools::ToolRegistry::new(std::path::PathBuf::from("/tmp/test"), 100_000);
        let actual: std::collections::HashSet<String> =
            registry.all_builtin_tool_names().into_iter().collect();
        let expected: std::collections::HashSet<String> =
            BUILTIN_TOOLS.iter().map(|s| s.to_string()).collect();

        let missing: Vec<_> = actual.difference(&expected).collect();
        let extra: Vec<_> = expected.difference(&actual).collect();

        assert!(
            missing.is_empty() && extra.is_empty(),
            "BUILTIN_TOOLS is out of sync with the registry!\n  \
             In registry but not in list: {missing:?}\n  \
             In list but not in registry: {extra:?}"
        );
    }
}
