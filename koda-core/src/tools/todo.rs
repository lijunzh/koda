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

/// Parse a todo list into structured items for client rendering.
///
/// Returns a list of `(indent_level, status, text)` tuples where
/// status is `"done"`, `"active"` (first pending), `"pending"`, or `"text"`.
pub fn parse_todo_items(content: &str) -> Vec<(usize, &'static str, String)> {
    // Find the first unchecked item to highlight as "active"
    let first_pending = content.lines().position(|l| {
        let t = l.trim();
        t.starts_with("- [ ] ") || t.starts_with("  - [ ] ")
    });

    let mut items = Vec::new();

    for (i, line) in content.lines().enumerate() {
        let is_active = Some(i) == first_pending;

        // Check nested (indented) patterns first, before trimming
        if let Some(task) = line
            .strip_prefix("  - [x] ")
            .or_else(|| line.strip_prefix("  - [X] "))
        {
            items.push((1, "done", task.to_string()));
        } else if let Some(task) = line.strip_prefix("  - [ ] ") {
            let status = if is_active { "active" } else { "pending" };
            items.push((1, status, task.to_string()));
        } else if let Some(task) = line
            .trim()
            .strip_prefix("- [x] ")
            .or_else(|| line.trim().strip_prefix("- [X] "))
        {
            items.push((0, "done", task.to_string()));
        } else if let Some(task) = line.trim().strip_prefix("- [ ] ") {
            let status = if is_active { "active" } else { "pending" };
            items.push((0, status, task.to_string()));
        } else if !line.trim().is_empty() {
            items.push((0, "text", line.trim().to_string()));
        }
    }

    items
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
    fn test_parse_todo_items() {
        let content =
            "- [x] Setup project\n- [ ] Write tests\n  - [ ] Unit tests\n  - [x] Integration tests";
        let items = parse_todo_items(content);
        assert_eq!(items.len(), 4);
        assert_eq!(items[0], (0, "done", "Setup project".into()));
        assert_eq!(items[1], (0, "active", "Write tests".into())); // first pending = active
        assert_eq!(items[2], (1, "pending", "Unit tests".into()));
        assert_eq!(items[3], (1, "done", "Integration tests".into()));
    }

    #[test]
    fn test_extract_content() {
        let args = json!({"content": "- [ ] Task one"});
        assert_eq!(extract_content(&args).unwrap(), "- [ ] Task one");

        let args = json!({});
        assert!(extract_content(&args).is_none());
    }
}
