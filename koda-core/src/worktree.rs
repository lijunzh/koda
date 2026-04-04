//! Git worktree provisioning for sub-agent isolation.
//!
//! Each sub-agent that can write files gets its own git worktree,
//! preventing parallel agents from corrupting each other's working tree.
//!
//! ## Lifecycle
//!
//! 1. `provision()` — `git worktree add .koda/worktrees/<session-id> HEAD`
//! 2. Sub-agent runs with `project_root = worktree_path`
//! 3. `cleanup()` — if worktree is clean, remove it; if dirty, leave for review
//!
//! ## Requirements
//!
//! - Project must be a git repo (graceful fallback if not)
//! - `git` must be in PATH

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tracing;

/// Result of attempting to provision a worktree.
#[derive(Debug)]
pub enum WorktreeResult {
    /// Worktree created successfully at this path.
    Created(PathBuf),
    /// Not a git repo — sub-agent will share the parent's project_root.
    NotGitRepo,
    /// Git is not available.
    NoGit,
}

/// Provision a git worktree for a sub-agent session.
///
/// Creates `.koda/worktrees/<session_id>` checked out at HEAD.
/// Returns `WorktreeResult::NotGitRepo` if the project isn't a git repo.
pub async fn provision(project_root: &Path, session_id: &str) -> Result<WorktreeResult> {
    // Validate session_id is safe for use as a directory name
    if session_id.is_empty()
        || session_id.contains('/')
        || session_id.contains('\\')
        || session_id.contains("..")
    {
        anyhow::bail!("Invalid session ID for worktree: {session_id}");
    }

    // Check if git is available
    let git_check = Command::new("git")
        .arg("--version")
        .current_dir(project_root)
        .output()
        .await;

    if git_check.is_err() {
        tracing::debug!("git not found in PATH, skipping worktree isolation");
        return Ok(WorktreeResult::NoGit);
    }

    // Check if we're in a git repo
    let is_git = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(project_root)
        .output()
        .await?;

    if !is_git.status.success() {
        tracing::debug!("Not a git repo, skipping worktree isolation");
        return Ok(WorktreeResult::NotGitRepo);
    }

    let worktree_dir = project_root.join(".koda").join("worktrees");
    std::fs::create_dir_all(&worktree_dir)
        .with_context(|| format!("Failed to create worktree dir: {}", worktree_dir.display()))?;

    let worktree_path = worktree_dir.join(session_id);

    // If already exists (e.g. resumed session), reuse it
    if worktree_path.exists() {
        tracing::debug!("Reusing existing worktree: {}", worktree_path.display());
        return Ok(WorktreeResult::Created(worktree_path));
    }

    let output = Command::new("git")
        .args([
            "worktree",
            "add",
            "--detach", // detached HEAD, no new branch
            &worktree_path.to_string_lossy(),
            "HEAD",
        ])
        .current_dir(project_root)
        .output()
        .await
        .context("Failed to run git worktree add")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git worktree add failed: {stderr}");
    }

    tracing::info!("Provisioned worktree: {}", worktree_path.display());
    Ok(WorktreeResult::Created(worktree_path))
}

/// Clean up a worktree after a sub-agent completes.
///
/// - If the worktree has no changes → remove it
/// - If the worktree has changes → leave it and return the diff summary
pub async fn cleanup(project_root: &Path, worktree_path: &Path) -> Result<Option<String>> {
    if !worktree_path.exists() {
        return Ok(None);
    }

    // Check if worktree has changes
    let status = Command::new("git")
        .args(["status", "--short"])
        .current_dir(worktree_path)
        .output()
        .await
        .context("Failed to check worktree status")?;

    let status_output = String::from_utf8_lossy(&status.stdout);

    if status_output.trim().is_empty() {
        // Clean — remove it
        let remove = Command::new("git")
            .args([
                "worktree",
                "remove",
                "--force",
                &worktree_path.to_string_lossy(),
            ])
            .current_dir(project_root)
            .output()
            .await
            .context("Failed to remove worktree")?;

        if remove.status.success() {
            tracing::info!("Removed clean worktree: {}", worktree_path.display());
        } else {
            // Best-effort: if git worktree remove fails, try rm -rf
            let stderr = String::from_utf8_lossy(&remove.stderr);
            tracing::warn!("git worktree remove failed ({stderr}), trying rm -rf");
            let _ = tokio::fs::remove_dir_all(worktree_path).await;
        }
        Ok(None)
    } else {
        // Dirty — leave it, return summary
        let diff = Command::new("git")
            .args(["diff", "--stat"])
            .current_dir(worktree_path)
            .output()
            .await
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();

        let summary = format!(
            "Worktree has uncommitted changes:\n{}\n\
             Review: git -C {} diff\n\
             Apply:  git -C {} stash && git stash pop",
            diff.trim(),
            worktree_path.display(),
            worktree_path.display(),
        );

        tracing::info!("Worktree has changes, keeping: {}", worktree_path.display());
        Ok(Some(summary))
    }
}

