//! Managed worktrees, as Codex makes them (`codex-rs/worktree`): a session
//! in a checkout of its own, outside the repository, so nothing a
//! repository commits (a symlinked folder, say) decides where it goes.
//!
//! - **Where:** `<root>/<4 hex>/<repository name>`, `root` being Koda's
//!   `worktrees` folder in its config directory. The bucket keeps two
//!   checkouts of one repository apart. On macOS the root holds a
//!   `.metadata_never_index` file, so Spotlight leaves the copies alone.
//! - **What:** the source's `HEAD`, detached (no branch is made). Git adds
//!   the worktree without checking files out; Koda pins `core.worktree` in
//!   the new worktree's own config, then checks the files out with every
//!   filter turned off ([`crate::Operation::WorkingTree`]).
//! - **Whose:** the session that runs in it is recorded in the worktree's
//!   git metadata (`koda-session.json`), as Codex records its thread.
//! - **Removal** is the user's: `git worktree remove <path>`, which refuses
//!   a checkout with changes.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

use crate::{Operation, command};

/// A worktree Koda made for a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedWorktree {
    /// The checkout.
    pub root: PathBuf,
    /// Where the session works: the source's folder, inside the checkout.
    pub cwd: PathBuf,
    /// The repository it came from.
    pub source_root: PathBuf,
    /// The commit it starts at.
    pub head_sha: String,
}

/// The file in a worktree's git metadata that names its session.
const OWNER_FILE: &str = "koda-session.json";

/// Make a worktree of the repository `source_cwd` is in, at its `HEAD`,
/// under `root`, as Codex's `WorktreeManager::create` does.
pub fn create(root: &Path, source_cwd: &Path) -> io::Result<ManagedWorktree> {
    if !root.is_absolute() {
        return Err(io::Error::other(
            "the worktree folder must be an absolute path",
        ));
    }
    std::fs::create_dir_all(root)?;
    let root = &root.canonicalize()?;
    let source_cwd = source_cwd.canonicalize()?;
    let source_root = PathBuf::from(stdout(
        &source_cwd,
        Operation::Metadata,
        ["rev-parse", "--show-toplevel"],
    )?)
    .canonicalize()?;
    let relative = source_cwd
        .strip_prefix(&source_root)
        .map_err(|_| io::Error::other("the folder is outside its repository"))?
        .to_path_buf();
    let name = source_root
        .file_name()
        .ok_or_else(|| io::Error::other("the repository's folder has no name"))?
        .to_os_string();
    let head_sha = stdout(
        &source_root,
        Operation::Metadata,
        ["rev-parse", "--verify", "--end-of-options", "HEAD^{commit}"],
    )
    .map_err(|e| io::Error::other(format!("the repository has no commit to start from: {e}")))?;

    let checkout = allocate(root, &name)?;
    if let Err(e) = run(
        &source_root,
        Operation::WorkingTree,
        [
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("--detach"),
            OsStr::new("--no-checkout"),
            checkout.as_os_str(),
            OsStr::new(&head_sha),
        ],
    ) {
        remove_empty_bucket(&checkout);
        return Err(e);
    }
    // Only the new worktree's config: the source's settings stay as they
    // are, and `core.worktree` holds the files where they belong even when
    // per-worktree config is off.
    let populated = stdout(
        &checkout,
        Operation::Metadata,
        [
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "config.worktree",
        ],
    )
    .and_then(|config| {
        run(
            &checkout,
            Operation::Metadata,
            [
                OsStr::new("config"),
                OsStr::new("--file"),
                OsStr::new(&config),
                OsStr::new("core.worktree"),
                checkout.as_os_str(),
            ],
        )?;
        run(
            &checkout,
            Operation::WorkingTree,
            [
                OsStr::new("--work-tree=."),
                OsStr::new("reset"),
                OsStr::new("--hard"),
                OsStr::new("--no-recurse-submodules"),
                OsStr::new(&head_sha),
            ],
        )
    });
    if let Err(e) = populated {
        roll_back(&source_root, &checkout);
        return Err(e);
    }
    let cwd = checkout.join(&relative);
    let inside = cwd
        .canonicalize()
        .is_ok_and(|cwd| cwd.is_dir() && cwd.starts_with(&checkout));
    if !inside {
        roll_back(&source_root, &checkout);
        return Err(io::Error::other(format!(
            "{} isn't in the commit the worktree starts at",
            relative.display()
        )));
    }
    Ok(ManagedWorktree {
        root: checkout,
        cwd,
        source_root,
        head_sha,
    })
}

/// Record `session_id` as the session working in `checkout`. Binding the
/// same session again changes nothing; another session's record stays.
pub fn bind_session(checkout: &Path, session_id: &str) -> io::Result<()> {
    if session_id.is_empty() {
        return Err(io::Error::other("no session id to record"));
    }
    let path = owner_path(checkout)?;
    if let Some(existing) = owner(checkout)? {
        return if existing == session_id {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "the worktree already belongs to session {existing}"
            )))
        };
    }
    let record = serde_json::json!({ "version": 1, "ownerSessionId": session_id });
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    io::Write::write_all(&mut file, record.to_string().as_bytes())
}

/// The session recorded for `checkout`, if any.
pub fn owner(checkout: &Path) -> io::Result<Option<String>> {
    let path = owner_path(checkout)?;
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let record: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| io::Error::other(format!("{}: {e}", path.display())))?;
    match (
        record["version"].as_u64(),
        record["ownerSessionId"].as_str(),
    ) {
        (Some(1), Some(id)) if !id.is_empty() => Ok(Some(id.to_string())),
        _ => Err(io::Error::other(format!(
            "{} isn't a session record",
            path.display()
        ))),
    }
}

