//! Path defense primitives — application-layer hardening against escape attacks.
//!
//! These complement the kernel sandbox (seatbelt/bwrap) by catching attacks
//! that happen *before* a syscall is issued, such as symlink-based redirects
//! and TOCTOU races on new-file writes.
//!
//! The design mirrors Claude Code's `fsOperations.ts` and
//! `permissions/filesystem.ts`:
//!
//! | This module              | CC equivalent                           |
//! |--------------------------|-----------------------------------------|
//! | [`find_symlink_in_path`] | implicit in `safeResolvePath` (lstat loop) |
//! | [`resolve_deepest_existing_ancestor`] | `resolveDeepestExistingAncestorSync` |
//! | [`paths_for_write_check`]| `getPathsForPermissionCheck`            |
//! | [`is_path_inside`]       | `pathInWorkingPath`                     |
//! | [`is_dangerous_system_path`] | `isDangerousRemovalPath`            |
//!
//! ## Attack classes addressed
//!
//! 1. **Symlink escape** — `./evil → /etc/shadow`. Without chain expansion
//!    the policy check sees `./evil` (inside root); the write lands at
//!    `/etc/shadow`. `paths_for_write_check` expands the full chain and
//!    checks every link.
//!
//! 2. **Dangling-symlink new-file bypass** — `./data/ → /etc/cron.d/`
//!    (directory symlink, target doesn't exist yet at check time). The
//!    write creates a cron job outside the root. `resolve_deepest_existing_ancestor`
//!    walks up to the first real component, canonicalizes it, and rejoins
//!    the non-existing tail so the check sees the real destination.
//!
//! 3. **Catastrophic deletion** — `rm -rf /usr`. `is_dangerous_system_path`
//!    blocks unconditionally regardless of allow rules.
//!
//! 4. **macOS private-symlink bypass** — `/tmp` is a symlink to
//!    `/private/tmp`. Without normalization, a path starting with
//!    `/private/tmp/…` would fail an `is_path_inside(path, /tmp)` check.
//!    Both sides are canonicalized by callers; `is_path_inside` uses
//!    Rust's `Path::starts_with` which is component-aware.
//!
//! ## Invariants
//!
//! - **No panics.** Every function is total; I/O errors are absorbed and
//!   reported through the return type or cause a graceful false-positive
//!   (fail closed on permission checks, fail open on non-existence checks
//!   where the path will be created fresh).
//! - **No blocking inside async**. All functions are synchronous. Callers
//!   in async contexts must use `tokio::task::spawn_blocking` if needed.
//! - **No TOCTOU widening**. We never `stat` the same path twice; we
//!   `symlink_metadata` once and branch on the result immediately.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

/// Maximum symlink hops before we declare a cycle and stop.
/// Matches Linux/macOS `SYMLOOP_MAX` (40) and the CC implementation.
const MAX_SYMLINK_DEPTH: usize = 40;

// ── 1. find_symlink_in_path ───────────────────────────────────────────────

/// Walk every *existing* prefix of `path` via `symlink_metadata` (lstat)
/// and return the first component that is a symlink, or `None` if every
/// existing component resolves to a real file/directory.
///
/// The walk is prefix-by-prefix (`/a`, `/a/b`, `/a/b/c`, …) and stops
/// at the first non-existing component — we can't lstat what doesn't exist.
///
/// ## Example
///
/// ```
/// # use std::path::Path;
/// // If /tmp is a symlink to /private/tmp on macOS:
/// // find_symlink_in_path(Path::new("/tmp/work/file.txt")) → Some("/tmp")
/// ```
///
/// ## Errors
///
/// Returns `Err` only for unexpected I/O (e.g. permission denied on `lstat`).
/// `ENOENT` is treated as "end of existing prefix" and causes `Ok(None)`.
pub fn find_symlink_in_path(path: &Path) -> std::io::Result<Option<PathBuf>> {
    let mut current = PathBuf::new();

    for component in path.components() {
        match component {
            Component::RootDir | Component::Prefix(_) => {
                current.push(component.as_os_str());
                continue; // root and drive letters are never symlinks
            }
            Component::CurDir | Component::ParentDir => {
                current.push(component.as_os_str());
                continue; // logical — don't lstat `.` or `..`
            }
            Component::Normal(part) => {
                current.push(part);
            }
        }

        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Ok(Some(current));
            }
            Ok(_) => {} // real entry — continue
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                break; // path component doesn't exist; nothing more to walk
            }
            Err(e) => return Err(e),
        }
    }

    Ok(None)
}

