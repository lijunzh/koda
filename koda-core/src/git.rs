//! Git integration for context injection and checkpointing.
//!
//! - `git_context()`: compact git info for the system prompt
//! - `checkpoint()` / `rollback()`: crash-safe undo via git stash

use std::path::Path;
use std::process::Command;

// ── Context injection (#263) ────────────────────────────────────

/// Maximum characters for the diff stat section.
const MAX_DIFF_STAT_CHARS: usize = 2_000;
/// Maximum recent commits to include.
const MAX_RECENT_COMMITS: usize = 5;

/// Compact git context for injection into the system prompt.
///
/// Returns `None` if not in a git repo. Includes:
/// - Current branch name
/// - Staged diff stat (truncated)
/// - Unstaged diff stat (truncated)
/// - Last N commit subjects
pub fn git_context(project_root: &Path) -> Option<String> {
    let branch = git_cmd(project_root, &["rev-parse", "--abbrev-ref", "HEAD"])?;

    let mut parts = vec![format!("[Git: branch={branch}")];

    // Staged changes (stat only — token-efficient)
    if let Some(staged) = git_cmd(project_root, &["diff", "--cached", "--stat"])
        && !staged.trim().is_empty()
    {
        let truncated = truncate_str(&staged, MAX_DIFF_STAT_CHARS);
        parts.push(format!("staged:\n{truncated}"));
    }

    // Unstaged changes (stat only)
    if let Some(unstaged) = git_cmd(project_root, &["diff", "--stat"])
        && !unstaged.trim().is_empty()
    {
        let truncated = truncate_str(&unstaged, MAX_DIFF_STAT_CHARS);
        parts.push(format!("unstaged:\n{truncated}"));
    }

    // Untracked file count
    if let Some(untracked) = git_cmd(
        project_root,
        &["ls-files", "--others", "--exclude-standard"],
    ) {
        let count = untracked.lines().count();
        if count > 0 {
            parts.push(format!("{count} untracked file(s)"));
        }
    }

    // Recent commits
    if let Some(log) = git_cmd(
        project_root,
        &[
            "log",
            "--oneline",
            &format!("-{MAX_RECENT_COMMITS}"),
            "--no-decorate",
        ],
    ) && !log.trim().is_empty()
    {
        parts.push(format!("recent commits:\n{log}"));
    }

    parts.push("]".to_string());
    Some(parts.join(", "))
}

// ── Checkpointing (#264) ────────────────────────────────────────

/// Create a lightweight git snapshot of the working tree.
///
/// Uses `git stash create` which creates a stash commit without
/// modifying the working tree or stash list. Returns the stash
/// commit SHA, or `None` if there's nothing to snapshot.
pub fn checkpoint(project_root: &Path) -> Option<String> {
    // First check if we're in a git repo
    git_cmd(project_root, &["rev-parse", "--git-dir"])?;

    // git stash create: makes a commit object but doesn't modify state
    let sha = git_cmd(project_root, &["stash", "create"])?;
    let sha = sha.trim().to_string();

    if sha.is_empty() {
        // Nothing to stash (clean working tree)
        return None;
    }

    // Store the ref so it doesn't get garbage-collected
    let _ = git_cmd(
        project_root,
        &["stash", "store", "-m", "koda checkpoint (auto)", &sha],
    );

    Some(sha)
}

/// Roll back the working tree to a checkpoint.
///
/// Restores the working tree to the checkpoint state by first
/// resetting uncommitted changes, then applying the stash.
/// Returns a summary or an error message.
pub fn rollback(project_root: &Path, sha: &str) -> Result<String, String> {
    // Verify the SHA exists
    if git_cmd(project_root, &["cat-file", "-t", sha]).is_none() {
        return Err(format!("Checkpoint {sha} not found"));
    }

    // Reset working tree to HEAD first (discard current changes)
    let reset = Command::new("git")
        .args(["checkout", "."])
        .current_dir(project_root)
        .output();
    if let Ok(o) = &reset
        && !o.status.success()
    {
        let stderr = String::from_utf8_lossy(&o.stderr);
        return Err(format!("Failed to reset working tree: {stderr}"));
    }

    // Apply the stash
    let result = Command::new("git")
        .args(["stash", "apply", sha])
        .current_dir(project_root)
        .output();

    match result {
        Ok(output) if output.status.success() => Ok("Restored to checkpoint.".to_string()),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            Err(format!("Rollback failed: {stderr}"))
        }
        Err(e) => Err(format!("Failed to run git: {e}")),
    }
}

