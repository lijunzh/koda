// Vendored from openai/codex (Apache-2.0) — see top-level NOTICE.
// Source: codex-rs/protocol/src/agent_path.rs
// Local modifications:
//   - Dropped the `/morpheus` special-case path (Codex-specific feature).
//   - Dropped `JsonSchema` and `TS` derives (koda doesn't currently depend
//     on `schemars` or `ts-rs`; can be added later if we expose this over
//     the ACP wire protocol).

//! Typed absolute path identifying an agent in the spawn tree.
//!
//! Paths look like `/root`, `/root/researcher`, `/root/researcher/worker`.
//! Names use `[a-z0-9_]+` only — no slashes, no uppercase, no `.`/`..`.
//!
//! This is the `AgentPath` from Codex's mailbox/peer-agent design: every
//! agent (including the root user-facing one) has a stable path that
//! `InterAgentCommunication` uses for `author` / `recipient`.

use serde::Deserialize;
use serde::Serialize;
use std::fmt;
use std::ops::Deref;
use std::str::FromStr;

/// Typed absolute path identifying an agent in the spawn tree.
///
/// See module docs for format and validation rules.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AgentPath(String);

impl AgentPath {
    /// Path of the user-facing root agent.
    pub const ROOT: &str = "/root";
    const ROOT_SEGMENT: &str = "root";

    /// Returns the root agent path (`/root`).
    pub fn root() -> Self {
        Self(Self::ROOT.to_string())
    }

    /// Validates and constructs an `AgentPath` from an absolute path string.
    pub fn from_string(path: String) -> Result<Self, String> {
        validate_absolute_path(path.as_str())?;
        Ok(Self(path))
    }

    /// Borrows the underlying path string (`/root/...`).
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// True iff this path is the root agent.
    pub fn is_root(&self) -> bool {
        self.as_str() == Self::ROOT
    }

    /// Final path segment (`/root/researcher/worker` → `worker`).
    /// The root path returns the literal `"root"`.
    pub fn name(&self) -> &str {
        if self.is_root() {
            return Self::ROOT_SEGMENT;
        }
        self.as_str()
            .rsplit('/')
            .next()
            .filter(|segment| !segment.is_empty())
            .unwrap_or(Self::ROOT_SEGMENT)
    }

    /// Append a child segment: `/root.join("worker") == /root/worker`.
    pub fn join(&self, agent_name: &str) -> Result<Self, String> {
        validate_agent_name(agent_name)?;
        Self::from_string(format!("{self}/{agent_name}"))
    }

    /// Resolve a reference relative to `self`. Absolute references
    /// (starting with `/`) bypass `self`; relative ones are appended.
    pub fn resolve(&self, reference: &str) -> Result<Self, String> {
        if reference.is_empty() {
            return Err("agent path must not be empty".to_string());
        }
        if reference == Self::ROOT {
            return Ok(Self::root());
        }
        if reference.starts_with('/') {
            return Self::try_from(reference);
        }

        validate_relative_reference(reference)?;
        Self::from_string(format!("{self}/{reference}"))
    }
}

impl TryFrom<String> for AgentPath {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::from_string(value)
    }
}

impl TryFrom<&str> for AgentPath {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::from_string(value.to_string())
    }
}

impl From<AgentPath> for String {
    fn from(value: AgentPath) -> Self {
        value.0
    }
}

impl FromStr for AgentPath {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::try_from(s)
    }
}

