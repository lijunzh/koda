//! Filesystem mutation safety verifier (#1281, Tier 1 from #1265).
//!
//! [`verify_mutation_safe`] is the single seam every Write/Edit/Delete
//! tool call passes through *after* [`koda-core::tools::safe_resolve_path`]
//! and *before* the actual `tokio::fs` (or `LocalFileSystem`) call. It
//! catches two SSRF-adjacent attacks the logical-path resolver alone
//! cannot:
//!
//! 1. **Symlink escape via final target.** A path can be logically
//!    under the project root yet have its final component be a symlink
//!    pointing to `/etc/passwd`, `~/.aws/credentials`, etc. The pre-fix
//!    code happily followed and overwrote the link target.
//! 2. **Symlink escape via parent directory.** A path like
//!    `<root>/escape/passwd` where `<root>/escape` is a symlink to
//!    `/etc` resolves logically inside the root but writes to `/etc`.
//!
//! The verifier:
//!
//! - Walks up from `target` to find the first existing ancestor.
//! - Canonicalizes that ancestor (resolves all symlinks in the chain).
//! - Verifies the canonical ancestor lives under at least one allowed
//!   root (also canonicalized for fairness).
//! - If `target` itself exists *and* is a symlink, canonicalizes it
//!   and re-runs the allowed-root check on the link's target.
//!   In-project symlinks are accepted; escaping ones are rejected.
//!
//! ## What this does NOT do
//!
//! - **Hardened TOCTOU**: a true `O_NOFOLLOW` + `openat2(RESOLVE_BENEATH)`
//!   based check would close every race window, but is Linux-specific
//!   (`openat2` is kernel ≥5.6) and would force a `nix`/`rustix` dep.
//!   The verifier closes the *common* race (single-shot symlink swap
//!   between `safe_resolve_path` and the FS call) by re-canonicalizing
//!   here, but a determined attacker with a tight loop on the
//!   filesystem may still win. File tools that write should pair this
//!   verifier with a write-to-tmp + rename pattern (see
//!   `koda-core::tools::file_tools::atomic_write`) so the post-write
//!   lstat catches anything that snuck in.
//! - **`--no-sandbox` / debug mode**: when the user explicitly opts
//!   out of the kernel sandbox (#934 §6 Phase 2 acceptance), the
//!   in-process verifier is still active — the user gets full kernel
//!   access via `bash -c …` but Write/Edit/Delete tools remain gated.
//!   This is intentional asymmetry: the sandbox bypass is for shell
//!   tracing, not for letting the LLM scribble outside the project.

use crate::fs::{FsError, FsResult};
use std::path::{Path, PathBuf};