// ── Helpers ─────────────────────────────────────────────────────

/// Run a git command and return stdout if successful.
fn git_cmd(cwd: &Path, args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
}

/// Truncate a string to max chars at a line boundary.
fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    // Find last newline before max
    let end = s[..max].rfind('\n').unwrap_or(max);
    let truncated = &s[..end];
    let remaining = s[end..].lines().count();
    format!("{truncated}\n  ... ({remaining} more lines)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_git_context_in_repo() {
        // We're running tests inside the koda repo, so this should work
        let ctx = git_context(Path::new("."));
        assert!(ctx.is_some());
        let ctx = ctx.unwrap();
        assert!(ctx.contains("[Git: branch="));
        assert!(ctx.contains("recent commits:"));
    }

    #[test]
    fn test_git_context_not_a_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = git_context(tmp.path());
        assert!(ctx.is_none());
    }

    #[test]
    fn test_truncate_str_short() {
        assert_eq!(truncate_str("hello", 100), "hello");
    }

    #[test]
    fn test_truncate_str_long() {
        let lines: Vec<String> = (0..50).map(|i| format!("line {i}")).collect();
        let input = lines.join("\n");
        let truncated = truncate_str(&input, 50);
        assert!(truncated.len() <= 80); // 50 + "... (N more lines)"
        assert!(truncated.contains("more lines"));
    }

    #[test]
    fn test_checkpoint_in_clean_repo() {
        // In CI or clean working tree, checkpoint returns None
        // (nothing to stash). This is correct behavior.
        let result = checkpoint(Path::new("."));
        // We can't assert None because the working tree might be dirty
        // during development. Just assert it doesn't panic.
        let _ = result;
    }

    #[test]
    fn test_checkpoint_not_a_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let result = checkpoint(tmp.path());
        assert!(result.is_none());
    }

    #[test]
    fn test_rollback_bad_sha() {
        let result = rollback(Path::new("."), "deadbeef1234567890");
        assert!(result.is_err());
    }

    #[test]
    fn test_checkpoint_and_rollback_cycle() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Init a git repo
        git_cmd(root, &["init"]).unwrap();
        git_cmd(root, &["config", "user.email", "test@test.com"]).unwrap();
        git_cmd(root, &["config", "user.name", "Test"]).unwrap();

        // Create and commit a file
        std::fs::write(root.join("file.txt"), "original").unwrap();
        git_cmd(root, &["add", "."]).unwrap();
        git_cmd(root, &["commit", "-m", "initial"]).unwrap();

        // Modify the file
        std::fs::write(root.join("file.txt"), "modified").unwrap();

        // Checkpoint
        let sha = checkpoint(root);
        assert!(sha.is_some(), "Should create checkpoint for dirty tree");
        let sha = sha.unwrap();

        // File is still modified (stash create doesn't touch working tree)
        let content = std::fs::read_to_string(root.join("file.txt")).unwrap();
        assert_eq!(content, "modified");

        // Make another change
        std::fs::write(root.join("file.txt"), "further modified").unwrap();

        // Rollback to checkpoint
        let result = rollback(root, &sha);
        assert!(result.is_ok(), "Rollback failed: {:?}", result);

        // File should be back to "modified" (the checkpoint state)
        let content = std::fs::read_to_string(root.join("file.txt")).unwrap();
        assert_eq!(content, "modified");
    }
}
