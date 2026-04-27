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
//! | `ClonefileProvider`   | 4     | macOS APFS `clonefile(2)` (CoW)      |
//! | `OverlayfsProvider`   | 4     | Linux overlayfs inside bwrap         |
//!
//! `ClonefileProvider` lives behind `#[cfg(target_os = "macos")]`, so its
//! intra-doc link is omitted from this table to keep Linux rustdoc happy.
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

// ── ClonefileProvider (macOS) ───────────────────────────────────────────

/// Workspace via APFS `clonefile(2)` — the speed champion (Phase 4d of #934).
///
/// Each slot gets a copy-on-write reflink of the project tree at
/// `~/.koda/clones/<project-hash>/<slot_id>/`. Provisioning is O(metadata),
/// not O(content): a 5 GB monorepo clones in **~10 ms** instead of the
/// 1-5 seconds [`GitWorktreeProvider`] takes for a fresh worktree.
///
/// ## Why the clones live OUTSIDE the project root
///
/// `clonefile(2)` is recursive AND returns `EPERM` if the destination
/// is a descendant of the source. If clones lived under
/// `<project>/.koda/clones/`, every slot's clone would *contain* every
/// previous slot's clone (matryoshka) AND the very first clone would
/// fail outright. Putting them under `~/.koda/clones/<project-hash>/`
/// keeps the source tree pristine and the clone footprint flat.
///
/// **Edge case (#1093):** when `project_root == $HOME`, the default
/// `~/.koda/clones/...` IS still inside the project root. `new()`
/// detects this and falls back to `$TMPDIR/koda-clones/<hash>/`; see
/// [`ClonefileProvider::new`] for the full fallback chain.
///
/// `<project-hash>` is a SHA-256 of the canonical project root path,
/// truncated to 16 hex chars. It collides only across projects with the
/// same canonical absolute path, which is impossible in practice.
///
/// ## Failure modes
///
/// - **Non-APFS filesystem** (`ENOTSUP`): caller must fall back to
///   [`GitWorktreeProvider`] or [`CwdProvider`]. The error message
///   names this explicitly so users aren't left guessing.
/// - **Cross-volume** (`EXDEV`): same as above. Happens when `$HOME`
///   and the project live on different APFS volumes (rare on default
///   macOS installs; common on dev VMs with mounted source trees).
/// - **Already exists**: idempotent — returns the existing path.
///   Lets `acquire` for a slot_id that previously crashed mid-flight
///   resume from where it left off.
///
/// We deliberately **do not** auto-fall-back to `GitWorktreeProvider`.
/// The user explicitly wired in `ClonefileProvider` (it's opt-in for at
/// least one release per the issue's roadmap); silently downgrading
/// would hide setup misconfigurations and make perf-debug confusing.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone)]
pub struct ClonefileProvider {
    project_root: PathBuf,
    /// Root under which all this provider's clones live, as in
    /// `<clones_root>/<slot_id>`. Defaults to
    /// `~/.koda/clones/<sha256(project_root)[..16]>/`. Tests override
    /// this so they don't pollute the user's home dir.
    clones_root: PathBuf,
}

