//! Todo tool: session-scoped task tracker.
//!
//! The LLM writes a markdown checklist via `TodoWrite`. The current
//! todo list is injected into the system prompt every turn so the LLM
//! always knows what's done and what's next.
//!
//! Koda renders the todo with visual formatting whenever it's updated.

use crate::providers::ToolDefinition;
use serde_json::{Value, json};

/// Return the TodoRead and TodoWrite tool definitions.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "TodoRead".to_string(),
            description: "Read the current task checklist. Returns the todo list \
                in markdown checkbox format, or a message if no todo exists."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
        ToolDefinition {
            name: "TodoWrite".to_string(),
            description: "Write or update your task checklist. Replaces the entire todo list. \
            Use markdown checkboxes: `- [x]` for done, `- [ ]` for pending. \
            Call this BEFORE starting multi-step work to create the plan, then call again \
            after EACH step to mark it done. The todo is shown to the user after every turn."
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
        },
    ]
}

/// Format a todo list for CLI display with visual checkboxes.
pub fn format_todo_display(content: &str) -> String {
    let mut output = String::new();
    output.push_str("  \x1b[1m\u{1f4cb} Todo\x1b[0m\n");

    // Find the first unchecked item to highlight as "active"
    let first_pending = content.lines().position(|l| {
        let t = l.trim();
        t.starts_with("- [ ] ") || t.starts_with("  - [ ] ")
    });

    for (i, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        let is_active = Some(i) == first_pending;

        if let Some(task) = trimmed
            .strip_prefix("- [x] ")
            .or_else(|| trimmed.strip_prefix("- [X] "))
        {
            // Done: green check + strikethrough
            output.push_str(&format!(
                "  \x1b[32m\u{2714}\x1b[0m \x1b[9m\x1b[90m{task}\x1b[0m\n"
            ));
        } else if let Some(task) = trimmed.strip_prefix("- [ ] ") {
            if is_active {
                // Active: bold white square + bold text
                output.push_str(&format!("  \x1b[33m\u{25a0}\x1b[0m \x1b[1m{task}\x1b[0m\n"));
            } else {
                // Pending: dim square
                output.push_str(&format!(
                    "  \x1b[90m\u{25a1}\x1b[0m \x1b[90m{task}\x1b[0m\n"
                ));
            }
        } else if let Some(task) = trimmed
            .strip_prefix("  - [x] ")
            .or_else(|| trimmed.strip_prefix("  - [X] "))
        {
            // Nested done
            output.push_str(&format!(
                "    \x1b[32m\u{2714}\x1b[0m \x1b[9m\x1b[90m{task}\x1b[0m\n"
            ));
        } else if let Some(task) = trimmed.strip_prefix("  - [ ] ") {
            if is_active {
                output.push_str(&format!(
                    "    \x1b[33m\u{25a0}\x1b[0m \x1b[1m{task}\x1b[0m\n"
                ));
            } else {
                output.push_str(&format!(
                    "    \x1b[90m\u{25a1}\x1b[0m \x1b[90m{task}\x1b[0m\n"
                ));
            }
        } else if !trimmed.is_empty() {
            output.push_str(&format!("  {trimmed}\n"));
        }
    }

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
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].name, "TodoRead");
        assert_eq!(defs[1].name, "TodoWrite");
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
