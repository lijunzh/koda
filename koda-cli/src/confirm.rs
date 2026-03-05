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
    _tool_name: &str,
    detail: &str,
    preview: Option<&koda_core::preview::DiffPreview>,
    whitelist_hint: Option<&str>,
) -> Confirmation {
    // Show the action details
    // For Bash: always show the full command (banner truncates long commands)
    println!("  \x1b[90m{detail}\x1b[0m");

    // Show diff preview if available
    if let Some(preview) = preview {
        println!();
        let rendered = crate::diff_render::render(preview);
        for line in rendered.lines() {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
