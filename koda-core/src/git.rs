//! Git integration for context injection.
//!
//! Provides a compact snapshot of the current git state for injection into
//! the system prompt. This gives the model awareness of:
//!
//! - **Current branch** — so it knows where it's working
//! - **Staged changes** — diff stat showing what's ready to commit
//! - **Unstaged changes** — diff stat showing what's modified but not staged
//! - **Recent commits** — last N commit subjects for historical context
//!
//! ## What this module does NOT do
//!
//! - **Git operations** — commits, pushes, etc. are done via the Bash tool
//! - **Worktree management** — handled by [`koda_sandbox::GitWorktreeProvider`]
//!
//! ## Output format
//!
//! ```text
//! [Git: branch=main
//!  Staged: 2 files changed, 15 insertions(+), 3 deletions(-)
//!  Unstaged: 1 file changed, 4 insertions(+)
//!  Recent: fix: align arrows | docs: enrich modules | feat: add AskUser]
//! ```
//!
//! Diff stats are truncated at 2KB to avoid bloating the system prompt.

use std::path::Path;
use std::sync::OnceLock;

use koda_git::{DIFF_ARGS, Operation};

// ── Context injection (#263) ────────────────────────────────────

/// Maximum characters for the diff stat section.
const MAX_DIFF_STAT_CHARS: usize = 2_000;
/// Maximum recent commits to include.
const MAX_RECENT_COMMITS: usize = 5;

/// The session's git context, taken once and kept.
///
/// The system prompt sits at the front of every request, so anything that
/// changes in it invalidates the provider's prompt cache for the whole
/// conversation behind it. Recomputing the diff stat each iteration did that
/// after every edit. The model runs git when it needs the current state.
#[derive(Debug, Default)]
pub struct GitSnapshot {
    text: OnceLock<String>,
    /// Add Koda's commit and pull request attribution
    /// ([`ATTRIBUTION_INSTRUCTIONS`]).
    attribution: bool,
}

impl GitSnapshot {
    /// A snapshot, with the attribution instructions when `attribution`.
    pub fn new(attribution: bool) -> Self {
        Self {
            text: OnceLock::new(),
            attribution,
        }
    }

    /// The text to append to the system prompt; empty outside a git repo.
    pub fn line(&self, project_root: &Path) -> &str {
        self.text.get_or_init(|| {
            git_context(project_root)
                .map(|ctx| {
                    let attribution = if self.attribution {
                        ATTRIBUTION_INSTRUCTIONS
                    } else {
                        NO_ATTRIBUTION_INSTRUCTIONS
                    };
                    format!("\n{ctx}\n{attribution}")
                })
                .unwrap_or_default()
        })
    }
}

/// The trailer Koda's commits end with, in Codex's form (`Co-authored-by:
/// Codex <noreply@openai.com>`).
pub const COMMIT_TRAILER: &str = "Co-authored-by: Koda <noreply@lijunzhu.info>";

/// The line Koda's pull requests carry, in Codex's form.
pub const PR_LINE: &str = "Generated with [Koda](https://github.com/lijunzh/koda).";

/// Codex's attribution instructions (`ext/git-attribution`), with Koda's
/// trailer and pull request line.
pub const ATTRIBUTION_INSTRUCTIONS: &str = "\
When you create or amend a git commit or create or update a pull request for this task, add Koda attribution exactly once:
- Commit messages must end with `Co-authored-by: Koda <noreply@lijunzhu.info>`. Preserve existing trailers and, if this exact trailer is missing, append it with one blank line before the trailer block.
- Pull request bodies must include the exact line `Generated with [Koda](https://github.com/lijunzh/koda).`. Preserve the existing body and, if this exact line is missing, append it near the end.
- Do not add duplicate attribution. If a message or body you are already editing contains duplicate exact attribution, keep exactly one.
- Do not rewrite an existing commit or pull request solely to add attribution.";

/// What the prompt says when `commit_attribution = false`, as Codex's
/// disabled policy says it.
pub const NO_ATTRIBUTION_INSTRUCTIONS: &str = "\
Koda commit and pull request attribution is disabled for this project. Do not add it.";

