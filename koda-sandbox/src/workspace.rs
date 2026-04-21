//! Workspace provisioning — separates the "where does the slot write?"
//! decision from the "what can the slot do?" policy.
//!
//! Per #934 §4.6 the sandbox runtime is workspace-agnostic. Provider impls
//! land across phases:
//!
//! | Impl                  | Phase | Backend                              |
//! |-----------------------|-------|--------------------------------------|
//! | [`CwdProvider`]       | 0     | Returns `project_root` as-is         |
//! | [`GitWorktreeProvider`]| 2    | `git worktree add` per sub-agent     |
//! | `ClonefileProvider`   | 4     | macOS APFS `clonefile(2)`            |
//! | `OverlayfsProvider`   | 4     | Linux overlayfs inside bwrap         |
//!
//! The pool selects which provider to use at slot acquisition time, based
//! on agent persona + trust mode + git availability.

use anyhow::{Context, Result};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use tokio::process::Command;

// ── Trait ────────────────────────────────────────────────────────────────────

/// Workspace lifecycle: provision on slot acquire, release on slot drop.
///
/// `slot_id` is supplied by the pool so providers can name their backing
/// storage deterministically (e.g. `~/.koda/worktrees/<slot_id>`).
#[async_trait]
pub trait WorkspaceProvider: Send + Sync {
    /// Provision a writable view for a new slot. Returns the path the
    /// slot should treat as its writable root.
    async fn provision(&self, slot_id: &str) -> Result<PathBuf>;

    /// Release on slot drop. Returns `Some(hint)` when there are unsaved
    /// changes worth surfacing to the user.
    async fn release(&self, slot_id: &str, path: &Path) -> Result<Option<String>>;
}

// ── CwdProvider ──────────────────────────────────────────────────────────────

/// No-op provider: hands back the project root unchanged. Suitable for:
///
/// - Read-only slots (plan/explore/verify personas)
/// - Trust-mode `Auto` on non-git projects
/// - Any scenario where copy-on-write isn't worth the latency
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
        Ok(None)
    }
}

// ── GitWorktreeProvider ───────────────────────────────────────────────────────

/// Isolated workspace via `git worktree add`.
///
/// Each slot gets its own branch (`koda/wt/<agent>-<short-id>`) and a
/// matching worktree directory at `.koda/worktrees/<slot_id>`.
///
/// ## Release behaviour
///
/// | Worktree state | Action                                               |
/// |----------------|------------------------------------------------------|
/// | Clean          | `git worktree remove --force`; delete ephemeral branch |
/// | Dirty          | `git add -A && git commit`; `git worktree remove --force`; **keep branch**; return hint |
///
/// The branch is the permanent record of what the sub-agent did. Users
/// can inspect, merge, or discard it at their discretion:
///
/// ```text
/// Review:  git diff main...koda/wt/<agent>-<id>
/// Merge:   git merge koda/wt/<agent>-<id>
/// Discard: git branch -D koda/wt/<agent>-<id>
/// ```
///
/// ## Fallback
///
/// If `git` is not in `PATH` or the project is not a git repo,
/// `provision` returns the `project_root` unchanged and `release` is a
/// no-op. No error is surfaced — the sub-agent just runs without
/// worktree isolation.
#[derive(Debug, Clone)]
pub struct GitWorktreeProvider {
    project_root: PathBuf,
    agent_name: String,
}

impl GitWorktreeProvider {
    /// Create a provider that will issue worktrees under `project_root`.
    ///
    /// `agent_name` is used for the human-readable branch prefix.
    pub fn new(project_root: impl Into<PathBuf>, agent_name: impl Into<String>) -> Self {
        Self {
            project_root: project_root.into(),
            agent_name: agent_name.into(),
        }
    }

    // ── helpers ──────────────────────────────────────────────────────────────

    /// Sanitise `agent_name` for use in a git branch segment.
    ///
    /// Git branch names can contain most characters but not spaces,
    /// `~`, `^`, `:`, `?`, `*`, `\`, `..`, or leading `-`. We replace
    /// anything outside `[a-zA-Z0-9._-]` with `-` and strip leading `-`.
    fn safe_agent(&self) -> String {
        let sanitised: String = self
            .agent_name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let trimmed = sanitised.trim_matches('-');
        if trimmed.is_empty() {
            "agent".to_string()
        } else {
            // Cap at 30 chars so the full branch name stays reasonable.
            trimmed.chars().take(30).collect()
        }
    }

    /// Build the branch name for a given slot.
    fn branch_name(&self, slot_id: &str) -> String {
        let short = &slot_id[..slot_id.len().min(8)];
        format!("koda/wt/{}-{short}", self.safe_agent())
    }