impl AsRef<str> for AgentPath {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Deref for AgentPath {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl fmt::Display for AgentPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

fn validate_agent_name(agent_name: &str) -> Result<(), String> {
    if agent_name.is_empty() {
        return Err("agent_name must not be empty".to_string());
    }
    if agent_name == AgentPath::ROOT_SEGMENT {
        return Err("agent_name `root` is reserved".to_string());
    }
    if agent_name == "." || agent_name == ".." {
        return Err(format!("agent_name `{agent_name}` is reserved"));
    }
    if agent_name.contains('/') {
        return Err("agent_name must not contain `/`".to_string());
    }
    if !agent_name
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
    {
        return Err(
            "agent_name must use only lowercase letters, digits, and underscores".to_string(),
        );
    }
    Ok(())
}

fn validate_absolute_path(path: &str) -> Result<(), String> {
    let Some(stripped) = path.strip_prefix('/') else {
        return Err("absolute agent paths must start with `/root`".to_string());
    };
    let mut segments = stripped.split('/');
    let Some(root) = segments.next() else {
        return Err("absolute agent path must not be empty".to_string());
    };
    if root != AgentPath::ROOT_SEGMENT {
        return Err("absolute agent paths must start with `/root`".to_string());
    }
    if stripped.ends_with('/') {
        return Err("absolute agent path must not end with `/`".to_string());
    }
    for segment in segments {
        validate_agent_name(segment)?;
    }
    Ok(())
}

fn validate_relative_reference(reference: &str) -> Result<(), String> {
    if reference.ends_with('/') {
        return Err("relative agent path must not end with `/`".to_string());
    }
    for segment in reference.split('/') {
        validate_agent_name(segment)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    // Tests vendored verbatim from codex (minus the morpheus-path test
    // since we dropped that special case). Behavioral lockdown — these
    // are the reference semantics for AgentPath.

    use super::AgentPath;

    #[test]
    fn root_has_expected_name() {
        let root = AgentPath::root();
        assert_eq!(root.as_str(), AgentPath::ROOT);
        assert_eq!(root.name(), "root");
        assert!(root.is_root());
    }

    #[test]
    fn join_builds_child_paths() {
        let root = AgentPath::root();
        let child = root.join("researcher").expect("child path");
        assert_eq!(child.as_str(), "/root/researcher");
        assert_eq!(child.name(), "researcher");
    }

    #[test]
    fn nested_join_builds_grandchild_paths() {
        let root = AgentPath::root();
        let grandchild = root
            .join("researcher")
            .expect("child path")
            .join("worker")
            .expect("grandchild path");
        assert_eq!(grandchild.as_str(), "/root/researcher/worker");
        assert_eq!(grandchild.name(), "worker");
    }

    #[test]
    fn resolve_supports_relative_and_absolute_references() {
        let current = AgentPath::try_from("/root/researcher").expect("path");
        assert_eq!(
            current.resolve("worker").expect("relative path"),
            AgentPath::try_from("/root/researcher/worker").expect("path")
        );
        assert_eq!(
            current.resolve("/root/other").expect("absolute path"),
            AgentPath::try_from("/root/other").expect("path")
        );
    }

    #[test]
    fn invalid_names_and_paths_are_rejected() {
        assert_eq!(
            AgentPath::root().join("BadName"),
            Err("agent_name must use only lowercase letters, digits, and underscores".to_string())
        );
        assert_eq!(
            AgentPath::try_from("/not-root"),
            Err("absolute agent paths must start with `/root`".to_string())
        );
        assert_eq!(
            AgentPath::root().resolve("../sibling"),
            Err("agent_name `..` is reserved".to_string())
        );
    }

    #[test]
    fn empty_name_rejected() {
        assert_eq!(
            AgentPath::root().join(""),
            Err("agent_name must not be empty".to_string())
        );
    }

    #[test]
    fn root_name_reserved() {
        assert_eq!(
            AgentPath::root().join("root"),
            Err("agent_name `root` is reserved".to_string())
        );
    }

    #[test]
    fn serde_roundtrip() {
        // Serializes as a bare string thanks to `#[serde(into = "String")]`.
        let path = AgentPath::try_from("/root/researcher").expect("path");
        let json = serde_json::to_string(&path).expect("serialize");
        assert_eq!(json, "\"/root/researcher\"");
        let back: AgentPath = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, path);
    }

    #[test]
    fn serde_rejects_invalid_path() {
        let res: Result<AgentPath, _> = serde_json::from_str("\"BadName\"");
        assert!(res.is_err(), "expected serde rejection of bad path");
    }
}
