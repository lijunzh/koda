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
//! | `OverlayfsProvider`   | 4     | Linux overlayfs inside bwrap         |
//!
//! The pool selects which provider to use at slot acquisition time, based
//! on agent persona + trust mode + git availability.

use anyhow::{Context, Result};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use tokio::process::Command;

/// `out` if git ran and reported success, else why not.
fn succeeded(
    step: &str,
    out: std::io::Result<std::process::Output>,
) -> std::result::Result<std::process::Output, String> {
    let out = out.map_err(|e| format!("{step}: {e}"))?;
    if out.status.success() {
        return Ok(out);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let said = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    Err(format!("{step}: {}", said.trim()))
}

/// A `git` command in `dir`, killed if its call is dropped. A half-made worktree
/// isn't cleaned up.
///
/// These run outside the sandbox on a tree the sub-agent wrote, so nothing it
/// wrote may decide what they execute. A relative `core.hooksPath` finds hooks
/// inside the worktree, and `--no-verify` only skips some hooks. Turning hooks
/// off means a project's own hooks, `post-checkout` included, don't run here.
fn git(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.args(["-c", "core.hooksPath=/dev/null"])
        .args(["-c", "core.fsmonitor=false"])
        .current_dir(dir)
        .kill_on_drop(true);
    cmd
}

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
/// | Clean, no new commits | `git worktree remove --force`; delete ephemeral branch |
/// | Clean, with commits | `git worktree remove --force`; **keep branch**; return hint |
/// | Dirty          | `git add -A && git commit`; `git worktree remove --force`; **keep branch**; return hint |
/// | Can't tell or can't save | Leave the worktree where it is; return a hint naming it |
///
/// Release isn't bounded. A slow save can outlast the turn's five seconds after
/// a cancel, which drops it partway. The worktree then stays at the path
/// announced when it was provisioned, and git's own `worktree list` names it.
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
/// ## Outside git
///
/// If `git` is not in `PATH` or the project is not a git repo, `provision`
/// fails. A caller picks this provider because it needs the isolation, so
/// handing back the project would quietly remove it.
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
        let Ok(out) = git(&self.project_root)
            .args(["rev-parse", "--is-inside-work-tree"])
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
            anyhow::bail!("{} is not a git repository", self.project_root.display());
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

        let out = git(&self.project_root)
            .args([
                "worktree",
                "add",
                "-b",
                &branch,
                &wt_path.to_string_lossy(),
                "HEAD",
            ])
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
        // Never remove the project itself.
        if path == self.project_root {
            return Ok(None);
        }

        if !path.exists() {
            return Ok(None);
        }

        let branch = self.branch_name(slot_id);

        // Removing a worktree is `--force`, so anything short of "clean" or
        // "saved" has to leave it where it is.
        let is_dirty = match self.has_changes(path).await {
            Ok(is_dirty) => is_dirty,
            Err(reason) => return Ok(Some(self.kept_hint(path, &reason))),
        };

        if is_dirty {
            // Auto-commit so the work is preserved on the branch.
            if let Err(reason) = self.save_changes(path).await {
                return Ok(Some(self.kept_hint(path, &reason)));
            }
        }

        // The sub-agent may have committed for itself, and removing the branch
        // would lose that, so a clean tree doesn't prove there's nothing to keep.
        let has_commits = match self.has_new_commits(&branch).await {
            Ok(has_commits) => has_commits,
            Err(reason) => return Ok(Some(self.kept_hint(path, &reason))),
        };

        if has_commits {
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

        // Nothing of its own — remove worktree and ephemeral branch.
        self.remove_worktree(path).await;
        let _ = git(&self.project_root)
            .args(["branch", "-D", &branch])
            .output()
            .await;

        tracing::info!("Removed clean worktree for slot {slot_id}");
        Ok(None)
    }
}

impl GitWorktreeProvider {
    /// Whether the worktree holds anything to save.
    async fn has_changes(&self, path: &Path) -> std::result::Result<bool, String> {
        let status = git(path).args(["status", "--short"]).output().await;
        let status = succeeded("git status", status)?;
        Ok(!String::from_utf8_lossy(&status.stdout).trim().is_empty())
    }

    /// Whether `branch` holds commits the project's `HEAD` doesn't.
    async fn has_new_commits(&self, branch: &str) -> std::result::Result<bool, String> {
        let commits = git(&self.project_root)
            .args(["rev-list", "--max-count=1", &format!("HEAD..{branch}")])
            .output()
            .await;
        let commits = succeeded("git rev-list", commits)?;
        Ok(!commits.stdout.is_empty())
    }

    /// Commit everything in the worktree to its branch. This is Koda's own save
    /// point, so the project's signing doesn't get a say, and [`git`] has already
    /// turned off its hooks.
    async fn save_changes(&self, path: &Path) -> std::result::Result<(), String> {
        succeeded("git add", git(path).args(["add", "-A"]).output().await)?;
        let commit_msg = format!("koda: sub-agent '{}' changes", self.agent_name);
        let committed = git(path)
            .args([
                "-c",
                "user.name=koda",
                "-c",
                "user.email=koda@localhost",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                &commit_msg,
            ])
            .output()
            .await;
        succeeded("git commit", committed).map(|_| ())
    }

