//! Task signature extraction for per-task-type learning.
//!
//! Fingerprints tasks so the InterventionObserver can learn
//! domain-specific autonomy preferences (e.g., "user always approves
//! git tasks but reviews refactoring carefully").

/// High-level domain of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum TaskDomain {
    Git,
    Refactor,
    Test,
    Release,
    Debug,
    General,
}

impl std::fmt::Display for TaskDomain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Git => write!(f, "git"),
            Self::Refactor => write!(f, "refactor"),
            Self::Test => write!(f, "test"),
            Self::Release => write!(f, "release"),
            Self::Debug => write!(f, "debug"),
            Self::General => write!(f, "general"),
        }
    }
}

/// Scope of changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum TaskScope {
    SingleFile,
    MultiFile,
    Project,
}

impl std::fmt::Display for TaskScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SingleFile => write!(f, "single_file"),
            Self::MultiFile => write!(f, "multi_file"),
            Self::Project => write!(f, "project"),
        }
    }
}

/// Fingerprint of a task for per-type learning.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TaskSignature {
    pub domain: TaskDomain,
    pub scope: TaskScope,
}

impl std::fmt::Display for TaskSignature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.domain, self.scope)
    }
}

impl TaskSignature {
    /// Classify a task from the user prompt.
    ///
    /// Uses keyword matching as a fast heuristic. The #320 design calls for
    /// LLM-classified structured output at session start (~50 tokens), but
    /// keyword matching is the fallback when no LLM is available.
    pub fn from_prompt(prompt: &str) -> Self {
        let lower = prompt.to_lowercase();
        let domain = classify_domain(&lower);
        let scope = classify_scope(&lower);
        Self { domain, scope }
    }
}

/// Classify the domain from prompt keywords.
fn classify_domain(lower: &str) -> TaskDomain {
    // Count matches for each domain
    let git_signals = [
        "merge",
        "branch",
        "commit",
        "rebase",
        "cherry-pick",
        "git ",
        "pull request",
        "pr ",
        "stash",
        "checkout",
    ];
    let test_signals = [
        "test",
        "spec",
        "assert",
        "expect",
        "mock",
        "fixture",
        "coverage",
        "pytest",
        "jest",
        "cargo test",
    ];
    let refactor_signals = [
        "refactor",
        "rename",
        "extract",
        "inline",
        "move ",
        "reorganize",
        "restructure",
        "clean up",
        "simplify",
    ];
    let release_signals = [
        "release",
        "version",
        "changelog",
        "deploy",
        "publish",
        "tag ",
        "bump",
        "ship",
    ];
    let debug_signals = [
        "debug",
        "fix ",
        "bug",
        "error",
        "crash",
        "issue",
        "broken",
        "failing",
        "investigate",
    ];

    let counts = [
        (TaskDomain::Git, count_matches(lower, &git_signals)),
        (TaskDomain::Test, count_matches(lower, &test_signals)),
        (
            TaskDomain::Refactor,
            count_matches(lower, &refactor_signals),
        ),
        (TaskDomain::Release, count_matches(lower, &release_signals)),
        (TaskDomain::Debug, count_matches(lower, &debug_signals)),
    ];

    // Pick the domain with the most matches (require ≥2 for specificity)
    let best = counts.iter().max_by_key(|(_, c)| *c).unwrap();
    if best.1 >= 2 {
        best.0
    } else {
        TaskDomain::General
    }
}

/// Classify the scope from prompt keywords.
fn classify_scope(lower: &str) -> TaskScope {
    let project_signals = [
        "project",
        "codebase",
        "repo",
        "repository",
        "all files",
        "everywhere",
        "across",
        "entire",
    ];
    let multi_signals = [
        "files",
        "modules",
        "components",
        "several",
        "multiple",
        "both",
        "each",
    ];

    if count_matches(lower, &project_signals) >= 1 {
        TaskScope::Project
    } else if count_matches(lower, &multi_signals) >= 1 {
        TaskScope::MultiFile
    } else {
        TaskScope::SingleFile
    }
}

/// Count how many signal keywords appear in the text.
fn count_matches(text: &str, signals: &[&str]) -> usize {
    signals.iter().filter(|s| text.contains(**s)).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_git_domain() {
        let sig = TaskSignature::from_prompt("merge the feature branch and resolve conflicts");
        assert_eq!(sig.domain, TaskDomain::Git);
    }

    #[test]
    fn test_test_domain() {
        let sig = TaskSignature::from_prompt("write unit tests for the parser with mocks");
        assert_eq!(sig.domain, TaskDomain::Test);
    }

    #[test]
    fn test_refactor_domain() {
        let sig = TaskSignature::from_prompt("refactor the database module and extract helpers");
        assert_eq!(sig.domain, TaskDomain::Refactor);
    }

    #[test]
    fn test_release_domain() {
        let sig =
            TaskSignature::from_prompt("prepare the release, bump version and update changelog");
        assert_eq!(sig.domain, TaskDomain::Release);
    }

    #[test]
    fn test_debug_domain() {
        let sig = TaskSignature::from_prompt("fix the crash bug when loading config");
        assert_eq!(sig.domain, TaskDomain::Debug);
    }

    #[test]
    fn test_general_domain_ambiguous() {
        let sig = TaskSignature::from_prompt("can you help me with this?");
        assert_eq!(sig.domain, TaskDomain::General);
    }

    #[test]
    fn test_project_scope() {
        let sig = TaskSignature::from_prompt("search the entire codebase for TODO comments");
        assert_eq!(sig.scope, TaskScope::Project);
    }

    #[test]
    fn test_multi_file_scope() {
        let sig = TaskSignature::from_prompt("update both modules to use the new API");
        assert_eq!(sig.scope, TaskScope::MultiFile);
    }

    #[test]
    fn test_single_file_scope() {
        let sig = TaskSignature::from_prompt("add a method to main.rs");
        assert_eq!(sig.scope, TaskScope::SingleFile);
    }

    #[test]
    fn test_display() {
        let sig = TaskSignature {
            domain: TaskDomain::Git,
            scope: TaskScope::Project,
        };
        assert_eq!(sig.to_string(), "git:project");
    }

    #[test]
    fn test_requires_two_matches() {
        // Single keyword shouldn't classify ("test" alone)
        let sig = TaskSignature::from_prompt("test this thing");
        assert_eq!(sig.domain, TaskDomain::General);
    }
}