/// `checkout`'s own git metadata folder, where its record goes.
fn owner_path(checkout: &Path) -> io::Result<PathBuf> {
    let dir = stdout(
        checkout,
        Operation::Metadata,
        ["rev-parse", "--path-format=absolute", "--git-dir"],
    )?;
    Ok(PathBuf::from(dir).join(OWNER_FILE))
}

/// A new, empty bucket under `root` for a checkout named `name`.
fn allocate(root: &Path, name: &OsStr) -> io::Result<PathBuf> {
    std::fs::create_dir_all(root)?;
    #[cfg(target_os = "macos")]
    std::fs::write(root.join(".metadata_never_index"), b"")?;
    for attempt in 0u32..=u16::MAX as u32 {
        let bucket = root.join(format!("{:04x}", random_u16(attempt)));
        match std::fs::create_dir(&bucket) {
            Ok(()) => return Ok(bucket.join(name)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("no free worktree folder left"))
}

/// A random 16-bit number, from the standard library's randomly seeded
/// hasher.
fn random_u16(salt: u32) -> u16 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u32(salt);
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default(),
    );
    hasher.finish() as u16
}

/// Undo a worktree that couldn't be finished. It's new and holds nothing of
/// anyone's, so `--force` loses nothing.
fn roll_back(source_root: &Path, checkout: &Path) {
    let _ = run(
        source_root,
        Operation::WorkingTree,
        [
            OsStr::new("worktree"),
            OsStr::new("remove"),
            OsStr::new("--force"),
            checkout.as_os_str(),
        ],
    );
    remove_empty_bucket(checkout);
}

/// Remove `checkout`'s bucket if it's empty.
fn remove_empty_bucket(checkout: &Path) {
    if let Some(bucket) = checkout.parent() {
        let _ = std::fs::remove_dir(bucket);
    }
}

/// Run git; its failure, with git's message, is the error.
fn run<I, S>(dir: &Path, operation: Operation, args: I) -> io::Result<std::process::Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let out = command(dir, operation)?.args(args).output()?;
    if out.status.success() {
        Ok(out)
    } else {
        Err(io::Error::other(format!(
            "git failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

/// Run git and return its output, trimmed.
fn stdout<I, S>(dir: &Path, operation: Operation, args: I) -> io::Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let out = run(dir, operation, args)?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A repository with a commit and a subfolder, and a root for worktrees.
    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("proj");
        fs::create_dir_all(repo.join("sub")).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        fs::write(repo.join("sub/a.txt"), "one\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "first"]);
        let root = tmp.path().join("config/worktrees");
        (tmp, repo.canonicalize().unwrap(), root)
    }

    #[test]
    fn a_worktree_is_a_detached_checkout_outside_the_repository() {
        let (_tmp, repo, root) = setup();
        let wt = create(&root, &repo.join("sub")).unwrap();
        let root = root.canonicalize().unwrap();
        assert!(wt.root.starts_with(&root), "{wt:?}");
        assert!(!wt.root.starts_with(&repo), "{wt:?}");
        assert_eq!(wt.root.file_name().unwrap(), "proj");
        assert_eq!(wt.root.parent().unwrap().parent().unwrap(), root);
        assert_eq!(wt.cwd, wt.root.join("sub"));
        assert_eq!(fs::read_to_string(wt.cwd.join("a.txt")).unwrap(), "one\n");
        assert_eq!(wt.head_sha, git(&repo, &["rev-parse", "HEAD"]));
        assert_eq!(wt.source_root, repo);
        // Detached: no branch was made.
        assert_eq!(
            git(&wt.root, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "HEAD"
        );
        assert_eq!(git(&repo, &["branch", "--list"]).lines().count(), 1);
        // The source's own config is untouched.
        assert!(git(&repo, &["config", "--list", "--local"]).contains("core.bare=false"));
        assert!(!git(&repo, &["config", "--list", "--local"]).contains("core.worktree"));
        #[cfg(target_os = "macos")]
        assert!(root.join(".metadata_never_index").exists());
    }

    #[test]
    fn two_worktrees_of_one_repository_get_their_own_folders() {
        let (_tmp, repo, root) = setup();
        let a = create(&root, &repo).unwrap();
        let b = create(&root, &repo).unwrap();
        assert_ne!(a.root, b.root);
        assert_eq!(a.cwd, a.root);
    }

    #[test]
    fn a_session_is_recorded_once() {
        let (_tmp, repo, root) = setup();
        let wt = create(&root, &repo).unwrap();
        assert_eq!(owner(&wt.root).unwrap(), None);
        bind_session(&wt.root, "s-1").unwrap();
        bind_session(&wt.root, "s-1").unwrap();
        assert_eq!(owner(&wt.root).unwrap().as_deref(), Some("s-1"));
        assert!(bind_session(&wt.root, "s-2").is_err());
        // The record sits in git's metadata, not in the checkout.
        assert!(!wt.root.join(OWNER_FILE).exists());
        assert_eq!(git(&wt.root, &["status", "--porcelain"]), "");
    }

    #[test]
    fn a_repository_without_a_commit_makes_no_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("empty");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        let root = tmp.path().join("worktrees");
        let err = create(&root, &repo).unwrap_err().to_string();
        assert!(err.contains("no commit"), "{err}");
        // Nothing left behind.
        assert!(fs::read_dir(&root).map(|d| d.count()).unwrap_or(0) <= 1);
    }

    #[test]
    fn a_folder_outside_a_repository_makes_no_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(create(&tmp.path().join("w"), tmp.path()).is_err());
        assert!(create(Path::new("relative"), tmp.path()).is_err());
    }
}
