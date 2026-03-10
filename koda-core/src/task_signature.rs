//! Task signature — fingerprints tasks for per-type learning.
//!
//! The LLM classifies {domain, scope} in the Observe phase via structured
//! output (~50 tokens). `from_prompt()` is a zero-cost fallback that always
//! returns General:SingleFile — no keyword stemming, no NLP pipeline.
//!
//! See #329 (Trap 3.5) for rationale.

/// High-level domain of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
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
#[serde(rename_all = "snake_case")]
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

impl Default for TaskSignature {
    fn default() -> Self {
        Self {
            domain: TaskDomain::General,
            scope: TaskScope::SingleFile,
        }
    }
}

impl TaskSignature {
    /// Fallback classification — always General:SingleFile.
    ///
    /// The real classification happens when the LLM emits `{domain, scope}`
    /// in its first Observe-phase response. This exists so we always have
    /// a signature, even if the LLM doesn't classify.
    pub fn from_prompt(_prompt: &str) -> Self {
        Self::default()
    }

    /// Construct from LLM-classified domain and scope strings.
    ///
    /// Falls back to General / SingleFile for unrecognized values.
    pub fn from_llm(domain: &str, scope: &str) -> Self {
        let domain = match domain {
            "git" => TaskDomain::Git,
            "refactor" => TaskDomain::Refactor,
            "test" => TaskDomain::Test,
            "release" => TaskDomain::Release,
            "debug" => TaskDomain::Debug,
            _ => TaskDomain::General,
        };
        let scope = match scope {
            "single_file" => TaskScope::SingleFile,
            "multi_file" => TaskScope::MultiFile,
            "project" => TaskScope::Project,
            _ => TaskScope::SingleFile,
        };
        Self { domain, scope }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_prompt_always_general() {
        let sig = TaskSignature::from_prompt("merge the feature branch and resolve conflicts");
        assert_eq!(sig.domain, TaskDomain::General);
        assert_eq!(sig.scope, TaskScope::SingleFile);
    }

    #[test]
    fn test_from_llm_known_values() {
        let sig = TaskSignature::from_llm("git", "project");
        assert_eq!(sig.domain, TaskDomain::Git);
        assert_eq!(sig.scope, TaskScope::Project);
    }

    #[test]
    fn test_from_llm_unknown_falls_back() {
        let sig = TaskSignature::from_llm("quantum_computing", "galaxy");
        assert_eq!(sig.domain, TaskDomain::General);
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
    fn test_default() {
        let sig = TaskSignature::default();
        assert_eq!(sig.to_string(), "general:single_file");
    }

    #[test]
    fn test_serde_roundtrip() {
        let sig = TaskSignature::from_llm("release", "multi_file");
        let json = serde_json::to_string(&sig).unwrap();
        let deserialized: TaskSignature = serde_json::from_str(&json).unwrap();
        assert_eq!(sig, deserialized);
    }
}