/// Compact git context for injection into the system prompt.
///
/// Returns `None` if not in a git repo. Includes:
/// - Current branch name
/// - Staged diff stat (truncated)
/// - Unstaged diff stat (truncated)
/// - Last N commit subjects
pub fn git_context(project_root: &Path) -> Option<String> {
    let branch = git_cmd(
        project_root,
        Operation::Metadata,
        &["rev-parse", "--abbrev-ref", "HEAD"],
    )?;

    let mut parts = vec![format!(
        "[Git snapshot from session start; run git for current state: branch={branch}"
    )];

    // Staged changes (stat only — token-efficient)
    if let Some(staged) = git_cmd(
        project_root,
        Operation::Metadata,
        &["diff", "--cached", "--stat", DIFF_ARGS[0], DIFF_ARGS[1]],
    ) && !staged.trim().is_empty()
    {
        let truncated = truncate_str(&staged, MAX_DIFF_STAT_CHARS);
        parts.push(format!("staged:\n{truncated}"));
    }

    // Unstaged changes (stat only)
    if let Some(unstaged) = git_cmd(
        project_root,
        Operation::WorkingTree,
        &["diff", "--stat", DIFF_ARGS[0], DIFF_ARGS[1]],
    ) && !unstaged.trim().is_empty()
    {
        let truncated = truncate_str(&unstaged, MAX_DIFF_STAT_CHARS);
        parts.push(format!("unstaged:\n{truncated}"));
    }

    // Untracked file count
    if let Some(untracked) = git_cmd(
        project_root,
        Operation::WorkingTree,
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
        Operation::Metadata,
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

/// Run a git command and return stdout if successful. It runs outside the
/// sandbox on a project Koda didn't vet, so through [`koda_git`]: the
/// repository's config can't make it run a program.
fn git_cmd(cwd: &Path, operation: Operation, args: &[&str]) -> Option<String> {
    koda_git::command(cwd, operation)
        .ok()?
        .args(args)
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
    let max = s.floor_char_boundary(max);
    let end = s[..max].rfind('\n').unwrap_or(max);
    let truncated = &s[..end];
    let remaining = s[end..].lines().count();
    format!("{truncated}\n  ... ({remaining} more lines)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_str_cut_inside_a_character_does_not_panic() {
        let text = "é\n".repeat(50);
        for max in 1..12 {
            assert!(truncate_str(&text, max).contains("more lines"));
        }
    }

    #[test]
    fn test_git_context_in_repo() {
        // We're running tests inside the koda repo, so this should work
        let ctx = git_context(Path::new("."));
        assert!(ctx.is_some());
        let ctx = ctx.unwrap();
        assert!(ctx.contains("branch="));
        assert!(ctx.contains("snapshot from session start"));
        assert!(ctx.contains("recent commits:"));
    }

    #[test]
    fn snapshot_stays_the_same_after_the_tree_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .current_dir(root)
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "start"]);

        let snapshot = GitSnapshot::default();
        let before = snapshot.line(root).to_string();
        std::fs::write(root.join("a.txt"), "two\nthree\n").unwrap();

        assert_ne!(
            before.trim_start(),
            git_context(root).unwrap(),
            "the live context must have moved for this test to mean anything"
        );
        assert_eq!(snapshot.line(root), before);
    }

    #[test]
    fn the_snapshot_carries_the_attribution_instructions_inside_a_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for args in [
            vec!["init", "-q"],
            vec![
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "start",
            ],
        ] {
            let ok = std::process::Command::new("git")
                .args(&args)
                .current_dir(root)
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        }
        let on = GitSnapshot::new(true);
        assert!(on.line(root).contains(COMMIT_TRAILER), "{}", on.line(root));
        assert!(on.line(root).contains(PR_LINE));
        let off = GitSnapshot::new(false);
        assert!(!off.line(root).contains(COMMIT_TRAILER));
        assert!(off.line(root).contains("attribution is disabled"));
        // The instructions name exactly the trailer and line Koda uses.
        assert!(ATTRIBUTION_INSTRUCTIONS.contains(&format!("`{COMMIT_TRAILER}`")));
        assert!(ATTRIBUTION_INSTRUCTIONS.contains(&format!("`{PR_LINE}`")));
        // Outside a repository: nothing.
        let elsewhere = tempfile::tempdir().unwrap();
        assert_eq!(GitSnapshot::new(true).line(elsewhere.path()), "");
    }

    #[test]
    fn snapshot_outside_a_repo_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(GitSnapshot::default().line(tmp.path()), "");
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
