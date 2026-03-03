//! User confirmation for potentially dangerous tool operations.
//!
//! Shows a confirmation prompt before executing shell commands,
//! file deletions, and file modifications. Uses arrow-key selection
//! with approve/reject/feedback options.
//!
//! In Normal mode, safe bash commands skip confirmation entirely.
//! The "Always allow" option adds the command pattern to the user's
//! whitelist in `~/.config/koda/settings.toml`.

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
    /// User approved AND wants this command pattern always auto-approved.
    AlwaysAllow,
}

/// Show a confirmation prompt for a tool action.
///
/// Displays the tool banner, action details, an optional diff preview,
/// and an arrow-key selector with Approve / Reject / Reject with feedback.
/// For Bash commands, also offers "Always allow" to whitelist the pattern.
pub fn confirm_tool_action(
    tool_name: &str,
    detail: &str,
    preview: Option<&str>,
    whitelist_hint: Option<&str>,
) -> Confirmation {
    // Show the action details (skip for Bash — header already shows the command)
    if tool_name != "Bash" {
        println!("  \x1b[90m{detail}\x1b[0m");
    }

    // Show diff preview if available
    if let Some(preview_text) = preview {
        println!();
        for line in preview_text.lines() {
            println!("  {line}");
        }
    }
    println!();

    let mut options = vec![
        SelectOption::new("✓ Approve", "Execute this action"),
        SelectOption::new("✗ Reject", "Skip this action"),
        SelectOption::new("💬 Feedback", "Reject and tell koda what to change"),
    ];

    // Offer "Always allow" for Bash commands
    if let Some(pattern) = whitelist_hint {
        options.push(SelectOption::new(
            "🔓 Always allow",
            format!("Auto-approve '{pattern}' from now on"),
        ));
    }

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
        Ok(Some(3)) => Confirmation::AlwaysAllow,
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
    fn test_confirmation_enum_variants() {
        // Ensure all variants are distinct
        assert_ne!(Confirmation::Approved, Confirmation::Rejected);
        assert_ne!(Confirmation::Approved, Confirmation::AlwaysAllow);
        assert_eq!(
            Confirmation::RejectedWithFeedback("fix it".into()),
            Confirmation::RejectedWithFeedback("fix it".into())
        );
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