/// Remove all clean worktrees in `.koda/worktrees/`.
///
/// Returns the count of removed vs kept worktrees.
pub async fn prune(project_root: &Path) -> Result<(usize, usize)> {
    let worktree_dir = project_root.join(".koda").join("worktrees");
    if !worktree_dir.exists() {
        return Ok((0, 0));
    }

    let mut removed = 0;
    let mut kept = 0;

    let mut entries = tokio::fs::read_dir(&worktree_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        match cleanup(project_root, &path).await? {
            None => removed += 1,
            Some(_) => kept += 1,
        }
    }

    Ok((removed, kept))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn provision_not_git_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let result = provision(tmp.path(), "test-session").await.unwrap();
        assert!(matches!(result, WorktreeResult::NotGitRepo));
    }

    #[tokio::test]
    async fn provision_in_git_repo() {
        let tmp = tempfile::tempdir().unwrap();
        // Init a git repo with an initial commit
        Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["commit", "--allow-empty", "-m", "init"])
            .current_dir(tmp.path())
            .output()
            .await
            .unwrap();

        let result = provision(tmp.path(), "test-session-123").await.unwrap();
        match result {
            WorktreeResult::Created(path) => {
                assert!(path.exists());
                assert!(path.ends_with("test-session-123"));
            }
            other => panic!("Expected Created, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn provision_reuses_existing() {
        let tmp = tempfile::tempdir().unwrap();
        Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["commit", "--allow-empty", "-m", "init"])
            .current_dir(tmp.path())
            .output()
            .await
            .unwrap();

        let r1 = provision(tmp.path(), "reuse-test").await.unwrap();
        let r2 = provision(tmp.path(), "reuse-test").await.unwrap();
        match (r1, r2) {
            (WorktreeResult::Created(p1), WorktreeResult::Created(p2)) => {
                assert_eq!(p1, p2);
            }
            _ => panic!("Expected both to be Created"),
        }
    }

    #[tokio::test]
    async fn cleanup_removes_clean_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["commit", "--allow-empty", "-m", "init"])
            .current_dir(tmp.path())
            .output()
            .await
            .unwrap();

        let result = provision(tmp.path(), "clean-wt").await.unwrap();
        let wt_path = match result {
            WorktreeResult::Created(p) => p,
            _ => panic!("Expected Created"),
        };

        let changes = cleanup(tmp.path(), &wt_path).await.unwrap();
        assert!(changes.is_none(), "Clean worktree should have no changes");
        assert!(!wt_path.exists(), "Clean worktree should be removed");
    }

    #[tokio::test]
    async fn cleanup_keeps_dirty_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["commit", "--allow-empty", "-m", "init"])
            .current_dir(tmp.path())
            .output()
            .await
            .unwrap();

        let result = provision(tmp.path(), "dirty-wt").await.unwrap();
        let wt_path = match result {
            WorktreeResult::Created(p) => p,
            _ => panic!("Expected Created"),
        };

        // Create a dirty file
        std::fs::write(wt_path.join("dirty.txt"), "changes").unwrap();

        let changes = cleanup(tmp.path(), &wt_path).await.unwrap();
        assert!(changes.is_some(), "Dirty worktree should report changes");
        assert!(wt_path.exists(), "Dirty worktree should be kept");
    }

    #[tokio::test]
    async fn invalid_session_id_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(provision(tmp.path(), "../escape").await.is_err());
        assert!(provision(tmp.path(), "").await.is_err());
        assert!(provision(tmp.path(), "foo/bar").await.is_err());
    }
}
