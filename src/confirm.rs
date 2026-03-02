//! User confirmation for potentially dangerous tool operations.
//!
//! Shows a confirmation prompt before executing shell commands,
//! file deletions, and file modifications. Uses arrow-key selection
//! with approve/reject/feedback options.

use crate::tui::SelectOption;
use std::io::Write;

/// The user's decision on a confirmation prompt.
#[derive(Debug, Clone, PartialEq)]
pub enum Confirmation {
    /// User approved the action.
    Approved,
    /// User rejected the action.
    Rejected,
    /// User rejected with feedback (tells the LLM what to change).
    RejectedWithFeedback(String),
}

/// Tools that require user confirmation.
const CONFIRM_TOOLS: &[&str] = &["Bash", "Delete", "Write", "Edit", "WebFetch"];

/// Check if a tool requires user confirmation (built-in tools only).
#[allow(dead_code)]
pub fn needs_confirmation(tool_name: &str) -> bool {
    CONFIRM_TOOLS.contains(&tool_name)
}

/// Check if a tool requires confirmation, including custom tool detection.
/// Custom tools execute shell commands, so they always require confirmation.
pub fn needs_confirmation_with_project(tool_name: &str, _project_root: &std::path::Path) -> bool {
    CONFIRM_TOOLS.contains(&tool_name)
}

/// Show a confirmation prompt for a tool action.
///
/// Displays the tool banner, action details, and an arrow-key
/// selector with Approve / Reject / Reject with feedback.
pub fn confirm_tool_action(tool_name: &str, detail: &str) -> Confirmation {
    // Show the action details (skip for Bash — header already shows the command)
    if tool_name != "Bash" {
        println!("  \x1b[90m{detail}\x1b[0m");
    }
    println!();

    let options = vec![
        SelectOption::new("✓ Approve", "Execute this action"),
        SelectOption::new("✗ Reject", "Skip this action"),
        SelectOption::new("💬 Feedback", "Reject and tell koda what to change"),
    ];

    match crate::tui::select("🐻 Confirm action?", &options, 0) {
        Ok(Some(0)) => Confirmation::Approved,
        Ok(Some(2)) => {
            // Get feedback from user
            print!("  \x1b[32m❯\x1b[0m Tell koda what to change: ");
            let _ = std::io::stdout().flush();

            let mut feedback = String::new();
            if std::io::stdin().read_line(&mut feedback).is_ok() {
                let feedback = feedback.trim().to_string();
                if feedback.is_empty() {
                    Confirmation::Rejected
                } else {
                    Confirmation::RejectedWithFeedback(feedback)
                }
            } else {
                Confirmation::Rejected
            }
        }
        _ => Confirmation::Rejected,
    }
}

/// Format a human-readable description of what a tool will do.
pub fn describe_action(tool_name: &str, args: &serde_json::Value) -> String {
    match tool_name {
        "Bash" => {
            let cmd = args
                .get("command")
                .or(args.get("cmd"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            format!("\x1b[1m{cmd}\x1b[0m")
        }
        "Delete" => {
            let path = args
                .get("file_path")
                .or(args.get("path"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let recursive = args
                .get("recursive")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if recursive {
                format!("Delete directory (recursive): \x1b[1m{path}\x1b[0m")
            } else {
                format!("Delete: \x1b[1m{path}\x1b[0m")
            }
        }
        "Write" => {
            let path = args
                .get("path")
                .or(args.get("file_path"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let overwrite = args
                .get("overwrite")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if overwrite {
                format!("Overwrite file: \x1b[1m{path}\x1b[0m")
            } else {
                format!("Create file: \x1b[1m{path}\x1b[0m")
            }
        }
        "Edit" => {
            let path = if let Some(payload) = args.get("payload") {
                payload
                    .get("file_path")
                    .or(payload.get("path"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
            } else {
                args.get("file_path")
                    .or(args.get("path"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
            };
            format!("Edit file: \x1b[1m{path}\x1b[0m")
        }
        "CreateTool" => {
            let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            format!("Create tool: \x1b[1m{name}\x1b[0m")
        }
        "DeleteTool" => {
            let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            format!("Delete tool: \x1b[1m{name}\x1b[0m")
        }
        "WebFetch" => {
            let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("?");
            format!("Fetch URL: \x1b[1m{url}\x1b[0m")
        }
        _ => format!("Execute: {tool_name}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_needs_confirmation() {
        assert!(needs_confirmation("Bash"));
        assert!(needs_confirmation("Delete"));
        assert!(needs_confirmation("Write"));
        assert!(needs_confirmation("Edit"));
        assert!(needs_confirmation("WebFetch"));
        assert!(!needs_confirmation("Read"));
        assert!(!needs_confirmation("List"));
        assert!(!needs_confirmation("Grep"));
    }

    #[test]
    fn test_describe_bash() {
        let desc = describe_action("Bash", &json!({"command": "cargo build"}));
        assert!(desc.contains("cargo build"));
    }

    #[test]
    fn test_describe_delete() {
        let desc = describe_action("Delete", &json!({"file_path": "old.rs"}));
        assert!(desc.contains("old.rs"));
    }

    #[test]
    fn test_describe_edit() {
        let desc = describe_action("Edit", &json!({"payload": {"file_path": "src/main.rs"}}));
        assert!(desc.contains("src/main.rs"));
    }

    #[test]
    fn test_describe_write() {
        let desc = describe_action("Write", &json!({"path": "new.rs"}));
        assert!(desc.contains("Create file"));
        assert!(desc.contains("new.rs"));
    }

    #[test]
    fn test_describe_write_overwrite() {
        let desc = describe_action("Write", &json!({"path": "x.rs", "overwrite": true}));
        assert!(desc.contains("Overwrite"));
    }
}