    /// `true` when git is reachable and the project is inside a git repo.
    async fn is_git_repo(&self) -> bool {
        let Ok(out) = Command::new("git")
            .args(["rev-parse", "--is-inside-work-tree"])
            .current_dir(&self.project_root)
            .output()
            .await
        else {
            return false;
        };
        out.status.success()
    }

    /// Path of the worktree directory for a given slot.
    fn worktree_path(&self, slot_id: &str) -> PathBuf {
        self.project_root
            .join(".koda")
            .join("worktrees")
            .join(slot_id)
    }
}

#[async_trait]
impl WorkspaceProvider for GitWorktreeProvider {
    async fn provision(&self, slot_id: &str) -> Result<PathBuf> {
        // Reject slot_ids that could escape the worktrees directory.
        if slot_id.is_empty() || slot_id.contains('/') || slot_id.contains('\\') {
            anyhow::bail!("Invalid slot_id for worktree: {slot_id:?}");
        }

        if !self.is_git_repo().await {
            tracing::debug!(
                "Not a git repo or git unavailable — skipping worktree isolation for {slot_id}"
            );
            return Ok(self.project_root.clone());
        }

        let wt_path = self.worktree_path(slot_id);
        let branch = self.branch_name(slot_id);

        // Reuse on resume (idempotent).
        if wt_path.exists() {
            tracing::debug!("Reusing existing worktree: {}", wt_path.display());
            return Ok(wt_path);
        }

        std::fs::create_dir_all(wt_path.parent().unwrap_or(&self.project_root))
            .context("Failed to create .koda/worktrees/ directory")?;

        let out = Command::new("git")
            .args([
                "worktree",
                "add",
                "-b",
                &branch,
                &wt_path.to_string_lossy(),
                "HEAD",
            ])
            .current_dir(&self.project_root)
            .output()
            .await
            .context("Failed to spawn git worktree add")?;

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            anyhow::bail!("git worktree add failed: {stderr}");
        }

        tracing::info!(
            "Provisioned worktree {} (branch {branch})",
            wt_path.display()
        );
        Ok(wt_path)
    }

    async fn release(&self, slot_id: &str, path: &Path) -> Result<Option<String>> {
        // Fallback path — provision returned project_root directly.
        if path == self.project_root {
            return Ok(None);
        }

        if !path.exists() {
            return Ok(None);
        }

        let branch = self.branch_name(slot_id);

        // Check for uncommitted changes.
        let status = Command::new("git")
            .args(["status", "--short"])
            .current_dir(path)
            .output()
            .await
            .context("Failed to run git status in worktree")?;
        let is_dirty = !String::from_utf8_lossy(&status.stdout).trim().is_empty();

        if is_dirty {
            // Auto-commit so the work is preserved on the branch.
            Command::new("git")
                .args(["add", "-A"])
                .current_dir(path)
                .output()
                .await
                .context("git add -A in worktree")?;

            let commit_msg = format!("koda: sub-agent '{}' changes", self.agent_name);
            let committed = Command::new("git")
                .args([
                    "-c",
                    "user.name=koda",
                    "-c",
                    "user.email=koda@localhost",
                    "commit",
                    "-m",
                    &commit_msg,
                ])
                .current_dir(path)
                .output()
                .await
                .context("git commit in worktree")?;

            if !committed.status.success() {
                let stderr = String::from_utf8_lossy(&committed.stderr);
                tracing::warn!("Worktree auto-commit failed: {stderr}");
            }

            // Remove the worktree dir but keep the branch.
            self.remove_worktree(path).await;

            let hint = format!(
                "🌿 Sub-agent '{}' left changes on branch {branch}\n\
                 Review:  git diff HEAD...{branch}\n\
                 Merge:   git merge {branch}\n\
                 Discard: git branch -D {branch}",
                self.agent_name
            );
            tracing::info!("Worktree committed to branch {branch}");
            return Ok(Some(hint));
        }

        // Clean — remove worktree and ephemeral branch.
        self.remove_worktree(path).await;
        let _ = Command::new("git")
            .args(["branch", "-D", &branch])
            .current_dir(&self.project_root)
            .output()
            .await;

        tracing::info!("Removed clean worktree for slot {slot_id}");
        Ok(None)
    }
}

