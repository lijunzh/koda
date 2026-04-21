//! Workspace provisioning — separates the "where does the slot write?"
//! decision from the "what can the slot do?" policy.
//!
//! Per #934 §4.6 the sandbox runtime is workspace-agnostic. Provider impls
//! land across phases:
//!
//! | Impl                  | Phase | Backend                              |
//! |-----------------------|-------|--------------------------------------|
//! | [`CwdProvider`]       | 0     | Returns `project_root` as-is         |
//! | `GitWorktreeProvider` | 2     | Wraps existing `koda-core::worktree` |
//! | `ClonefileProvider`   | 4     | macOS APFS `clonefile(2)`            |
//! | `OverlayfsProvider`   | 4     | Linux overlayfs inside bwrap         |
//!
//! The pool selects which provider to use at slot acquisition time, based
//! on agent persona + trust mode + git availability.

use anyhow::Result;
use async_trait::async_trait;
use std::path::{Path, PathBuf};

/// Workspace lifecycle: provision on slot acquire, release on slot drop.
///
/// `slot_id` is supplied by the pool so providers can name their backing
/// storage deterministically (e.g. `~/.koda/worktrees/<slot_id>`).
#[async_trait]
pub trait WorkspaceProvider: Send + Sync {
    /// Provision a writable view for a new slot. Returns the path the
    /// slot should treat as its writable root.
    async fn provision(&self, slot_id: &str) -> Result<PathBuf>;

    /// Release on slot drop. Returns `Some(diff)` when there are unsaved
    /// changes worth surfacing to the user (Phase 2+ semantics — Phase 0
    /// always returns `None`).
    async fn release(&self, slot_id: &str, path: &Path) -> Result<Option<String>>;
}

/// No-op provider: hands back the project root unchanged. Suitable for:
///
/// - Read-only slots (plan/explore/verify personas)
/// - Trust-mode `Auto` on non-git projects
/// - Any scenario where copy-on-write isn't worth the latency
///
/// This is the *only* provider implemented in Phase 0; it lets the
/// sandbox layer ship without depending on `koda-core::worktree` yet.
#[derive(Debug, Clone)]
pub struct CwdProvider {
    project_root: PathBuf,
}

impl CwdProvider {
    /// Construct a provider rooted at the given project directory.
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        Self {
            project_root: project_root.into(),
        }
    }
}

#[async_trait]
impl WorkspaceProvider for CwdProvider {
    async fn provision(&self, _slot_id: &str) -> Result<PathBuf> {
        Ok(self.project_root.clone())
    }

    async fn release(&self, _slot_id: &str, _path: &Path) -> Result<Option<String>> {
        // Nothing to clean up — we never created anything.
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cwd_provider_returns_root_unchanged() {
        let root = PathBuf::from("/tmp/some-project");
        let p = CwdProvider::new(&root);
        let provisioned = p.provision("slot-1").await.unwrap();
        assert_eq!(provisioned, root);
    }

    #[tokio::test]
    async fn cwd_provider_release_is_noop() {
        let p = CwdProvider::new("/tmp/x");
        assert!(
            p.release("slot-1", Path::new("/tmp/x"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn cwd_provider_provision_is_idempotent() {
        // Provider should be safe to call provision() multiple times for
        // the same slot — pool reuse pattern in Phase 4.
        let p = CwdProvider::new("/tmp/x");
        let a = p.provision("slot-1").await.unwrap();
        let b = p.provision("slot-1").await.unwrap();
        assert_eq!(a, b);
    }
}