#[cfg(target_os = "macos")]
impl ClonefileProvider {
    /// Standard constructor. Derives `clones_root` from `$HOME` and a
    /// 16-hex-char SHA-256 of the canonical project path.
    ///
    /// Returns `Err` if `$HOME` is unset or the project path can't be
    /// canonicalized — both indicate a setup problem the caller must
    /// surface, not silently work around.
    ///
    /// ## `project_root == $HOME` recursion guard (#1093)
    ///
    /// `clonefile(2)` returns `EPERM` when the destination is a
    /// descendant of the source. The default `clones_root` lives at
    /// `$HOME/.koda/clones/<hash>/`, which IS a descendant of
    /// `$HOME` — so when a user runs koda with `project_root ==
    /// $HOME`, every `provision()` would fail. Pre-#1093 the
    /// constructor still returned `Ok`, so `pick_write_provider`
    /// never fell back to `GitWorktreeProvider` and sub-agents with
    /// write tools could not be dispatched at all.
    ///
    /// We now detect this up front via [`choose_clones_root`] and
    /// fall back to `$TMPDIR/koda-clones/<hash>/`. macOS's default
    /// `$TMPDIR` (`/var/folders/...`) is never inside `$HOME`, so
    /// the fallback always works on stock installs. If both
    /// candidates would land inside `project_root` (a configuration
    /// so degenerate it requires a hand-rolled `$TMPDIR`), we return
    /// `Err` so `pick_write_provider` falls back to
    /// `GitWorktreeProvider`.
    pub fn new(project_root: impl Into<PathBuf>) -> Result<Self> {
        let project_root = project_root.into();
        let canonical = project_root
            .canonicalize()
            .with_context(|| format!("canonicalize project_root {}", project_root.display()))?;
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("$HOME not set; cannot derive ClonefileProvider clones_root")?;
        let tmpdir = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        let clones_root = Self::choose_clones_root(&canonical, &home, &tmpdir)?;
        Ok(Self {
            project_root: canonical,
            clones_root,
        })
    }

    /// Pure helper that picks a `clones_root` guaranteed not to be a
    /// descendant of `canonical_project`. Extracted from [`new`] so
    /// the recursion logic is unit-testable without manipulating
    /// process-wide `$HOME` / `$TMPDIR`.
    ///
    /// Strategy: prefer `<home>/.koda/clones/<hash>/` (the historical
    /// location, kept for cross-session resume + flat per-project
    /// layout). If that would recurse, fall back to
    /// `<tmpdir>/koda-clones/<hash>/`. If both would recurse, return
    /// `Err` — callers (i.e. `pick_write_provider`) treat that as
    /// "this provider can't work here" and fall back to a different
    /// provider entirely.
    fn choose_clones_root(canonical_project: &Path, home: &Path, tmpdir: &Path) -> Result<PathBuf> {
        let hash = Self::project_hash(canonical_project);
        let primary = home.join(".koda").join("clones").join(&hash);
        if !Self::is_inside(&primary, canonical_project) {
            return Ok(primary);
        }
        // Primary would recurse — try the tmpdir fallback. The
        // `koda-clones` (vs `.koda/clones`) name is deliberate: a
        // visible top-level dir under `$TMPDIR` is easier to spot
        // and clean up than a hidden one, and there's no
        // cross-session-resume motivation in `$TMPDIR` since macOS
        // periodically purges it.
        let fallback = tmpdir.join("koda-clones").join(&hash);
        if !Self::is_inside(&fallback, canonical_project) {
            return Ok(fallback);
        }
        anyhow::bail!(
            "ClonefileProvider cannot derive a clones_root outside project_root {}: \
             both primary ({}) and tmpdir fallback ({}) would recurse into the source. \
             $TMPDIR={} — set it to a directory outside the project root or use \
             GitWorktreeProvider.",
            canonical_project.display(),
            primary.display(),
            fallback.display(),
            tmpdir.display(),
        );
    }

    /// Test/advanced constructor: explicit `clones_root`. Lets tests
    /// point at a tempdir without mutating `$HOME`.
    ///
    /// Rejects `clones_root` paths that are descendants of
    /// `project_root` (#1093 defense in depth) — such a configuration
    /// would cause `clonefile(2)` to return `EPERM` at every
    /// `provision()` call, which is better surfaced as a constructor
    /// error than a confusing runtime failure.
    pub fn with_clones_root(
        project_root: impl Into<PathBuf>,
        clones_root: impl Into<PathBuf>,
    ) -> Result<Self> {
        let project_root = project_root.into();
        let clones_root = clones_root.into();
        let canonical = project_root
            .canonicalize()
            .with_context(|| format!("canonicalize project_root {}", project_root.display()))?;
        if Self::is_inside(&clones_root, &canonical) {
            anyhow::bail!(
                "clones_root {} is a descendant of project_root {} — \
                 clonefile(2) would return EPERM at every provision; \
                 choose a clones_root outside the project tree",
                clones_root.display(),
                canonical.display(),
            );
        }
        Ok(Self {
            project_root: canonical,
            clones_root,
        })
    }

