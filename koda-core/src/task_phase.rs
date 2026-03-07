//! Task phase state machine.
//!
//! Tracks the current phase of a multi-step task to inject
//! phase-appropriate instructions into the system prompt.
//! Auto-detected from tool call patterns.

/// Current phase of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TaskPhase {
    /// Initial: reading the user's request, exploring codebase.
    #[default]
    Understanding,
    /// Planning what changes to make.
    Planning,
    /// Making changes (editing, writing, running commands).
    Executing,
    /// Verifying changes (running tests, checking output).
    Verifying,
    /// Summarizing what was done.
    Reporting,
}

impl TaskPhase {
    /// Detect phase from recent tool calls.
    pub fn detect(recent_tools: &[String]) -> Self {
        if recent_tools.is_empty() {
            return Self::Understanding;
        }

        let last_few: Vec<&str> = recent_tools
            .iter()
            .rev()
            .take(3)
            .map(|s| s.as_str())
            .collect();

        // If mostly read/search → Understanding
        let read_count = last_few
            .iter()
            .filter(|t| matches!(**t, "Read" | "List" | "Grep" | "Glob"))
            .count();

        // If mostly edit/write → Executing
        let write_count = last_few
            .iter()
            .filter(|t| matches!(**t, "Edit" | "Write" | "Delete"))
            .count();

        // If running tests → Verifying
        let bash_count = last_few.iter().filter(|t| matches!(**t, "Bash")).count();

        if bash_count >= 2 {
            Self::Verifying
        } else if write_count >= 2 {
            Self::Executing
        } else if read_count >= 2 {
            Self::Understanding
        } else if write_count >= 1 {
            Self::Executing
        } else {
            Self::Understanding
        }
    }

    /// Short label for injection into system prompt.
    pub fn prompt_hint(self) -> &'static str {
        match self {
            Self::Understanding => "[Phase: Understanding — explore before acting]",
            Self::Planning => "[Phase: Planning — outline steps before executing]",
            Self::Executing => "[Phase: Executing — apply changes carefully]",
            Self::Verifying => "[Phase: Verifying — check results, run tests]",
            Self::Reporting => "[Phase: Reporting — summarize what was done]",
        }
    }
}

impl std::fmt::Display for TaskPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Understanding => "Understanding",
                Self::Planning => "Planning",
                Self::Executing => "Executing",
                Self::Verifying => "Verifying",
                Self::Reporting => "Reporting",
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_understanding() {
        let tools = vec!["Read".into(), "Grep".into(), "List".into()];
        assert_eq!(TaskPhase::detect(&tools), TaskPhase::Understanding);
    }

    #[test]
    fn test_detect_executing() {
        let tools = vec!["Read".into(), "Edit".into(), "Write".into()];
        assert_eq!(TaskPhase::detect(&tools), TaskPhase::Executing);
    }

    #[test]
    fn test_detect_verifying() {
        let tools = vec!["Bash".into(), "Bash".into(), "Read".into()];
        assert_eq!(TaskPhase::detect(&tools), TaskPhase::Verifying);
    }

    #[test]
    fn test_detect_empty() {
        assert_eq!(TaskPhase::detect(&[]), TaskPhase::Understanding);
    }

    #[test]
    fn test_prompt_hint() {
        assert!(
            TaskPhase::Understanding
                .prompt_hint()
                .contains("Understanding")
        );
        assert!(TaskPhase::Executing.prompt_hint().contains("Executing"));
    }
}
