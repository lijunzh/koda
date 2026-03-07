//! Rule-based intent classifier.
//!
//! Classifies user messages into task intents and suggests
//! appropriate skills or agents. Zero LLM cost.

/// Classified intent of a user message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskIntent {
    /// Simple question or explanation request.
    Question,
    /// Code exploration (read, search, understand).
    Explore,
    /// Code modification (edit, write, refactor).
    Modify,
    /// Test generation.
    TestGen,
    /// Code review or analysis.
    Review,
    /// Build, run, or deploy.
    Build,
    /// Complex multi-step task.
    Complex,
}

/// Optional suggestion to surface to the user.
#[derive(Debug, Clone)]
pub struct IntentSuggestion {
    pub intent: TaskIntent,
    /// Suggested skill or agent name (if applicable).
    pub suggestion: Option<String>,
    /// Short explanation for the suggestion.
    pub reason: Option<String>,
}

/// Classify a user message into an intent.
pub fn classify_intent(msg: &str) -> IntentSuggestion {
    let lower = msg.to_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();

    // Test generation
    if matches_any(
        &lower,
        &[
            "write tests",
            "add tests",
            "generate tests",
            "test coverage",
            "unit test",
        ],
    ) {
        return IntentSuggestion {
            intent: TaskIntent::TestGen,
            suggestion: Some("testgen".into()),
            reason: Some("The testgen agent specializes in test generation.".into()),
        };
    }

    // Code review
    if matches_any(&lower, &["review", "code review", "audit", "check quality"]) {
        return IntentSuggestion {
            intent: TaskIntent::Review,
            suggestion: None,
            reason: Some("Try the /review skill for expert code review.".into()),
        };
    }

    // Build/run
    if matches_any(&lower, &["build", "compile", "deploy", "run tests", "make"]) {
        return IntentSuggestion {
            intent: TaskIntent::Build,
            suggestion: None,
            reason: None,
        };
    }

    // Simple question
    if words.len() < 10
        && matches_any(
            &lower,
            &[
                "what is", "what are", "how do", "explain", "why ", "show me", "tell me",
            ],
        )
    {
        return IntentSuggestion {
            intent: TaskIntent::Question,
            suggestion: None,
            reason: None,
        };
    }

    // Exploration
    if matches_any(
        &lower,
        &[
            "find", "search", "look for", "where is", "list all", "show all", "scan",
        ],
    ) {
        return IntentSuggestion {
            intent: TaskIntent::Explore,
            suggestion: Some("scout".into()),
            reason: Some("The scout agent is optimized for codebase exploration.".into()),
        };
    }

    // Modification
    if matches_any(
        &lower,
        &[
            "refactor",
            "rename",
            "fix bug",
            "add feature",
            "implement",
            "change",
            "update",
            "modify",
            "rewrite",
        ],
    ) {
        return IntentSuggestion {
            intent: TaskIntent::Modify,
            suggestion: None,
            reason: None,
        };
    }

    // Long messages are likely complex
    if words.len() > 30 {
        return IntentSuggestion {
            intent: TaskIntent::Complex,
            suggestion: None,
            reason: None,
        };
    }

    IntentSuggestion {
        intent: TaskIntent::Modify,
        suggestion: None,
        reason: None,
    }
}

/// Check if the text contains any of the patterns.
fn matches_any(text: &str, patterns: &[&str]) -> bool {
    patterns.iter().any(|p| text.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_test_generation() {
        let r = classify_intent("write tests for the auth module");
        assert_eq!(r.intent, TaskIntent::TestGen);
        assert_eq!(r.suggestion.as_deref(), Some("testgen"));
    }

    #[test]
    fn test_classify_review() {
        let r = classify_intent("review this PR");
        assert_eq!(r.intent, TaskIntent::Review);
    }

    #[test]
    fn test_classify_question() {
        let r = classify_intent("what is this function doing?");
        assert_eq!(r.intent, TaskIntent::Question);
    }

    #[test]
    fn test_classify_exploration() {
        let r = classify_intent("find all uses of DatabaseConfig");
        assert_eq!(r.intent, TaskIntent::Explore);
        assert_eq!(r.suggestion.as_deref(), Some("scout"));
    }

    #[test]
    fn test_classify_build() {
        let r = classify_intent("build and run tests");
        assert_eq!(r.intent, TaskIntent::Build);
    }

    #[test]
    fn test_classify_modification() {
        let r = classify_intent("refactor the payment module");
        assert_eq!(r.intent, TaskIntent::Modify);
    }

    #[test]
    fn test_classify_complex_long_message() {
        let msg = "I need you to ".to_string() + &"word ".repeat(30) + "and more";
        let r = classify_intent(&msg);
        assert_eq!(r.intent, TaskIntent::Complex);
    }
}