    /// Where the clone for `slot_id` lives.
    fn clone_path(&self, slot_id: &str) -> PathBuf {
        self.clones_root.join(slot_id)
    }

    /// Stable 16-hex-char FNV-1a hash of the canonical project path.
    /// Stable means: same path → same hash across processes / restarts,
    /// so clones from a previous koda session can be resumed.
    ///
    /// FNV-1a (not SipHash) because `std::hash::DefaultHasher` is
    /// seeded with a random value per process, which would defeat
    /// cross-session resume. We don't need cryptographic strength
    /// here — collision space is "projects on this developer's
    /// machine," which is dozens at most.
    fn project_hash(canonical: &Path) -> String {
        use std::os::unix::ffi::OsStrExt;
        const FNV_OFFSET: u64 = 0xcbf29ce484222325;
        const FNV_PRIME: u64 = 0x100000001b3;
        let mut hash = FNV_OFFSET;
        for byte in canonical.as_os_str().as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        format!("{hash:016x}")
    }

    /// Returns true iff `candidate` would resolve to a path inside
    /// `canonical_root`. Compares using the deepest existing ancestor
    /// of `candidate` (canonicalized) so that symlinks like macOS
    /// `/var → /private/var` don't produce false negatives.
    ///
    /// `candidate` typically does not yet exist (we're about to
    /// create it), so we cannot canonicalize it directly. But its
    /// deepest existing ancestor IS canonicalizable, and that's
    /// enough to make `starts_with` symlink-safe.
    fn is_inside(candidate: &Path, canonical_root: &Path) -> bool {
        let mut probe: PathBuf = candidate.to_path_buf();
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        loop {
            if let Ok(canonical) = probe.canonicalize() {
                let mut resolved = canonical;
                for component in tail.iter().rev() {
                    resolved.push(component);
                }
                return resolved.starts_with(canonical_root);
            }
            match (probe.parent(), probe.file_name()) {
                (Some(parent), Some(name)) => {
                    tail.push(name.to_os_string());
                    probe = parent.to_path_buf();
                }
                _ => {
                    // Reached root without canonicalizing — fall
                    // back to a literal comparison. Conservative.
                    return candidate.starts_with(canonical_root);
                }
            }
        }
    }

    /// Internal: actually invoke `clonefile(2)`. Synchronous; callers
    /// must wrap in `tokio::task::spawn_blocking`. Translates errno
    /// into messages that name the failure mode and the recommended
    /// fallback.
    fn clonefile_sync(src: &Path, dst: &Path) -> Result<()> {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let src_c = CString::new(src.as_os_str().as_bytes())
            .with_context(|| format!("src path contains NUL: {}", src.display()))?;
        let dst_c = CString::new(dst.as_os_str().as_bytes())
            .with_context(|| format!("dst path contains NUL: {}", dst.display()))?;

        // SAFETY: both pointers are valid CStrings owned for the
        // duration of the call; libc::clonefile takes them by const
        // pointer and copies what it needs synchronously.
        let rc = unsafe { libc::clonefile(src_c.as_ptr(), dst_c.as_ptr(), 0) };
        if rc == 0 {
            return Ok(());
        }

        let err = std::io::Error::last_os_error();
        let raw = err.raw_os_error().unwrap_or(0);
        // Translate the well-known errno values into actionable messages.
        // `ENOTSUP` (45) and `EXDEV` (18) are the ones users actually hit;
        // everything else falls through to the generic message.
        let msg = match raw {
            libc::ENOTSUP => format!(
                "clonefile({}, {}) returned ENOTSUP — destination volume is not APFS. \
                 Use GitWorktreeProvider or CwdProvider on this volume.",
                src.display(),
                dst.display()
            ),
            libc::EXDEV => format!(
                "clonefile({}, {}) returned EXDEV — source and destination are on \
                 different volumes. Either move ~/.koda onto the same volume as the \
                 project, or use GitWorktreeProvider.",
                src.display(),
                dst.display()
            ),
            _ => format!(
                "clonefile({}, {}) failed: {err} (errno {raw})",
                src.display(),
                dst.display()
            ),
        };
        anyhow::bail!(msg)
    }
}

