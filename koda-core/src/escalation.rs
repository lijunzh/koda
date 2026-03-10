//! Escalation detection for phase demotion.
//!
//! When a tool fails during execution, this module classifies whether
//! the error represents a scope change (requiring phase demotion to
//! Understanding) or a retryable/ignorable error.
//!
//! Conservative by design: requires ≥2 scope-change signals to escalate.
//! Single keyword matches are too noisy.

use regex::RegexSet;
use std::sync::LazyLock;

/// Patterns indicating the task scope has changed.
/// These suggest the agent needs to re-observe before re-planning.
static SCOPE_CHANGE_PATTERNS: LazyLock<RegexSet> = LazyLock::new(|| {
    RegexSet::new([
        r"(?i)CONFLICT",
        r"(?i)merge conflict",
        r"(?i)unresolved dependency",
        r"(?i)schema mismatch",
        r"(?i)breaking change",
        r"(?i)incompatible version",
        r"(?i)cannot find module",
        r"(?i)undefined reference",
        r"(?i)circular dependency",
        r"(?i)missing required",
        r"(?i)no such table",
        r"(?i)migration failed",
    ])
    .expect("valid regex patterns")
});

/// Patterns indicating retryable errors (no scope change).
/// The agent should retry or adjust the command, not re-observe.
static RETRYABLE_PATTERNS: LazyLock<RegexSet> = LazyLock::new(|| {
    RegexSet::new([
        r"(?i)permission denied",
        r"(?i)file not found",
        r"(?i)no such file",
        r"(?i)syntax error",
        r"(?i)command not found",
        r"(?i)timed? ?out",
        r"(?i)connection refused",
        r"(?i)already exists",
        r"(?i)not a directory",
        r"(?i)resource busy",
    ])
    .expect("valid regex patterns")
});

/// Result of analyzing a tool error for escalation.
#[derive(Debug, Clone, PartialEq)]
pub enum EscalationSignal {
    /// Scope changed — demote to Understanding.
    Escalate {
        /// Number of scope-change patterns matched.
        match_count: usize,
        /// Description for the reflection prompt.
        reason: String,
    },
    /// Retryable error — stay in current phase.
    Retryable,
    /// Unknown error — stay in current phase, inject reflection.
    Unknown,
}

/// Analyze tool output/error for escalation signals.
///
/// Only escalates if ≥2 scope-change patterns match (confidence gate).
/// Returns `Retryable` if any retryable pattern matches (takes precedence
/// over single scope-change matches).
pub fn classify_error(tool_name: &str, output: &str) -> EscalationSignal {
    let scope_matches: Vec<usize> = SCOPE_CHANGE_PATTERNS.matches(output).into_iter().collect();
    let retryable_matches = RETRYABLE_PATTERNS.matches(output);

    // Retryable patterns take precedence over single scope-change matches
    if retryable_matches.matched_any() && scope_matches.len() < 2 {
        return EscalationSignal::Retryable;
    }

    // Confidence gate: require ≥2 scope-change signals
    if scope_matches.len() >= 2 {
        // Build a human-readable reason from the first few matches
        let reason = format!(
            "{tool_name} failed with {} scope-change signals",
            scope_matches.len()
        );
        return EscalationSignal::Escalate {
            match_count: scope_matches.len(),
            reason,
        };
    }

    if scope_matches.len() == 1 {
        // Single match — not confident enough to escalate, but notable
        return EscalationSignal::Unknown;
    }

    EscalationSignal::Unknown
}

/// Build the reflection prompt injected when escalating.
pub fn escalation_prompt(tool_name: &str, reason: &str) -> String {
    format!(
        "\u{26a0}\u{fe0f} Tool `{tool_name}` failed \u{2014} {reason}.\n\
         This changes task scope. Falling back to Observe phase.\n\
         [Phase: Observe \u{2014} Read and understand what changed before re-planning.]"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_merge_conflict_escalates() {
        let output = "CONFLICT (content): Merge conflict in src/main.rs\n\
                      Auto-merging failed; fix conflicts and then commit.";
        match classify_error("Bash", output) {
            EscalationSignal::Escalate { match_count, .. } => {
                assert!(match_count >= 2, "expected ≥2 matches, got {match_count}");
            }
            other => panic!("expected Escalate, got {other:?}"),
        }
    }

    #[test]
    fn test_permission_denied_is_retryable() {
        let output = "error: permission denied: /etc/shadow";
        assert_eq!(classify_error("Bash", output), EscalationSignal::Retryable);
    }

    #[test]
    fn test_single_scope_signal_is_unknown() {
        // Only one scope-change pattern — not confident enough
        let output = "error: incompatible version of rustc";
        assert_eq!(classify_error("Bash", output), EscalationSignal::Unknown);
    }

    #[test]
    fn test_retryable_overrides_single_scope() {
        // Has both a retryable pattern and one scope-change pattern
        let output = "file not found: missing required config.toml";
        assert_eq!(classify_error("Bash", output), EscalationSignal::Retryable);
    }

    #[test]
    fn test_multiple_scope_signals_escalate() {
        let output = "error: incompatible version of openssl\n\
                      unresolved dependency: libssl-dev";
        match classify_error("Bash", output) {
            EscalationSignal::Escalate { match_count, .. } => {
                assert!(match_count >= 2);
            }
            other => panic!("expected Escalate, got {other:?}"),
        }
    }

    #[test]
    fn test_clean_output_is_unknown() {
        let output = "Everything compiled successfully.";
        assert_eq!(classify_error("Bash", output), EscalationSignal::Unknown);
    }

    #[test]
    fn test_escalation_prompt_format() {
        let prompt = escalation_prompt("Bash", "merge conflict detected");
        assert!(prompt.contains("Bash"));
        assert!(prompt.contains("merge conflict"));
        assert!(prompt.contains("Observe"));
    }
}
