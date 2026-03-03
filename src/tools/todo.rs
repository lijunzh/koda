//! Todo tool: session-scoped task tracker.
//!
//! The LLM writes a markdown checklist via `TodoWrite`. The current
//! todo list is injected into the system prompt every turn so the LLM
//! always knows what's done and what's next.
//!
//! Koda renders the todo with visual formatting whenever it's updated.

use crate::providers::ToolDefinition;
use serde_json::{Value, json};

/// Return the TodoWrite tool definition.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "TodoWrite".to_string(),
        description: "Write or update your task checklist. Replaces the entire todo list. \
            Use markdown checkboxes: `- [x]` for done, `- [ ]` for pending. \
            The todo list is automatically shown in your context every turn."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The full todo list in markdown checkbox format"
                }
            },
            "required": ["content"]
        }),
    }]
}

/// Format a todo list for CLI display with visual checkboxes.
pub fn format_todo_display(content: &str) -> String {
    let mut output = String::new();
    output.push_str("  \x1b[1m\u{1f4cb} Todo\x1b[0m\n");
    output.push_str("  \x1b[90m\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\x1b[0m\n");

    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(task) = trimmed
            .strip_prefix("- [x] ")
            .or_else(|| trimmed.strip_prefix("- [X] "))
        {
            output.push_str(&format!(
                "  \x1b[32m\u{2705}\x1b[0m \x1b[9m\x1b[90m{task}\x1b[0m\n"
            ));
        } else if let Some(task) = trimmed.strip_prefix("- [ ] ") {
            output.push_str(&format!("  \x1b[90m\u{2b1c}\x1b[0m {task}\n"));
        } else if let Some(task) = trimmed
            .strip_prefix("  - [x] ")
            .or_else(|| trimmed.strip_prefix("  - [X] "))
        {
            output.push_str(&format!(
                "    \x1b[32m\u{2705}\x1b[0m \x1b[9m\x1b[90m{task}\x1b[0m\n"
            ));
        } else if let Some(task) = trimmed.strip_prefix("  - [ ] ") {
            output.push_str(&format!("    \x1b[90m\u{2b1c}\x1b[0m {task}\n"));
        } else if !trimmed.is_empty() {
            output.push_str(&format!("  {trimmed}\n"));
        }
    }

    output.push_str("  \x1b[90m\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\x1b[0m");
    output
}

/// Extract the content string from tool arguments.
pub fn extract_content(args: &Value) -> Option<String> {
    args.get("content")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_definitions() {
        let defs = definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "TodoWrite");
    }

    #[test]
    fn test_format_todo_display() {
        let content =
            "- [x] Setup project\n- [ ] Write tests\n  - [ ] Unit tests\n  - [x] Integration tests";
        let output = format_todo_display(content);
        // Should contain visual checkmarks
        assert!(output.contains("Todo"));
        assert!(output.contains("Setup project"));
        assert!(output.contains("Write tests"));
        assert!(output.contains("Unit tests"));
        assert!(output.contains("Integration tests"));
    }

    #[test]
    fn test_extract_content() {
        let args = json!({"content": "- [ ] Task one"});
        assert_eq!(extract_content(&args).unwrap(), "- [ ] Task one");

        let args = json!({});
        assert!(extract_content(&args).is_none());
    }
}
