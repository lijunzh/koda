//! [`LocalFileSystem`] — direct, unsandboxed [`FileSystem`] impl
//! (Phase 2b of #934).
//!
//! Uses `tokio::fs` for the IO methods so it stays cooperatively async,
//! and `spawn_blocking` for the CPU/sync-blocking ones (glob expansion,
//! recursive walk for grep). No policy enforcement happens here — that's
//! `SandboxedFileSystem`'s job in Phase 2c. This impl exists for two
//! callers:
//!
//! 1. The `--no-sandbox` debug escape hatch (#934 Phase 2 acceptance).
//! 2. Unit tests for the file tools (Phase 2d) — much faster than
//!    spinning up a worker process per test.
//!
//! ## Why not just use `std::fs` synchronously?
//!
//! The trait has to be `async` for the `SandboxedFileSystem` impl
//! (which round-trips to a worker over IPC). If `LocalFileSystem` used
//! sync `std::fs`, the trait would either need two flavors or each call
//! would block the runtime thread. Using `tokio::fs` keeps both impls
//! shape-compatible.
//!
//! ## Why `spawn_blocking` for glob/grep?
//!
//! Neither `glob` nor `ignore` ship async APIs (recursive directory
//! walks are inherently CPU-bound for large trees anyway), so we
//! offload them to the runtime's blocking pool. Same shape `tokio::fs`
//! uses internally.

use crate::fs::{FileSystem, FsError, FsResult, Metadata};
use crate::ipc::GrepMatch;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use tokio::fs;

/// Unsandboxed, in-process [`FileSystem`].
///
/// Construct with `LocalFileSystem::new()` (or `Default`) — there's
/// nothing to configure. Cheap to clone (zero state).
#[derive(Debug, Default, Clone, Copy)]
pub struct LocalFileSystem;

