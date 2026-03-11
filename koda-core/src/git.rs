//! Git integration for context injection.
//!
//! - `git_context()`: compact git info for the system prompt
//!
//! File-level undo is handled by `undo.rs` (in-memory snapshots),
//! not git. See DESIGN.md for rationale.

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

}