#[cfg(target_os = "macos")]
#[async_trait]
impl WorkspaceProvider for ClonefileProvider {
    async fn provision(&self, slot_id: &str) -> Result<PathBuf> {
        // Same slot_id sanity as GitWorktreeProvider: empty or
        // path-separator-bearing slot_ids would let a caller escape
        // the clones_root directory.
        if slot_id.is_empty() || slot_id.contains('/') || slot_id.contains('\\') {
            anyhow::bail!("Invalid slot_id for clonefile: {slot_id:?}");
        }

        let dst = self.clone_path(slot_id);

        // Idempotent resume: if the clone already exists, reuse it.
        // Matches GitWorktreeProvider's behavior so callers can retry
        // a crashed slot without first running release().
        if dst.exists() {
            tracing::debug!("Reusing existing clone: {}", dst.display());
            return Ok(dst);
        }

        // Ensure the parent dir exists; clonefile won't create it.
        if let Some(parent) = dst.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("create clones_root {}", parent.display()))?;
        }

        // clonefile(2) is synchronous. Wrap in spawn_blocking so we
        // don't park the tokio worker on a 5 GB metadata clone.
        let src = self.project_root.clone();
        let dst_clone = dst.clone();
        tokio::task::spawn_blocking(move || Self::clonefile_sync(&src, &dst_clone))
            .await
            .context("spawn_blocking for clonefile")??;

        tracing::info!("Provisioned clone {} for slot {slot_id}", dst.display());
        Ok(dst)
    }

    async fn release(&self, slot_id: &str, path: &Path) -> Result<Option<String>> {
        // Defense in depth: only delete if `path` is genuinely under
        // our clones_root. A bug in caller code that hands us
        // `project_root` would otherwise rm -rf the user's source.
        let canonical = match path.canonicalize() {
            Ok(c) => c,
            Err(_) => return Ok(None), // already gone
        };
        // Canonicalize `clones_root` for the comparison too — on macOS,
        // `/var` is a symlink to `/private/var`, so a non-canonicalized
        // `starts_with` check would spuriously refuse legitimate releases
        // when `clones_root` is under a tempdir. We tolerate the case
        // where `clones_root` doesn't yet exist (release-before-any-
        // provision) by treating it as "nothing to compare against, so
        // refuse" — safer than guessing.
        let canonical_root = self.clones_root.canonicalize().with_context(|| {
            format!(
                "canonicalize clones_root {} for release safety check",
                self.clones_root.display()
            )
        })?;
        if !canonical.starts_with(&canonical_root) {
            anyhow::bail!(
                "refusing to release {} — not under clones_root {}",
                canonical.display(),
                canonical_root.display()
            );
        }

        // Best-effort removal. CoW means free space comes back
        // automatically as soon as the inode refcount hits zero.
        if let Err(e) = tokio::fs::remove_dir_all(&canonical).await {
            // EBUSY / EACCES on individual files inside the clone
            // shouldn't tank the whole release — log and move on.
            tracing::warn!(
                "clonefile release of slot {slot_id} at {} hit error: {e}",
                canonical.display()
            );
        }
        Ok(None)
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

    // ── ClonefileProvider (macOS only) ───────────────────────────────────
    //
    // These tests exercise the real clonefile(2) syscall against a
    // tempdir. They will SKIP (assertion-free early return) if the
    // tempdir's volume isn't APFS, so they're safe in CI on any
    // macOS runner but pass with full coverage on developer machines
    // (where /tmp is APFS by default).

    #[cfg(target_os = "macos")]
    mod clonefile {
        use super::*;

        /// Probe whether `clonefile(2)` is supported on the volume
        /// hosting `dir`. Returns `true` iff a 1-file clone succeeds.
        /// Lets the rest of the suite skip cleanly on filesystems
        /// where the syscall is unavailable (e.g. an HFS+ tempdir).
        async fn clonefile_supported_at(dir: &Path) -> bool {
            let src = dir.join(".probe-src");
            let dst = dir.join(".probe-dst");
            tokio::fs::write(&src, b"x").await.unwrap();
            // Use the same syscall the provider uses, so support is
            // tested via the actual code path.
            let result = tokio::task::spawn_blocking({
                let src = src.clone();
                let dst = dst.clone();
                move || ClonefileProvider::clonefile_sync(&src, &dst)
            })
            .await
            .unwrap();
            let _ = tokio::fs::remove_file(&src).await;
            let _ = tokio::fs::remove_file(&dst).await;
            result.is_ok()
        }

        /// Build a (project_root, clones_root, provider) triple in one
        /// tempdir. Both roots live under the same parent so they're
        /// guaranteed on the same volume — EXDEV won't surprise us.
        fn make_provider(tmp: &Path) -> (PathBuf, PathBuf, ClonefileProvider) {
            let project = tmp.join("project");
            let clones = tmp.join("clones");
            std::fs::create_dir_all(&project).unwrap();
            std::fs::write(project.join("hello.txt"), "world").unwrap();
            let p = ClonefileProvider::with_clones_root(&project, &clones).unwrap();
            (project, clones, p)
        }

        // ── #1093: project_root == $HOME recursion guard ──────────
        //
        // These tests are pure-logic over `choose_clones_root` and
        // `with_clones_root` — they never call `clonefile(2)` so they
        // run on any macOS host regardless of tempdir filesystem.

        /// Regression for #1093: when `project_root == $HOME`, the
        /// default `$HOME/.koda/clones/<hash>` would be a descendant
        /// of `project_root`. The helper must fall back to
        /// `$TMPDIR/koda-clones/<hash>` instead, so `new()` returns
        /// `Ok` with a clones_root that is NOT inside the project.
        #[test]
        fn choose_clones_root_falls_back_when_home_equals_project() {
            let tmp = tempfile::tempdir().unwrap();
            // Simulate "home is the project root" by passing the same
            // canonical path as both `project` and `home`.
            let home = tmp.path().canonicalize().unwrap();
            let project = home.clone();
            // A tmpdir that is NOT inside `project` — like the real
            // macOS `/var/folders/...` is never inside `$HOME`.
            let tmpdir = std::env::temp_dir();
            assert!(
                !tmpdir.starts_with(&project),
                "test precondition: system tempdir must not be inside the synthetic home"
            );

            let chosen = ClonefileProvider::choose_clones_root(&project, &home, &tmpdir)
                .expect("fallback to tmpdir must succeed when primary recurses");
            assert!(
                !chosen.starts_with(&project),
                "chosen clones_root {} must not be inside project_root {}",
                chosen.display(),
                project.display(),
            );
            assert!(
                chosen.starts_with(&tmpdir) && chosen.to_string_lossy().contains("koda-clones"),
                "fallback must land under <tmpdir>/koda-clones, got {}",
                chosen.display(),
            );
        }

        /// When `project_root` is NOT `$HOME` (the common case), the
        /// helper must return the primary path unchanged — we do not
        /// want to perturb the historical layout for users who weren't
        /// hitting the bug.
        #[test]
        fn choose_clones_root_keeps_primary_in_common_case() {
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path().canonicalize().unwrap();
            let project = home.join("some-project");
            std::fs::create_dir_all(&project).unwrap();
            let project = project.canonicalize().unwrap();
            let tmpdir = std::env::temp_dir();

            let chosen = ClonefileProvider::choose_clones_root(&project, &home, &tmpdir).unwrap();
            let expected_prefix = home.join(".koda").join("clones");
            assert!(
                chosen.starts_with(&expected_prefix),
                "common case must preserve $HOME/.koda/clones/ layout, got {}",
                chosen.display(),
            );
        }

        /// Degenerate case: both primary AND fallback are inside the
        /// project root (e.g. user set `$TMPDIR` to a path inside
        /// their home, and home == project). Helper must `Err` so
        /// `pick_write_provider` falls back to `GitWorktreeProvider`
        /// instead of returning a provider that will EPERM at every
        /// `provision()`.
        #[test]
        fn choose_clones_root_errors_when_both_candidates_recurse() {
            let tmp = tempfile::tempdir().unwrap();
            let project = tmp.path().canonicalize().unwrap();
            let home = project.clone();
            // Force tmpdir inside project as well (extreme but legal).
            let tmpdir_inside = project.join("tmp");
            std::fs::create_dir_all(&tmpdir_inside).unwrap();

            let err = ClonefileProvider::choose_clones_root(&project, &home, &tmpdir_inside)
                .expect_err("must error when no candidate escapes project_root");
            let msg = format!("{err:#}");
            assert!(
                msg.contains("GitWorktreeProvider"),
                "error message must point users at the fallback provider, got: {msg}",
            );
        }

        /// Defense in depth: `with_clones_root` must reject a
        /// caller-supplied clones_root that's inside project_root,
        /// rather than returning a provider that will EPERM at runtime.
        #[test]
        fn with_clones_root_rejects_inside_project() {
            let tmp = tempfile::tempdir().unwrap();
            let project = tmp.path().join("project");
            std::fs::create_dir_all(&project).unwrap();
            // clones_root explicitly INSIDE project — the historical
            // foot-gun.
            let clones = project.join(".koda").join("clones");

            let err = ClonefileProvider::with_clones_root(&project, &clones)
                .expect_err("must reject clones_root inside project_root");
            let msg = format!("{err:#}");
            assert!(
                msg.contains("EPERM"),
                "error message must explain why (EPERM at provision), got: {msg}",
            );
        }

        /// `with_clones_root` must still accept the common case (clones
        /// as a sibling of project) — this is what every existing test
        /// using `make_provider` relies on.
        #[test]
        fn with_clones_root_accepts_sibling() {
            let tmp = tempfile::tempdir().unwrap();
            let project = tmp.path().join("project");
            let clones = tmp.path().join("clones");
            std::fs::create_dir_all(&project).unwrap();
            ClonefileProvider::with_clones_root(&project, &clones)
                .expect("sibling clones_root must be accepted");
        }

        #[tokio::test]
        async fn project_hash_is_stable_and_pathlike() {
            let tmp = tempfile::tempdir().unwrap();
            let canonical = tmp.path().canonicalize().unwrap();
            let a = ClonefileProvider::project_hash(&canonical);
            let b = ClonefileProvider::project_hash(&canonical);
            assert_eq!(a, b, "hash must be deterministic across calls");
            assert_eq!(a.len(), 16, "hash must be 16 hex chars");
            assert!(
                a.chars().all(|c| c.is_ascii_hexdigit()),
                "hash must be hex-only: {a}"
            );
        }

        #[tokio::test]
        async fn project_hash_differs_for_different_paths() {
            let a = ClonefileProvider::project_hash(Path::new("/tmp/proj-a"));
            let b = ClonefileProvider::project_hash(Path::new("/tmp/proj-b"));
            assert_ne!(a, b);
        }

        #[tokio::test]
        async fn invalid_slot_id_rejected() {
            let tmp = tempfile::tempdir().unwrap();
            let (_, _, p) = make_provider(tmp.path());
            assert!(p.provision("").await.is_err());
            assert!(p.provision("foo/bar").await.is_err());
            assert!(p.provision("foo\\bar").await.is_err());
        }

        #[tokio::test]
        async fn provision_clones_project_tree() {
            let tmp = tempfile::tempdir().unwrap();
            if !clonefile_supported_at(tmp.path()).await {
                eprintln!("skipping: tempdir is not on an APFS volume");
                return;
            }
            let (_, clones, p) = make_provider(tmp.path());

            let dst = p.provision("slot-1").await.unwrap();

            assert_eq!(dst, clones.join("slot-1"));
            assert!(dst.exists(), "clone dir should exist");
            // The cloned tree must contain the source content.
            let cloned = std::fs::read_to_string(dst.join("hello.txt")).unwrap();
            assert_eq!(cloned, "world");
        }

        #[tokio::test]
        async fn provision_is_copy_on_write() {
            // The whole point of clonefile is that mutations on the
            // clone don't leak back to the source. Pin that down so a
            // future refactor (e.g. swapping in `cp -R`) gets caught.
            let tmp = tempfile::tempdir().unwrap();
            if !clonefile_supported_at(tmp.path()).await {
                eprintln!("skipping: tempdir is not on an APFS volume");
                return;
            }
            let (project, _, p) = make_provider(tmp.path());

            let dst = p.provision("slot-cow").await.unwrap();
            std::fs::write(dst.join("hello.txt"), "clobbered").unwrap();
            std::fs::write(dst.join("new.txt"), "only-in-clone").unwrap();

            // Source must be untouched.
            let src_hello = std::fs::read_to_string(project.join("hello.txt")).unwrap();
            assert_eq!(src_hello, "world");
            assert!(!project.join("new.txt").exists());
        }

        #[tokio::test]
        async fn provision_is_idempotent() {
            // Re-provisioning the same slot_id (e.g. resuming after a
            // crash) must reuse the existing clone rather than
            // erroring or wastefully cloning again.
            let tmp = tempfile::tempdir().unwrap();
            if !clonefile_supported_at(tmp.path()).await {
                eprintln!("skipping: tempdir is not on an APFS volume");
                return;
            }
            let (_, _, p) = make_provider(tmp.path());

            let a = p.provision("slot-resume").await.unwrap();
            std::fs::write(a.join("marker.txt"), "persisted").unwrap();
            let b = p.provision("slot-resume").await.unwrap();

            assert_eq!(a, b);
            // The marker we wrote between provisions must still be
            // there — proving we reused, not re-cloned.
            assert_eq!(
                std::fs::read_to_string(b.join("marker.txt")).unwrap(),
                "persisted"
            );
        }

        #[tokio::test]
        async fn release_removes_clone() {
            let tmp = tempfile::tempdir().unwrap();
            if !clonefile_supported_at(tmp.path()).await {
                eprintln!("skipping: tempdir is not on an APFS volume");
                return;
            }
            let (_, _, p) = make_provider(tmp.path());

            let dst = p.provision("slot-rm").await.unwrap();
            assert!(dst.exists());

            let hint = p.release("slot-rm", &dst).await.unwrap();
            // ClonefileProvider doesn't preserve work onto a branch
            // (no git semantics), so release returns no hint.
            assert!(hint.is_none());
            assert!(!dst.exists(), "clone dir should be gone");
        }

        #[tokio::test]
        async fn release_missing_path_is_ok() {
            // If the clone is already gone (e.g. user manually rm'd
            // ~/.koda/clones), release should not error — idempotent
            // cleanup is the contract.
            let tmp = tempfile::tempdir().unwrap();
            let (_, clones, p) = make_provider(tmp.path());
            let phantom = clones.join("never-existed");
            let result = p.release("never-existed", &phantom).await.unwrap();
            assert!(result.is_none());
        }

        #[tokio::test]
        async fn release_refuses_path_outside_clones_root() {
            // The provider should refuse to delete anything that
            // isn't under its clones_root — critical defense against a
            // caller bug that would otherwise rm -rf the user's
            // source tree.
            let tmp = tempfile::tempdir().unwrap();
            let (project, _, p) = make_provider(tmp.path());
            let result = p.release("slot-bad", &project).await;
            assert!(
                result.is_err(),
                "release of a path outside clones_root must error"
            );
            // And the source must still exist — belt and suspenders.
            assert!(project.join("hello.txt").exists());
        }
    }
}
