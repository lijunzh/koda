//! Sub-agent delegation scope.
//!
//! When a parent agent spawns a sub-agent, it can constrain what the
//! sub-agent is allowed to do via a `DelegationScope`. This limits
//! blast radius from prompt injection attacks.

use crate::approval::ApprovalMode;
use path_clean::PathClean;
use std::path::{Path, PathBuf};

/// Filesystem access grant for a delegated sub-agent.
#[derive(Debug, Clone)]
pub enum FsGrant {
    /// Read anything within project_root, write nothing.
    ReadOnly,
    /// Read + write within specific paths only.
    Scoped {
        read_paths: Vec<PathBuf>,
        write_paths: Vec<PathBuf>,
    },
    /// Full project_root access (default for auto mode).
    FullProject,
}

/// Constraints on what a sub-agent can do.
///
/// Rules:
/// - Mode can never escalate (safe parent → safe sub-agent only)
/// - Scope can only narrow (FullProject → Scoped, never reverse)
/// - Auto mode default: FullProject (no friction unless explicitly constrained)
#[derive(Debug, Clone)]
pub struct DelegationScope {
    /// Approval mode — can never exceed parent's mode.
    pub mode: ApprovalMode,
    /// Filesystem grant.
    pub fs_grant: FsGrant,
    /// Tool allowlist. None = inherit parent's full set.
    pub allowed_tools: Option<Vec<String>>,
    /// Whether the sub-agent can spawn further sub-agents.
    pub can_delegate: bool,
}

impl DelegationScope {
    /// Default scope for auto mode: full project access, can delegate.
    pub fn auto_default(parent_mode: ApprovalMode) -> Self {
        Self {
            mode: parent_mode,
            fs_grant: FsGrant::FullProject,
            allowed_tools: None,
            can_delegate: true,
        }
    }

    /// Restricted scope: read-only filesystem, no delegation.
    pub fn read_only(parent_mode: ApprovalMode) -> Self {
        Self {
            mode: clamp_mode(parent_mode, ApprovalMode::Safe),
            fs_grant: FsGrant::ReadOnly,
            allowed_tools: None,
            can_delegate: false,
        }
    }

    /// Check if a file write is allowed by this scope.
    pub fn can_write(&self, path: &Path, project_root: &Path) -> bool {
        match &self.fs_grant {
            FsGrant::ReadOnly => false,
            FsGrant::FullProject => {
                // Must be within project_root
                let resolved = resolve(path, project_root);
                resolved.starts_with(project_root)
            }
            FsGrant::Scoped { write_paths, .. } => {
                let resolved = resolve(path, project_root);
                write_paths
                    .iter()
                    .any(|wp| resolved.starts_with(project_root.join(wp).clean()))
            }
        }
    }

    /// Check if a file read is allowed by this scope.
    pub fn can_read(&self, path: &Path, project_root: &Path) -> bool {
        match &self.fs_grant {
            FsGrant::ReadOnly | FsGrant::FullProject => {
                let resolved = resolve(path, project_root);
                resolved.starts_with(project_root)
            }
            FsGrant::Scoped { read_paths, .. } => {
                let resolved = resolve(path, project_root);
                read_paths
                    .iter()
                    .any(|rp| resolved.starts_with(project_root.join(rp).clean()))
            }
        }
    }

    /// Check if a tool is allowed by this scope.
    pub fn is_tool_allowed(&self, tool_name: &str) -> bool {
        match &self.allowed_tools {
            None => true,
            Some(allowed) => allowed.iter().any(|t| t == tool_name),
        }
    }
}

/// Clamp a child mode to never exceed the parent's mode.
/// Mode ordering: Safe < Strict < Auto.
fn clamp_mode(parent: ApprovalMode, child: ApprovalMode) -> ApprovalMode {
    if (child as u8) > (parent as u8) {
        parent
    } else {
        child
    }
}

/// Resolve a path relative to project_root.
fn resolve(path: &Path, project_root: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf().clean()
    } else {
        project_root.join(path).clean()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        PathBuf::from("/home/user/project")
    }

    #[test]
    fn test_full_project_allows_writes_inside() {
        let scope = DelegationScope::auto_default(ApprovalMode::Auto);
        assert!(scope.can_write(Path::new("src/main.rs"), &root()));
    }

    #[test]
    fn test_full_project_blocks_writes_outside() {
        let scope = DelegationScope::auto_default(ApprovalMode::Auto);
        assert!(!scope.can_write(Path::new("/etc/passwd"), &root()));
    }

    #[test]
    fn test_read_only_blocks_all_writes() {
        let scope = DelegationScope::read_only(ApprovalMode::Auto);
        assert!(!scope.can_write(Path::new("src/main.rs"), &root()));
        assert!(scope.can_read(Path::new("src/main.rs"), &root()));
    }

    #[test]
    fn test_scoped_write_paths() {
        let scope = DelegationScope {
            mode: ApprovalMode::Auto,
            fs_grant: FsGrant::Scoped {
                read_paths: vec![PathBuf::from(".")],
                write_paths: vec![PathBuf::from("src/")],
            },
            allowed_tools: None,
            can_delegate: false,
        };
        assert!(scope.can_write(Path::new("src/main.rs"), &root()));
        assert!(!scope.can_write(Path::new("tests/test.rs"), &root()));
        assert!(scope.can_read(Path::new("tests/test.rs"), &root()));
    }

    #[test]
    fn test_mode_clamping() {
        // Safe parent can't spawn Auto child
        assert_eq!(
            clamp_mode(ApprovalMode::Safe, ApprovalMode::Auto),
            ApprovalMode::Safe
        );
        // Auto parent can spawn any child
        assert_eq!(
            clamp_mode(ApprovalMode::Auto, ApprovalMode::Safe),
            ApprovalMode::Safe
        );
        // Same mode passes through
        assert_eq!(
            clamp_mode(ApprovalMode::Strict, ApprovalMode::Strict),
            ApprovalMode::Strict
        );
    }

    #[test]
    fn test_tool_allowlist() {
        let scope = DelegationScope {
            mode: ApprovalMode::Auto,
            fs_grant: FsGrant::FullProject,
            allowed_tools: Some(vec!["Read".to_string(), "Grep".to_string()]),
            can_delegate: false,
        };
        assert!(scope.is_tool_allowed("Read"));
        assert!(scope.is_tool_allowed("Grep"));
        assert!(!scope.is_tool_allowed("Write"));
    }

    #[test]
    fn test_tool_allowlist_none_allows_all() {
        let scope = DelegationScope::auto_default(ApprovalMode::Auto);
        assert!(scope.is_tool_allowed("Write"));
        assert!(scope.is_tool_allowed("Delete"));
    }

    #[test]
    fn test_read_only_clamps_to_safe() {
        let scope = DelegationScope::read_only(ApprovalMode::Auto);
        assert_eq!(scope.mode, ApprovalMode::Safe);
    }
}