/// Reject `target` if writing/editing/deleting it would escape every
/// path in `allowed_roots`, including via symlink in any path component.
///
/// `target` does not need to exist (it's a fresh Write target). The
/// verifier walks up to the deepest *existing* ancestor and
/// canonicalizes from there, so a not-yet-created file in a verified
/// directory is fine.
///
/// `allowed_roots` is the policy list — typically `project_root` plus
/// the tempdir prefixes the kernel sandbox already allows. The caller
/// owns the policy; the verifier just enforces it. Roots are
/// canonicalized internally so the comparison is symlink-safe both
/// directions.
///
/// ## Errors
///
/// Returns [`FsError::PolicyDenied`] with a message describing which
/// invariant was violated. Never returns [`FsError::Io`] for the
/// "ancestor doesn't exist" case — that's expected for a fresh Write.
pub fn verify_mutation_safe(target: &Path, allowed_roots: &[PathBuf]) -> FsResult<()> {
    if allowed_roots.is_empty() {
        return Err(FsError::PolicyDenied {
            message: "no allowed mutation roots configured (this is a bug)".to_string(),
        });
    }

    // Canonicalize allowed roots once. A root that doesn't exist is
    // skipped rather than failing the whole call: tempdirs like
    // /var/folders/.../T/ may not exist on every host, and we don't
    // want a missing tempdir to lock the user out of project writes.
    let canonical_roots: Vec<PathBuf> = allowed_roots
        .iter()
        .filter_map(|r| std::fs::canonicalize(r).ok())
        .collect();
    if canonical_roots.is_empty() {
        return Err(FsError::PolicyDenied {
            message: format!(
                "none of the {} allowed mutation roots could be canonicalized; \
                 they may not exist on disk",
                allowed_roots.len()
            ),
        });
    }

    // ── Step 1: find the deepest existing ancestor and canonicalize. ──
    //
    // For a fresh Write target like `<root>/new/sub/file.txt`, the file
    // and possibly some parents don't exist yet. Walk up until something
    // does, then canonicalize that. If the canonical ancestor is inside
    // an allowed root, the eventual path will be too — `create_dir_all`
    // on a real-path-anchored chain can't escape via symlink because the
    // missing components don't exist yet.
    let existing_ancestor =
        deepest_existing_ancestor(target).ok_or_else(|| FsError::PolicyDenied {
            message: format!(
                "no existing ancestor for {} — refusing to walk up past the filesystem root",
                target.display()
            ),
        })?;

    let canonical_ancestor =
        std::fs::canonicalize(&existing_ancestor).map_err(|e| FsError::PolicyDenied {
            message: format!(
                "failed to canonicalize ancestor {}: {e}",
                existing_ancestor.display()
            ),
        })?;

    if !canonical_roots
        .iter()
        .any(|r| canonical_ancestor.starts_with(r))
    {
        return Err(FsError::PolicyDenied {
            message: format!(
                "path {} resolves through {} which escapes every allowed mutation root \
                 (likely a symlinked parent directory pointing outside the project)",
                target.display(),
                canonical_ancestor.display()
            ),
        });
    }

    // ── Step 2: if target itself exists and is a symlink, the link's
    // target must also stay within an allowed root.
    //
    // We allow in-project symlinks (e.g. `examples/latest -> v3/`) so
    // existing developer workflows keep working. We only reject the
    // dangerous case: a link whose target escapes. `lstat` (=
    // `symlink_metadata`) is essential here — `metadata` would follow
    // the link silently and we'd lose the chance to detect it.
    if let Ok(meta) = std::fs::symlink_metadata(target)
        && meta.file_type().is_symlink()
    {
        let canonical_target =
            std::fs::canonicalize(target).map_err(|e| FsError::PolicyDenied {
                message: format!(
                    "{} is a symlink whose target failed to canonicalize ({e}); \
                     refusing to mutate to avoid following an unresolvable link",
                    target.display()
                ),
            })?;
        if !canonical_roots
            .iter()
            .any(|r| canonical_target.starts_with(r))
        {
            return Err(FsError::PolicyDenied {
                message: format!(
                    "refusing to mutate {}: it is a symlink to {} which is outside \
                     every allowed mutation root. Resolve the link manually or \
                     point your tool at the target directly.",
                    target.display(),
                    canonical_target.display()
                ),
            });
        }
    }

    Ok(())
}