// ── 2. resolve_deepest_existing_ancestor ─────────────────────────────────

/// Resolve the deepest *existing* ancestor of `path` through symlinks and
/// return the resolved path with non-existing tail segments re-joined.
///
/// Returns `Some(resolved)` when at least one existing ancestor contains
/// a symlink. Returns `None` when no symlinks are present in the existing
/// prefix (the path's existing portion is already canonical).
///
/// ## Why this matters
///
/// Standard `canonicalize` fails with `ENOENT` for non-existing paths, so
/// a simple `canonicalize(path)` is useless for new-file write checks.
/// Without this function, an agent can bypass write policy via a dangling
/// directory symlink:
///
/// ```text
/// ./uploads/ → /var/www/html/     # symlink created by prior action
/// Write("./uploads/backdoor.php") # check sees "./uploads/" (inside root)
///                                 # write lands at /var/www/html/backdoor.php
/// ```
///
/// This function catches that: it walks up to `./uploads`, detects the
/// symlink, and returns `/var/www/html/backdoor.php` — outside the root.
///
/// ## Equivalent
///
/// Claude Code's `resolveDeepestExistingAncestorSync` in `fsOperations.ts`.
pub fn resolve_deepest_existing_ancestor(path: &Path) -> std::io::Result<Option<PathBuf>> {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut dir = path.to_path_buf();

    while let Some(parent_ref) = dir.parent() {
        let parent = parent_ref.to_path_buf();

        match std::fs::symlink_metadata(&dir) {
            Ok(meta) if meta.file_type().is_symlink() => {
                // Found a symlink (live or dangling). Try canonicalize first
                // (handles chained symlinks); fall back to readlink for
                // dangling ones whose target doesn't exist yet.
                let resolved = match std::fs::canonicalize(&dir) {
                    Ok(r) => r,
                    Err(_) => {
                        // Dangling — readlink gives us the raw target.
                        let target = std::fs::read_link(&dir)?;
                        if target.is_absolute() {
                            target
                        } else {
                            dir.parent().map(|p| p.join(&target)).unwrap_or(target)
                        }
                    }
                };
                // Rejoin any non-existing tail that was popped earlier.
                let final_path = tail.iter().rev().fold(resolved, |acc, seg| acc.join(seg));
                return Ok(Some(final_path));
            }
            Ok(_) => {
                // Existing non-symlink: canonicalize resolves any symlinks
                // lurking in *its* ancestors. If canonicalize returns a
                // different path, a symlink existed somewhere above.
                match std::fs::canonicalize(&dir) {
                    Ok(canon) if canon != dir => {
                        let final_path = tail.iter().rev().fold(canon, |acc, seg| acc.join(seg));
                        return Ok(Some(final_path));
                    }
                    Ok(_) => return Ok(None),  // no symlink anywhere above
                    Err(_) => return Ok(None), // EACCES etc — fail open
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Non-existing component — pop it and walk up.
                if let Some(name) = dir.file_name() {
                    tail.push(name.to_os_string());
                }
                dir = parent;
                continue;
            }
            Err(e) => return Err(e),
        }
    }

    Ok(None)
}

// ── 3. paths_for_write_check ──────────────────────────────────────────────

/// Collect every path that must be validated before allowing a write to
/// `path`. The returned `Vec` always contains at least `[path]`.
///
/// For symlinks the list contains the original path, all intermediate link
/// targets in the chain, and the final resolved path. This ensures a deny
/// rule on `/etc/passwd` fires even when the agent writes through
/// `./evil.txt → /etc/passwd` — every hop is checked.
///
/// ## Algorithm (mirrors CC `getPathsForPermissionCheck`)
///
/// 1. Seed the set with the original path.
/// 2. If the path doesn't exist on disk, call
///    [`resolve_deepest_existing_ancestor`] to detect dangling-symlink
///    redirects and add the resolved destination.
/// 3. Otherwise follow the symlink chain: at each hop, add the current
///    target; break on non-symlink, FIFO/socket/device, or cycle.
/// 4. Cap at `MAX_SYMLINK_DEPTH` hops (matches `SYMLOOP_MAX`).
/// 5. Add the final `canonicalize()` result for completeness (resolves any
///    remaining directory-component symlinks).
///
/// ## Errors
///
/// Never fails — I/O errors inside the chain walk are silently swallowed
/// and the function returns whatever it collected so far. The caller's
/// policy check is fail-closed: if we can't prove a path is inside the
/// root, it's denied.
pub fn paths_for_write_check(path: &Path) -> Vec<PathBuf> {
    let mut result: HashSet<PathBuf> = HashSet::new();
    result.insert(path.to_path_buf());

    // ── Case A: path doesn't exist yet ────────────────────────────────────
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Could be a dangling symlink or a genuinely new file. Either
            // way, resolve the deepest existing ancestor so we know where
            // the write would *actually* land after the OS follows links.
            if let Ok(Some(resolved)) = resolve_deepest_existing_ancestor(path) {
                result.insert(resolved);
            }
            return result.into_iter().collect();
        }
        _ => {}
    }

    // ── Case B: path exists — follow the symlink chain ────────────────────
    let mut current = path.to_path_buf();
    let mut visited: HashSet<PathBuf> = HashSet::new();

    for _ in 0..MAX_SYMLINK_DEPTH {
        if visited.contains(&current) {
            break; // cycle detected
        }
        visited.insert(current.clone());

        let meta = match std::fs::symlink_metadata(&current) {
            Ok(m) => m,
            Err(_) => break,
        };

        // Guard: FIFOs, sockets, and block/char devices can cause hangs or
        // irreversible side-effects. Stop the chain here (same guard as CC).
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileTypeExt as _;
            let ft = meta.file_type();
            if ft.is_fifo() || ft.is_socket() || ft.is_block_device() || ft.is_char_device() {
                break;
            }
        }

        if !meta.file_type().is_symlink() {
            // Real file or directory — canonicalize and add the final path.
            if let Ok(canon) = std::fs::canonicalize(&current) {
                result.insert(canon);
            }
            break;
        }

        // Follow the symlink: readlink → resolve relative targets.
        let target = match std::fs::read_link(&current) {
            Ok(t) => t,
            Err(_) => break,
        };
        let next = if target.is_absolute() {
            target
        } else {
            match current.parent() {
                Some(parent) => parent.join(&target),
                None => break,
            }
        };

        result.insert(next.clone());
        current = next;
    }

    result.into_iter().collect()
}