impl GitWorktreeProvider {
    /// Best-effort `git worktree remove --force` with rm-rf fallback.
    async fn remove_worktree(&self, path: &Path) {
        let out = Command::new("git")
            .args(["worktree", "remove", "--force", &path.to_string_lossy()])
            .current_dir(&self.project_root)
            .output()
            .await;

        match out {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                tracing::warn!("git worktree remove failed ({stderr}), falling back to rm -rf");
                let _ = tokio::fs::remove_dir_all(path).await;
            }
            Err(e) => {
                tracing::warn!("Could not spawn git worktree remove: {e}");
                let _ = tokio::fs::remove_dir_all(path).await;
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── CwdProvider ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn cwd_provider_returns_root_unchanged() {
        let root = PathBuf::from("/tmp/some-project");
        let p = CwdProvider::new(&root);
        assert_eq!(p.provision("slot-1").await.unwrap(), root);
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
        let p = CwdProvider::new("/tmp/x");
        let a = p.provision("slot-1").await.unwrap();
        let b = p.provision("slot-1").await.unwrap();
        assert_eq!(a, b);
    }

    // ── GitWorktreeProvider helpers ───────────────────────────────────────────

    #[test]
    fn safe_agent_strips_bad_chars() {
        let p = GitWorktreeProvider::new("/tmp/proj", "my agent/name!");
        // spaces, slashes, and punctuation become hyphens; trailing hyphens stripped
        assert_eq!(p.safe_agent(), "my-agent-name");
        // all-punctuation collapses to the fallback
        let p2 = GitWorktreeProvider::new("/tmp/proj", "!!!");
        assert_eq!(p2.safe_agent(), "agent");
    }

    #[test]
    fn branch_name_is_readable() {
        let p = GitWorktreeProvider::new("/tmp/proj", "refactor");
        let b = p.branch_name("abcdef1234567890");
        assert_eq!(b, "koda/wt/refactor-abcdef12");
    }

    #[test]
    fn invalid_slot_id_rejected() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let p = GitWorktreeProvider::new("/tmp/x", "agent");
        rt.block_on(async {
            assert!(p.provision("").await.is_err());
            assert!(p.provision("foo/bar").await.is_err());
            assert!(p.provision("foo\\bar").await.is_err());
        });
    }

    // ── GitWorktreeProvider end-to-end (requires git in PATH) ────────────────

    async fn init_repo(path: &Path) {
        for args in [
            vec!["init"],
            vec![
                "-c",
                "user.name=test",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ],
        ] {
            Command::new("git")
                .args(&args)
                .current_dir(path)
                .output()
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn provision_not_git_repo_falls_back() {
        let tmp = tempfile::tempdir().unwrap();
        let p = GitWorktreeProvider::new(tmp.path(), "agent");
        let result = p.provision("slot-abc").await.unwrap();
        assert_eq!(result, tmp.path());
    }

    #[tokio::test]
    async fn provision_in_git_repo() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path()).await;

        let p = GitWorktreeProvider::new(tmp.path(), "my-agent");
        let wt = p.provision("slot-001").await.unwrap();

        assert!(wt.exists());
        assert!(wt.ends_with("slot-001"));
        // Should have a git HEAD inside the worktree
        assert!(
            wt.join(".git").exists() || wt.join("HEAD").exists() || {
                // git ≥2.5 worktrees have a .git file (not dir) pointing at main
                std::fs::read_to_string(wt.join(".git"))
                    .map(|s| s.contains("gitdir"))
                    .unwrap_or(false)
            }
        );
    }

    #[tokio::test]
    async fn provision_reuses_existing_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path()).await;

        let p = GitWorktreeProvider::new(tmp.path(), "agent");
        let a = p.provision("slot-reuse").await.unwrap();
        let b = p.provision("slot-reuse").await.unwrap();
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn release_clean_worktree_removes_it() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path()).await;

        let p = GitWorktreeProvider::new(tmp.path(), "agent");
        let wt = p.provision("slot-clean").await.unwrap();
        assert!(wt.exists());

        let hint = p.release("slot-clean", &wt).await.unwrap();
        assert!(hint.is_none(), "clean worktree should leave no hint");
        assert!(!wt.exists(), "clean worktree dir should be removed");
    }

    #[tokio::test]
    async fn release_dirty_worktree_commits_and_hints() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path()).await;

        let p = GitWorktreeProvider::new(tmp.path(), "refactor");
        let wt = p.provision("slot-dirty").await.unwrap();

        // Write a file so the worktree is dirty.
        std::fs::write(wt.join("output.rs"), "// generated").unwrap();

        let hint = p.release("slot-dirty", &wt).await.unwrap();
        let hint = hint.expect("dirty release must return a hint");

        // Worktree dir is gone…
        assert!(!wt.exists(), "worktree dir should be removed after commit");
        // …but the branch exists and the hint is well-formed.
        let branch = "koda/wt/refactor-slot-dir"; // first 8 chars of "slot-dirty"
        assert!(hint.contains(branch), "{hint}");
        assert!(hint.contains("git diff HEAD"), "{hint}");
        assert!(hint.contains("git merge"), "{hint}");
    }

    #[tokio::test]
    async fn release_fallback_path_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let p = GitWorktreeProvider::new(tmp.path(), "agent");
        // path == project_root → must be no-op even if not a git repo
        let hint = p.release("slot-x", tmp.path()).await.unwrap();
        assert!(hint.is_none());
    }
}