impl LocalFileSystem {
    /// Construct a new [`LocalFileSystem`].
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl FileSystem for LocalFileSystem {
    async fn read(&self, path: &Path, max_bytes: Option<usize>) -> FsResult<Vec<u8>> {
        let mut buf = fs::read(path).await?;
        if let Some(cap) = max_bytes
            && buf.len() > cap
        {
            buf.truncate(cap);
        }
        Ok(buf)
    }

    async fn write(&self, path: &Path, content: &[u8]) -> FsResult<usize> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            // Match the existing Write tool contract — create any
            // missing parent dirs so the LLM doesn't have to mkdir
            // before writing. `create_dir_all` is a no-op if it
            // already exists.
            fs::create_dir_all(parent).await?;
        }
        fs::write(path, content).await?;
        Ok(content.len())
    }

    async fn edit(
        &self,
        path: &Path,
        old_string: &str,
        new_string: &str,
        all: bool,
    ) -> FsResult<usize> {
        let original = fs::read_to_string(path).await?;
        let (replaced, count) = if all {
            // matches() doesn't construct intermediate strings — count
            // first so we know whether to bail before allocating.
            let n = original.matches(old_string).count();
            if n == 0 {
                return Err(FsError::EditNotFound {
                    path: path.to_path_buf(),
                });
            }
            (original.replace(old_string, new_string), n)
        } else if original.contains(old_string) {
            // Replace exactly one occurrence — `replacen` does
            // exactly this and avoids the second pass `replace` would do.
            (original.replacen(old_string, new_string, 1), 1)
        } else {
            return Err(FsError::EditNotFound {
                path: path.to_path_buf(),
            });
        };
        // `count` is what we return; bind a no-op ref to the buffer
        // so future maintainers don't "clean up" the destructure.
        let _ = &replaced;
        fs::write(path, replaced.as_bytes()).await?;
        Ok(count)
    }

    async fn glob(&self, pattern: &str, root: &Path) -> FsResult<Vec<PathBuf>> {
        // The `glob` crate is sync and walks the tree itself; offload
        // to spawn_blocking so we don't park a runtime worker.
        let pattern = pattern.to_string();
        let root = root.to_path_buf();
        tokio::task::spawn_blocking(move || -> FsResult<Vec<PathBuf>> {
            // Anchor the pattern at `root` unless the caller already
            // supplied an absolute pattern. Matches the existing
            // `Glob` tool's expected behavior.
            let full = if Path::new(&pattern).is_absolute() {
                pattern.clone()
            } else {
                root.join(&pattern).to_string_lossy().into_owned()
            };
            let mut out: Vec<PathBuf> = glob::glob(&full)
                .map_err(|e| FsError::InvalidPattern {
                    message: format!("glob pattern {pattern:?}: {e}"),
                })?
                .filter_map(Result::ok)
                .collect();
            // Sorted output = deterministic for snapshot tests + LLM
            // context stability.
            out.sort();
            Ok(out)
        })
        .await
        .map_err(|e| FsError::Transport {
            message: format!("glob spawn_blocking join error: {e}"),
        })?
    }

    async fn grep(
        &self,
        pattern: &str,
        root: &Path,
        include: Option<&str>,
    ) -> FsResult<Vec<GrepMatch>> {
        let pattern = pattern.to_string();
        let include = include.map(str::to_string);
        let root = root.to_path_buf();
        tokio::task::spawn_blocking(move || -> FsResult<Vec<GrepMatch>> {
            let re = regex::Regex::new(&pattern).map_err(|e| FsError::InvalidPattern {
                message: format!("grep regex {pattern:?}: {e}"),
            })?;
            let include_glob = include
                .as_ref()
                .map(|g| {
                    glob::Pattern::new(g).map_err(|e| FsError::InvalidPattern {
                        message: format!("grep include {g:?}: {e}"),
                    })
                })
                .transpose()?;

            let mut matches = Vec::new();
            // `ignore::Walk` honors .gitignore + .ignore by default —
            // this is exactly what we want for grep so the LLM doesn't
            // get drowned in node_modules/target/.git matches.
            for entry in ignore::Walk::new(&root) {
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => continue, // permission denied on a subdir, etc.
                };
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                let path = entry.path();
                if let Some(g) = &include_glob {
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if !g.matches(name) {
                        continue;
                    }
                }
                // read_to_string fails on binary files (non-UTF8) —
                // skip them silently. Matches `rg`'s default behavior
                // and avoids dumping garbage at the LLM.
                let content = match std::fs::read_to_string(path) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                for (lineno, line) in content.lines().enumerate() {
                    if re.is_match(line) {
                        matches.push(GrepMatch {
                            path: path.to_path_buf(),
                            line: lineno + 1, // 1-based per the IPC contract
                            text: line.to_string(),
                        });
                    }
                }
            }
            Ok(matches)
        })
        .await
        .map_err(|e| FsError::Transport {
            message: format!("grep spawn_blocking join error: {e}"),
        })?
    }

    async fn stat(&self, path: &Path) -> FsResult<Metadata> {
        // symlink_metadata = lstat = does NOT follow the final
        // component. We need this so `is_symlink` is meaningful
        // (regular `metadata` would always report the target's type).
        let m = fs::symlink_metadata(path).await?;
        Ok(Metadata {
            size: m.len(),
            is_dir: m.is_dir(),
            is_symlink: m.file_type().is_symlink(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Cheap helper — every test wants a tempdir + LocalFS pair.
    fn fixture() -> (TempDir, LocalFileSystem) {
        (TempDir::new().expect("tempdir"), LocalFileSystem::new())
    }

    // ── read ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn read_returns_full_contents_when_no_cap() {
        let (dir, fs) = fixture();
        let path = dir.path().join("hello.txt");
        std::fs::write(&path, b"hello world").unwrap();
        let got = fs.read(&path, None).await.unwrap();
        assert_eq!(got, b"hello world");
    }

    #[tokio::test]
    async fn read_truncates_to_max_bytes() {
        let (dir, fs) = fixture();
        let path = dir.path().join("big.txt");
        std::fs::write(&path, b"abcdefghij").unwrap();
        let got = fs.read(&path, Some(4)).await.unwrap();
        assert_eq!(got, b"abcd");
    }

    #[tokio::test]
    async fn read_max_bytes_above_size_returns_full_file() {
        // Cap higher than file size must NOT pad or error — just
        // return what's there.
        let (dir, fs) = fixture();
        let path = dir.path().join("small.txt");
        std::fs::write(&path, b"hi").unwrap();
        let got = fs.read(&path, Some(1024)).await.unwrap();
        assert_eq!(got, b"hi");
    }

    #[tokio::test]
    async fn read_missing_file_returns_io_error() {
        let (dir, fs) = fixture();
        let err = fs
            .read(&dir.path().join("nope"), None)
            .await
            .expect_err("missing file must error");
        assert!(matches!(err, FsError::Io(_)));
    }

    // ── write ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn write_creates_file_and_returns_byte_count() {
        let (dir, fs) = fixture();
        let path = dir.path().join("out.txt");
        let n = fs.write(&path, b"abc").await.unwrap();
        assert_eq!(n, 3);
        assert_eq!(std::fs::read(&path).unwrap(), b"abc");
    }

    #[tokio::test]
    async fn write_creates_missing_parent_dirs() {
        // The Write tool's contract creates parents — LLMs forget to
        // mkdir before writing constantly, so we accept it.
        let (dir, fs) = fixture();
        let path = dir.path().join("a/b/c/deep.txt");
        let n = fs.write(&path, b"hi").await.unwrap();
        assert_eq!(n, 2);
        assert!(path.exists());
    }

    #[tokio::test]
    async fn write_overwrites_existing_file() {
        let (dir, fs) = fixture();
        let path = dir.path().join("over.txt");
        std::fs::write(&path, b"old contents").unwrap();
        fs.write(&path, b"new").await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }

    // ── edit ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn edit_replaces_first_occurrence_when_all_false() {
        let (dir, fs) = fixture();
        let path = dir.path().join("e.txt");
        std::fs::write(&path, b"foo bar foo baz foo").unwrap();
        let n = fs.edit(&path, "foo", "FOO", false).await.unwrap();
        assert_eq!(n, 1);
        assert_eq!(std::fs::read(&path).unwrap(), b"FOO bar foo baz foo");
    }

    #[tokio::test]
    async fn edit_replaces_all_occurrences_when_all_true() {
        let (dir, fs) = fixture();
        let path = dir.path().join("e.txt");
        std::fs::write(&path, b"foo bar foo baz foo").unwrap();
        let n = fs.edit(&path, "foo", "FOO", true).await.unwrap();
        assert_eq!(n, 3);
        assert_eq!(std::fs::read(&path).unwrap(), b"FOO bar FOO baz FOO");
    }

    #[tokio::test]
    async fn edit_missing_old_string_returns_edit_not_found() {
        // Hard fail rather than silent no-op — LLMs are bad at
        // verifying their own context. Surfacing the mismatch early
        // gives them a chance to re-read the file.
        let (dir, fs) = fixture();
        let path = dir.path().join("e.txt");
        std::fs::write(&path, b"hello").unwrap();
        let err = fs
            .edit(&path, "world", "X", false)
            .await
            .expect_err("must fail");
        assert!(matches!(err, FsError::EditNotFound { .. }));
        // File must be unchanged.
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
    }

    // ── glob ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn glob_matches_files_relative_to_root() {
        let (dir, fs) = fixture();
        std::fs::write(dir.path().join("a.rs"), b"").unwrap();
        std::fs::write(dir.path().join("b.rs"), b"").unwrap();
        std::fs::write(dir.path().join("c.txt"), b"").unwrap();
        let got = fs.glob("*.rs", dir.path()).await.unwrap();
        assert_eq!(got.len(), 2);
        // Sorted ordering contract.
        assert!(got[0].file_name().unwrap() < got[1].file_name().unwrap());
    }

    #[tokio::test]
    async fn glob_recursive_pattern_works() {
        let (dir, fs) = fixture();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("top.rs"), b"").unwrap();
        std::fs::write(dir.path().join("sub/inner.rs"), b"").unwrap();
        let got = fs.glob("**/*.rs", dir.path()).await.unwrap();
        assert_eq!(got.len(), 2);
    }

    #[tokio::test]
    async fn glob_invalid_pattern_returns_invalid_pattern() {
        let (dir, fs) = fixture();
        // `[` opens a character class that's never closed — definitely
        // invalid in any glob dialect.
        let err = fs
            .glob("[abc", dir.path())
            .await
            .expect_err("invalid pattern must error");
        assert!(matches!(err, FsError::InvalidPattern { .. }));
    }

    // ── grep ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn grep_finds_matches_with_line_numbers() {
        let (dir, fs) = fixture();
        std::fs::write(dir.path().join("a.txt"), b"hello\nworld\nhello again\n").unwrap();
        let got = fs.grep("hello", dir.path(), None).await.unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].line, 1);
        assert_eq!(got[1].line, 3);
        assert!(got[0].text.contains("hello"));
    }

    #[tokio::test]
    async fn grep_respects_include_filter() {
        let (dir, fs) = fixture();
        std::fs::write(dir.path().join("a.rs"), b"fn target() {}").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"fn target() {}").unwrap();
        let got = fs.grep("target", dir.path(), Some("*.rs")).await.unwrap();
        assert_eq!(got.len(), 1);
        assert!(got[0].path.extension().unwrap() == "rs");
    }

    #[tokio::test]
    async fn grep_skips_binary_files_silently() {
        // Real-world grepping always trips over binary files — we
        // skip rather than error so the LLM gets clean text-only
        // matches.
        let (dir, fs) = fixture();
        std::fs::write(dir.path().join("a.txt"), b"hello text\n").unwrap();
        // Mixed binary content — has a NUL and high bytes — read_to_string
        // will fail on the unpaired surrogate.
        std::fs::write(dir.path().join("b.bin"), b"\x00\xc3\x28hello\x00\xc3\x28").unwrap();
        let got = fs.grep("hello", dir.path(), None).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].path.extension().unwrap(), "txt");
    }

    #[tokio::test]
    async fn grep_invalid_regex_returns_invalid_pattern() {
        let (dir, fs) = fixture();
        std::fs::write(dir.path().join("a.txt"), b"x").unwrap();
        let err = fs
            .grep("[unclosed", dir.path(), None)
            .await
            .expect_err("invalid regex must error");
        assert!(matches!(err, FsError::InvalidPattern { .. }));
    }

    #[tokio::test]
    async fn grep_honors_ignore_files() {
        // The `ignore` crate's default walker reads .ignore (and
        // .gitignore inside a git repo) — that's the whole reason we
        // picked it over plain walkdir. We use .ignore here because it
        // works without initializing a git repo in the tempdir.
        let (dir, fs) = fixture();
        std::fs::write(dir.path().join(".ignore"), b"ignored.txt\n").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), b"target line").unwrap();
        std::fs::write(dir.path().join("kept.txt"), b"target line").unwrap();
        let got = fs.grep("target", dir.path(), None).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].path.file_name().unwrap(), "kept.txt");
    }

    // ── stat ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn stat_reports_file_size_and_type() {
        let (dir, fs) = fixture();
        let path = dir.path().join("s.txt");
        std::fs::write(&path, b"abcdef").unwrap();
        let m = fs.stat(&path).await.unwrap();
        assert_eq!(m.size, 6);
        assert!(!m.is_dir);
        assert!(!m.is_symlink);
    }

    #[tokio::test]
    async fn stat_reports_directory() {
        let (dir, fs) = fixture();
        let m = fs.stat(dir.path()).await.unwrap();
        assert!(m.is_dir);
        assert!(!m.is_symlink);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stat_reports_symlink_without_following() {
        // Critical for the symlink-defense work in Phase 2f — we need
        // to know the path itself is a link, not its target's type.
        let (dir, fs) = fixture();
        let target = dir.path().join("real.txt");
        std::fs::write(&target, b"target").unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let m = fs.stat(&link).await.unwrap();
        assert!(m.is_symlink);
        assert!(!m.is_dir);
    }
}