/// Walk up from `path` and return the first ancestor (including `path`
/// itself) that exists on disk. Returns `None` only if even the
/// filesystem root doesn't exist (essentially never).
///
/// Used by [`verify_mutation_safe`] to anchor canonicalization on
/// real bytes for fresh-Write targets whose final path components
/// don't exist yet.
fn deepest_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut current: Option<&Path> = Some(path);
    while let Some(p) = current {
        if p.exists() {
            return Some(p.to_path_buf());
        }
        current = p.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// One root, the tempdir; verifier should accept any path inside it.
    #[test]
    fn accepts_fresh_write_inside_root() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        let target = root.join("new").join("file.txt");
        verify_mutation_safe(&target, &[root]).expect("fresh write in root must pass");
    }

    #[test]
    fn rejects_path_outside_every_root() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        // /etc is real on every test host but not under our tempdir.
        let target = PathBuf::from("/etc/passwd");
        let err = verify_mutation_safe(&target, &[root])
            .expect_err("path outside roots must be rejected");
        assert!(matches!(err, FsError::PolicyDenied { .. }), "got: {err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_final_component_escaping_root() {
        // The headline #1281 attack: project_root/link.txt -> /etc/passwd.
        // Without the verifier, Write/Edit would happily clobber /etc/passwd.
        let dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let outside_target = outside.path().join("victim.txt");
        std::fs::write(&outside_target, b"original outside contents").unwrap();

        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&outside_target, &link).unwrap();

        let err = verify_mutation_safe(&link, &[dir.path().to_path_buf()])
            .expect_err("escaping symlink must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("symlink") && msg.contains("outside"),
            "error should mention symlink + outside, got: {msg}"
        );
        // And the outside file is untouched (verifier never wrote anything).
        assert_eq!(
            std::fs::read(&outside_target).unwrap(),
            b"original outside contents"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_parent_dir_escaping_root() {
        // project_root/escape -> /tmp/<other_tempdir>, then path is
        // project_root/escape/file.txt → would write into the other
        // tempdir if we didn't canonicalize the parent.
        let dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let escape = dir.path().join("escape");
        std::os::unix::fs::symlink(outside.path(), &escape).unwrap();
        let target = escape.join("file.txt");

        let err = verify_mutation_safe(&target, &[dir.path().to_path_buf()])
            .expect_err("escaping parent symlink must be rejected");
        assert!(matches!(err, FsError::PolicyDenied { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn accepts_in_project_symlink() {
        // examples/latest -> v3/ kind of pattern. The link target is
        // still inside the root, so it's fine to mutate through.
        let dir = TempDir::new().unwrap();
        let real = dir.path().join("v3");
        std::fs::create_dir(&real).unwrap();
        let real_file = real.join("config.toml");
        std::fs::write(&real_file, b"old").unwrap();

        let link = dir.path().join("latest");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let via_link = link.join("config.toml");

        verify_mutation_safe(&via_link, &[dir.path().to_path_buf()])
            .expect("in-project symlink must be accepted");
    }

    #[cfg(unix)]
    #[test]
    fn accepts_symlink_to_inside_root_at_final_component() {
        // dir/alias.txt -> dir/real.txt. The link target is in-project,
        // so the verifier accepts it.
        let dir = TempDir::new().unwrap();
        let real = dir.path().join("real.txt");
        std::fs::write(&real, b"hi").unwrap();
        let alias = dir.path().join("alias.txt");
        std::os::unix::fs::symlink(&real, &alias).unwrap();

        verify_mutation_safe(&alias, &[dir.path().to_path_buf()])
            .expect("in-project final-component symlink must be accepted");
    }

    #[test]
    fn empty_allowed_roots_is_a_hard_error() {
        // Belt-and-suspenders: catch the "callsite forgot to populate
        // policy" bug as a clear PolicyDenied rather than silently
        // letting everything through.
        let err =
            verify_mutation_safe(Path::new("/tmp/x"), &[]).expect_err("empty roots must error");
        assert!(matches!(err, FsError::PolicyDenied { .. }));
    }

    #[test]
    fn nonexistent_root_is_skipped_not_fatal() {
        // If the user's $TMPDIR doesn't exist on this host (rare but
        // possible in containers), the verifier should fall back to
        // the other roots rather than locking everyone out.
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("ok.txt");
        let bogus = PathBuf::from("/this/path/definitely/does/not/exist");
        verify_mutation_safe(&target, &[bogus, dir.path().to_path_buf()])
            .expect("at least one valid root means we proceed");
    }

    #[cfg(unix)]
    #[test]
    fn race_swap_between_check_and_act_is_caught_by_reverify() {
        // Models the TOCTOU window: caller does its own ancestor walk
        // (legacy code path), then attacker swaps the final component
        // for an escaping symlink, then caller calls verify_mutation_safe.
        // Verifier MUST catch the swap because it lstat's target itself.
        let dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let outside_file = outside.path().join("victim.txt");
        std::fs::write(&outside_file, b"untouched").unwrap();

        // Attacker just installed this symlink between caller's
        // logical-path check and our verifier call:
        let target = dir.path().join("attack.txt");
        std::os::unix::fs::symlink(&outside_file, &target).unwrap();

        let err = verify_mutation_safe(&target, &[dir.path().to_path_buf()])
            .expect_err("re-verify must catch the swap");
        assert!(matches!(err, FsError::PolicyDenied { .. }));
        assert_eq!(std::fs::read(&outside_file).unwrap(), b"untouched");
    }
}