    /// What to tell the user about a worktree left in place.
    fn kept_hint(&self, path: &Path, reason: &str) -> String {
        tracing::warn!("Kept worktree {}: {reason}", path.display());
        let path = path.display();
        format!(
            "⚠️ Sub-agent '{}' left work that was not committed ({reason}). It's still in {path}\n\
             Save it there, then remove it: git worktree remove --force {path}",
            self.agent_name
        )
    }

    /// Best-effort `git worktree remove --force` with rm-rf fallback.
    async fn remove_worktree(&self, path: &Path) {
        let out = git(&self.project_root)
            .args(["worktree", "remove", "--force", &path.to_string_lossy()])
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
    async fn a_commit_the_sub_agent_made_keeps_its_branch() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path()).await;

        let p = GitWorktreeProvider::new(tmp.path(), "committer");
        let wt = p.provision("slot-commit").await.unwrap();
        std::fs::write(wt.join("done.txt"), "done").unwrap();
        for args in [
            &["add", "-A"][..],
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "x",
            ][..],
        ] {
            let out = git(&wt).args(args).output().await.unwrap();
            assert!(out.status.success(), "{out:?}");
        }

        let hint = p.release("slot-commit", &wt).await.unwrap();

        let branch = p.branch_name("slot-commit");
        let hint = hint.expect("committed work must be reported");
        assert!(hint.contains(&branch), "{hint}");
        assert!(!wt.exists(), "the branch holds the work now");
        let shown = git(tmp.path())
            .args(["show", &format!("{branch}:done.txt")])
            .output()
            .await
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&shown.stdout), "done");
    }

    #[tokio::test]
    async fn a_branch_with_nothing_new_is_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path()).await;

        let p = GitWorktreeProvider::new(tmp.path(), "idle");
        let wt = p.provision("slot-idle").await.unwrap();
        assert!(p.release("slot-idle", &wt).await.unwrap().is_none());

        let branches = git(tmp.path())
            .args(["branch", "--list", &p.branch_name("slot-idle")])
            .output()
            .await
            .unwrap();
        assert!(branches.stdout.is_empty(), "{branches:?}");
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

    /// A provisioned worktree holding uncommitted work.
    async fn dirty_worktree() -> (tempfile::TempDir, GitWorktreeProvider, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path()).await;
        let provider = GitWorktreeProvider::new(tmp.path(), "refactor");
        let worktree = provider.provision("slot-dirty").await.unwrap();
        std::fs::write(worktree.join("output.rs"), "// generated").unwrap();
        (tmp, provider, worktree)
    }

    /// The work is still on disk, and the hint says where, not that it's saved.
    fn assert_kept(worktree: &Path, hint: Option<String>) {
        let hint = hint.expect("a kept worktree must say so");
        assert!(worktree.join("output.rs").exists(), "work lost: {hint}");
        assert!(hint.contains(&worktree.display().to_string()), "{hint}");
        assert!(hint.contains("not committed"), "{hint}");
        assert!(!hint.contains("git merge"), "{hint}");
    }

    /// The work really is on the branch the hint names.
    async fn assert_saved(repo: &Path) {
        let shown = Command::new("git")
            .args(["show", "koda/wt/refactor-slot-dir:output.rs"])
            .current_dir(repo)
            .output()
            .await
            .unwrap();
        assert!(shown.status.success(), "{shown:?}");
        assert_eq!(shown.stdout, b"// generated");
    }

    #[tokio::test]
    async fn a_failed_commit_keeps_the_worktree() {
        let (tmp, provider, worktree) = dirty_worktree().await;
        // Git can't move the branch while its ref is locked.
        let lock = tmp
            .path()
            .join(".git/refs/heads/koda/wt/refactor-slot-dir.lock");
        std::fs::write(&lock, "").unwrap();

        let hint = provider.release("slot-dirty", &worktree).await.unwrap();
        assert_kept(&worktree, hint);

        // Nothing was lost, so the next release can still save it.
        std::fs::remove_file(&lock).unwrap();
        let hint = provider.release("slot-dirty", &worktree).await.unwrap();
        assert!(hint.unwrap().contains("git merge"));
        assert!(!worktree.exists());
        assert_saved(tmp.path()).await;
    }

    #[tokio::test]
    async fn a_failed_add_keeps_the_worktree() {
        let (tmp, provider, worktree) = dirty_worktree().await;
        let git_dir = tmp.path().join(".git/worktrees/slot-dirty");
        std::fs::write(git_dir.join("index.lock"), "").unwrap();

        let hint = provider.release("slot-dirty", &worktree).await.unwrap();
        assert_kept(&worktree, hint);
    }

    #[tokio::test]
    async fn a_failed_status_keeps_the_worktree() {
        let (_tmp, provider, worktree) = dirty_worktree().await;
        // Reading as "clean" here would force-remove the work.
        std::fs::write(worktree.join(".git"), "not a pointer").unwrap();

        let hint = provider.release("slot-dirty", &worktree).await.unwrap();
        assert_kept(&worktree, hint);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hooks_and_signing_cannot_refuse_the_save() {
        use std::os::unix::fs::PermissionsExt as _;

        let (tmp, provider, worktree) = dirty_worktree().await;
        let hook = tmp.path().join(".git/hooks/pre-commit");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        for setting in [["commit.gpgsign", "true"], ["gpg.program", "/no/such/gpg"]] {
            Command::new("git")
                .arg("config")
                .args(setting)
                .current_dir(tmp.path())
                .output()
                .await
                .unwrap();
        }

        let hint = provider.release("slot-dirty", &worktree).await.unwrap();
        assert!(hint.unwrap().contains("git merge"));
        assert!(!worktree.exists());
        assert_saved(tmp.path()).await;
    }

    /// An executable script that leaves `canary` behind if anything runs it.
    #[cfg(unix)]
    fn canary_script(path: &Path, canary: &Path) {
        use std::os::unix::fs::PermissionsExt as _;

        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("#!/bin/sh\ntouch {}\n", canary.display())).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    async fn git_ok(dir: &Path, args: &[&str]) {
        let done = Command::new("git")
            .args(["-c", "user.name=test", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(dir)
            .output()
            .await
            .unwrap();
        assert!(done.status.success(), "git {args:?}: {done:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_relative_hooks_path_runs_nothing_koda_does() {
        let outside = tempfile::tempdir().unwrap();
        let canary = outside.path().join("hook-ran");
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path()).await;
        git_ok(tmp.path(), &["config", "core.hooksPath", ".githooks"]).await;
        canary_script(&tmp.path().join(".githooks/post-checkout"), &canary);
        git_ok(tmp.path(), &["add", "-A"]).await;
        git_ok(tmp.path(), &["commit", "--no-verify", "-m", "hooks"]).await;
        std::fs::remove_file(tmp.path().join(".githooks/post-checkout")).unwrap();
        git_ok(tmp.path(), &["checkout", "--", ".githooks"]).await;
        assert!(canary.exists(), "git ran no post-checkout hook here");
        std::fs::remove_file(&canary).unwrap();

        let provider = GitWorktreeProvider::new(tmp.path(), "refactor");
        let worktree = provider.provision("slot-dirty").await.unwrap();
        assert!(!canary.exists(), "provisioning ran a hook");

        // The hooks a child can write, in its own worktree.
        for hook in ["prepare-commit-msg", "post-commit"] {
            canary_script(&worktree.join(".githooks").join(hook), &canary);
        }
        std::fs::write(worktree.join("output.rs"), "// generated").unwrap();
        // `--no-verify` doesn't stop these, so that alone wouldn't be enough.
        git_ok(
            &worktree,
            &["commit", "--allow-empty", "--no-verify", "-m", "control"],
        )
        .await;
        assert!(canary.exists(), "git ran no commit hook here");
        std::fs::remove_file(&canary).unwrap();

        let hint = provider.release("slot-dirty", &worktree).await.unwrap();

        assert!(!canary.exists(), "release ran a hook");
        assert!(hint.unwrap().contains("git merge"));
        assert_saved(tmp.path()).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_planted_fsmonitor_runs_nothing_koda_does() {
        let outside = tempfile::tempdir().unwrap();
        let canary = outside.path().join("fsmonitor-ran");
        let script = outside.path().join("fsmonitor");
        canary_script(&script, &canary);
        let (tmp, provider, worktree) = dirty_worktree().await;
        git_ok(tmp.path(), &["config", "extensions.worktreeConfig", "true"]).await;
        std::fs::write(
            tmp.path().join(".git/worktrees/slot-dirty/config.worktree"),
            format!("[core]\n\tfsmonitor = {}\n", script.display()),
        )
        .unwrap();
        git_ok(&worktree, &["status"]).await;
        assert!(canary.exists(), "git ran no fsmonitor here");
        std::fs::remove_file(&canary).unwrap();

        let hint = provider.release("slot-dirty", &worktree).await.unwrap();

        assert!(!canary.exists(), "release ran the fsmonitor");
        assert!(hint.unwrap().contains("git merge"));
        assert_saved(tmp.path()).await;
    }

    #[tokio::test]
    async fn outside_git_provision_fails_rather_than_share_the_project() {
        let tmp = tempfile::tempdir().unwrap();
        let p = GitWorktreeProvider::new(tmp.path(), "agent");
        let refused = p.provision("slot-x").await.unwrap_err();
        assert!(
            refused.to_string().contains("not a git repository"),
            "{refused}"
        );
    }

    #[tokio::test]
    async fn release_of_the_project_itself_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let p = GitWorktreeProvider::new(tmp.path(), "agent");
        let hint = p.release("slot-x", tmp.path()).await.unwrap();
        assert!(hint.is_none());
    }
}