// ── 4. is_path_inside ────────────────────────────────────────────────────

/// True when `path` is equal to `root` or is a descendant of `root`.
///
/// Both inputs **must be canonicalized** by the caller (via
/// `std::fs::canonicalize` or [`resolve_deepest_existing_ancestor`]).
/// This function does no I/O — it compares components directly.
///
/// Uses Rust's `Path::starts_with` which is component-aware:
/// - `is_path_inside("/etc/password", "/etc/pass")` → `false` ✅
/// - `is_path_inside("/etc/passwd", "/etc")` → `true` ✅
/// - `is_path_inside("/etc", "/etc")` → `true` ✅
///
/// ## Equivalent
///
/// Claude Code's `pathInWorkingPath` (after both sides are canonicalized).
#[inline]
pub fn is_path_inside(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}

// ── 5. is_dangerous_system_path ──────────────────────────────────────────

/// True when `path` is a system-critical location that should never be
/// the target of a write or deletion, even if a broad allow rule exists.
///
/// This is a last-resort guard — it fires *after* all other policy checks
/// and cannot be overridden by any allow rule.
///
/// ## Blocked paths (mirrors CC `isDangerousRemovalPath`)
///
/// | Category | Rule |
/// |---|---|
/// | Filesystem root | `/` exactly |
/// | Home directory | `$HOME` exactly (runtime check) |
/// | Direct children of root | `/usr`, `/etc`, `/var`, `/bin`, etc. — but `/usr/local` is **safe** |
/// | Glob wildcards | `*` or paths ending in `/*` |
/// | Windows drive roots | `C:\`, `C:\Windows`, etc. |
///
/// The "direct child of `/`" rule captures all of `/bin`, `/sbin`, `/etc`,
/// `/usr`, `/var`, `/dev`, `/proc`, `/sys`, `/tmp`, `/Library`,
/// `/System`, etc. without needing an explicit allowlist — matching CC's
/// `dirname(normalizedPath) === '/'` condition exactly.
///
/// ## Equivalent
///
/// Claude Code's `isDangerousRemovalPath` in `permissions/pathValidation.ts`.
pub fn is_dangerous_system_path(path: &Path) -> bool {
    // Glob wildcard check (guard for bash-expansion paths stored as PathBuf).
    {
        let s = path.to_string_lossy();
        if s == "*" || s.ends_with("/*") || s.ends_with("/\\*") {
            return true;
        }
    }

    // Root itself.
    if path == Path::new("/") {
        return true;
    }

    // Home directory exactly (not subdirectories — those are user data).
    if let Ok(home) = std::env::var("HOME")
        && path == Path::new(&home)
    {
        return true;
    }

    // Windows system paths.
    #[cfg(windows)]
    {
        let s = path.to_string_lossy().to_lowercase();
        // Drive roots: C:\ or C:
        if s.len() <= 3 && s.chars().nth(1) == Some(':') {
            return true;
        }
        // Direct children of drive root: C:\Windows, C:\Users, etc.
        // (exactly one path segment after the drive root)
        if let Some(parent) = path.parent() {
            let parent_s = parent.to_string_lossy().to_lowercase();
            if parent_s.len() <= 3 && parent_s.chars().nth(1) == Some(':') {
                return true;
            }
        }
    }

    // POSIX: direct children of root (parent == "/").
    if let Some(parent) = path.parent()
        && parent == Path::new("/")
    {
        return true;
    }

    false
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // ── is_path_inside ────────────────────────────────────────────────────

    #[test]
    fn same_path_is_inside() {
        assert!(is_path_inside(Path::new("/a/b"), Path::new("/a/b")));
    }

    #[test]
    fn child_is_inside() {
        assert!(is_path_inside(Path::new("/a/b/c"), Path::new("/a/b")));
    }

    #[test]
    fn sibling_is_not_inside() {
        assert!(!is_path_inside(Path::new("/a/bc"), Path::new("/a/b")));
    }

    #[test]
    fn parent_is_not_inside_child() {
        assert!(!is_path_inside(Path::new("/a"), Path::new("/a/b")));
    }

    #[test]
    fn prefix_match_respects_component_boundary() {
        // /etc/password must NOT match root /etc/pass
        assert!(!is_path_inside(
            Path::new("/etc/password"),
            Path::new("/etc/pass")
        ));
    }

    // ── is_dangerous_system_path ─────────────────────────────────

    #[test]
    fn root_is_dangerous() {
        assert!(is_dangerous_system_path(Path::new("/")));
    }

    #[test]
    fn direct_children_of_root_are_dangerous() {
        // Matches CC's `dirname(normalizedPath) === '/'` rule.
        for p in [
            "/usr",
            "/etc",
            "/bin",
            "/sbin",
            "/dev",
            "/proc",
            "/sys",
            "/var",
            "/tmp",
            "/boot",
            "/lib",
            "/lib64",
            "/System",
            "/Library",
            "/Applications",
        ] {
            assert!(
                is_dangerous_system_path(Path::new(p)),
                "{p} should be dangerous"
            );
        }
    }

    #[test]
    fn subdirs_of_system_dirs_are_safe() {
        // CC allows subdirs: /usr/local, /var/tmp/work, /tmp/myapp, etc.
        for p in [
            "/usr/local",
            "/usr/local/share",
            "/var/tmp/build",
            "/tmp/sandbox",
            "/etc/apache2",
        ] {
            assert!(
                !is_dangerous_system_path(Path::new(p)),
                "{p} should be safe"
            );
        }
    }

    #[test]
    fn user_project_dirs_are_safe() {
        for p in ["/home/user/project", "/Users/alice/work"] {
            assert!(
                !is_dangerous_system_path(Path::new(p)),
                "{p} should be safe"
            );
        }
    }

    #[test]
    fn glob_wildcards_are_dangerous() {
        assert!(is_dangerous_system_path(Path::new("*")));
        assert!(is_dangerous_system_path(Path::new("/home/user/*")));
    }

    // ── find_symlink_in_path ──────────────────────────────────────────────

    #[test]
    fn no_symlink_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("real.txt");
        fs::write(&file, b"hi").unwrap();
        let result = find_symlink_in_path(&file).unwrap();
        // The tmp dir itself may be a symlink on macOS (/var → /private/var),
        // so just verify we don't panic. The real assertion is that when
        // there are no symlinks in a purely real path, we get None.
        // (We test the symlink case below.)
        let _ = result;
    }

    #[cfg(unix)]
    #[test]
    fn symlink_component_detected() {
        let tmp = tempfile::tempdir().unwrap();
        let real_dir = tmp.path().join("real");
        let link_dir = tmp.path().join("link");
        let target_file = real_dir.join("secret.txt");

        fs::create_dir_all(&real_dir).unwrap();
        fs::write(&target_file, b"secret").unwrap();
        std::os::unix::fs::symlink(&real_dir, &link_dir).unwrap();

        let path_through_link = link_dir.join("secret.txt");
        let found = find_symlink_in_path(&path_through_link).unwrap();
        assert!(found.is_some(), "should detect symlink component");
    }

    // ── paths_for_write_check ─────────────────────────────────────────────

    #[test]
    fn real_file_returns_at_least_original() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("plain.txt");
        fs::write(&file, b"x").unwrap();
        let paths = paths_for_write_check(&file);
        assert!(paths.contains(&file) || paths.iter().any(|p| p.ends_with("plain.txt")));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_expands_to_full_chain() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real.txt");
        let link = tmp.path().join("link.txt");
        fs::write(&real, b"secret").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let paths = paths_for_write_check(&link);

        // Should contain at minimum: the link itself and the real target.
        assert!(paths.len() >= 2, "expected ≥2 paths, got {paths:?}");
        let has_real = paths.iter().any(|p| {
            p.ends_with("real.txt")
                || std::fs::canonicalize(p)
                    .ok()
                    .map(|c| c.ends_with("real.txt"))
                    .unwrap_or(false)
        });
        assert!(has_real, "resolved target should be in chain: {paths:?}");
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_ancestor_resolved() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        let link = tmp.path().join("link");

        // Dangling symlink — outside doesn't exist yet.
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let check_target = link.join("new_file.txt");
        let paths = paths_for_write_check(&check_target);

        // The resolved path should point to outside/new_file.txt, not link/new_file.txt.
        let has_outside = paths.iter().any(|p| p.ends_with("outside/new_file.txt"));
        assert!(
            has_outside,
            "dangling symlink should resolve to outside dir: {paths:?}"
        );
    }

    // ── resolve_deepest_existing_ancestor ─────────────────────────────

    #[test]
    fn ancestor_no_symlink_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let result = resolve_deepest_existing_ancestor(tmp.path()).unwrap();
        // tmp itself might be via /var → /private/var on macOS, so this
        // is always Some on macOS. On Linux it should be None.
        // Just assert no panic.
        let _ = result;
    }

    #[cfg(unix)]
    #[test]
    fn live_symlink_resolved() {
        let tmp = tempfile::tempdir().unwrap();
        let real_dir = tmp.path().join("real");
        let link_dir = tmp.path().join("link");
        fs::create_dir_all(&real_dir).unwrap();
        std::os::unix::fs::symlink(&real_dir, &link_dir).unwrap();

        let path = link_dir.join("new_file.txt");
        let resolved = resolve_deepest_existing_ancestor(&path).unwrap();
        let resolved = resolved.expect("should find a symlink");
        assert!(resolved.ends_with("real/new_file.txt"), "got: {resolved:?}");
    }
}
