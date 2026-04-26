//! Filesystem abstraction trait for sandboxed and unsandboxed file
//! operations (Phase 2b of #934).
//!
//! [`FileSystem`] is the seam between the file tools (Read, Write, Edit,
//! MultiEdit, Glob, Grep — currently in `koda-core::tools`) and the
//! actual filesystem they operate on. Two implementations land in this
//! crate:
//!
//! - [`LocalFileSystem`] — direct `tokio::fs`, no policy enforcement.
//!   Used outside the sandbox (the `--no-sandbox` debug escape hatch
//!   per #934 §6 Phase 2 acceptance) and as the in-process baseline
//!   that 2d's tool migration regressions get measured against.
//! - `SandboxedFileSystem` (Phase 2c) — sends each call to a
//!   `koda-fs-worker` over IPC; the worker enforces `SandboxPolicy`
//!   in code on every request.
//!
//! Both impls satisfy the same trait, so the migrated tools (Phase 2d)
//! get dependency-injected with whichever one matches the current
//! sandbox trust mode.
//!
//! ## Why a trait at all?
//!
//! 1. **Same tool code, two backends.** Read/Write/Edit don't change
//!    based on whether they go through the sandbox or not — the only
//!    thing that changes is the FS object they hold. Without a trait,
//!    every tool would `if sandbox { … } else { … }` itself.
//! 2. **Testability.** Tools can be unit-tested against
//!    [`LocalFileSystem`] backed by a `tempfile::TempDir` without
//!    spawning a worker process.
//! 3. **`--no-sandbox` debug mode.** When the user explicitly opts
//!    out (single-trace debugging, mostly), we hand them
//!    [`LocalFileSystem`] and the tools work unchanged.
//!
//! ## Why not put this in `koda-core::tools`?
//!
//! The original #934 sketch suggested koda-core. But koda-core already
//! depends on koda-sandbox (for the `sandbox::build` shim used by
//! `tools::shell`), and the natural Phase 2c implementation
//! (`SandboxedFileSystem`) needs to live next to the worker code in
//! koda-sandbox. Putting the trait in koda-core would force either
//! a dep cycle or a third "interface" crate (overkill at this stage).
//! The trait stays here; koda-core imports it for tool migration in 2d.
//!
//! ## Error model
//!
//! [`FsError`] is intentionally coarse — fine-grained classification
//! belongs at the calling tool's level (e.g. "path not found" → user
//! message vs. "policy denied" → "this path is outside your write
//! permissions"). The IPC error code wire enum
//! ([`crate::ipc::ErrorCode`]) maps 1:1 to FsError variants in 2c.

use crate::ipc::GrepMatch;
use async_trait::async_trait;
use std::path::{Path, PathBuf};

pub mod local;
#[cfg(unix)]
pub mod sandboxed;

pub use local::LocalFileSystem;
#[cfg(unix)]
pub use sandboxed::SandboxedFileSystem;

/// Abstraction over filesystem operations needed by the file tools.
///
/// All methods are `async` because the `SandboxedFileSystem` impl
/// (Phase 2c) round-trips each call to a worker process. The
/// [`LocalFileSystem`] impl uses `tokio::fs` to keep the same
/// signatures and avoid a sync/async split at the call sites.
///
/// `Send + Sync` are required because the file tools are dispatched
/// by an async runtime that may move the future across threads.
#[async_trait]
pub trait FileSystem: Send + Sync {
    /// Read file contents.
    ///
    /// `max_bytes` caps the returned buffer; `None` reads the whole
    /// file (still bounded by [`crate::ipc::MAX_PAYLOAD_BYTES`] on the
    /// sandboxed impl). Tools should pass a tighter cap when the LLM
    /// asks for a head/tail slice.
    async fn read(&self, path: &Path, max_bytes: Option<usize>) -> FsResult<Vec<u8>>;

    /// Overwrite a file with `content`. Creates parent directories if
    /// they don't exist (matches the `Write` tool's current contract).
    /// Returns the number of bytes written.
    async fn write(&self, path: &Path, content: &[u8]) -> FsResult<usize>;

    /// Replace the first occurrence (or all occurrences if `all = true`)
    /// of `old_string` with `new_string` in the file. Returns how many
    /// substitutions happened.
    ///
    /// Errors with [`FsError::EditNotFound`] if `old_string` doesn't
    /// appear in the file — matches the existing `Edit` tool semantics
    /// in `koda-core::tools::file_tools` (LLMs are surprisingly bad at
    /// asserting their own context, so a hard fail beats a silent no-op).
    async fn edit(
        &self,
        path: &Path,
        old_string: &str,
        new_string: &str,
        all: bool,
    ) -> FsResult<usize>;

    /// Expand a glob pattern relative to `root`. Returns matches in
    /// deterministic (sorted) order.
    async fn glob(&self, pattern: &str, root: &Path) -> FsResult<Vec<PathBuf>>;

    /// Recursive regex grep starting at `root`. `include` is an
    /// optional file-glob filter (e.g. `*.rs`).
    ///
    /// Honors `.gitignore` / `.ignore` (via the `ignore` crate's
    /// default walker config) so we don't drown the LLM in matches
    /// from `node_modules/`.
    async fn grep(
        &self,
        pattern: &str,
        root: &Path,
        include: Option<&str>,
    ) -> FsResult<Vec<GrepMatch>>;

    /// `stat`-like metadata fetch. Follows symlinks (use the
    /// `is_symlink` field on the result to detect when the original
    /// path *was* a link).
    async fn stat(&self, path: &Path) -> FsResult<Metadata>;
}

/// Subset of `std::fs::Metadata` we care about — small and
/// serializable so the IPC layer can ship it across the wire
/// unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metadata {
    /// File size in bytes (0 for directories).
    pub size: u64,
    /// True if the path is a directory.
    pub is_dir: bool,
    /// True if the path itself is a symlink (not its target).
    pub is_symlink: bool,
}

/// Result alias used by every [`FileSystem`] method.
pub type FsResult<T> = Result<T, FsError>;

/// Errors a [`FileSystem`] call can return.
///
/// Coarse on purpose — the file tools translate these into user-facing
/// messages, and richer classification happens in the IPC layer
/// ([`crate::ipc::ErrorCode`]).
#[derive(Debug)]
pub enum FsError {
    /// Underlying IO error (file not found, permission denied, …).
    Io(std::io::Error),
    /// Sandbox policy refused the operation. Only the
    /// `SandboxedFileSystem` impl (Phase 2c) returns this;
    /// [`LocalFileSystem`] never does.
    PolicyDenied {
        /// Human-readable explanation suitable for showing the LLM.
        message: String,
    },
    /// `Edit` couldn't find `old_string` in the file.
    EditNotFound {
        /// Path the substring was looked for in.
        path: PathBuf,
    },
    /// Invalid glob/regex/etc. supplied by the caller.
    InvalidPattern {
        /// Human-readable explanation of what was wrong.
        message: String,
    },
    /// Worker process / IPC transport blew up
    /// (`SandboxedFileSystem` only).
    Transport {
        /// Detail describing the transport failure.
        message: String,
    },
}

impl std::fmt::Display for FsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FsError::Io(e) => write!(f, "io error: {e}"),
            FsError::PolicyDenied { message } => write!(f, "policy denied: {message}"),
            FsError::EditNotFound { path } => {
                write!(f, "old_string not found in {}", path.display())
            }
            FsError::InvalidPattern { message } => write!(f, "invalid pattern: {message}"),
            FsError::Transport { message } => write!(f, "fs worker transport: {message}"),
        }
    }
}

impl std::error::Error for FsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FsError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for FsError {
    fn from(e: std::io::Error) -> Self {
        FsError::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _; // for .source()

    // ── Display ──────────────────────────────────────────────────────────
    // Pin the exact wire strings so a message change during refactor
    // gets caught here rather than silently reaching the LLM.

    #[test]
    fn display_io() {
        let e = FsError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "nope"));
        assert_eq!(e.to_string(), "io error: nope");
    }

    #[test]
    fn display_policy_denied() {
        let e = FsError::PolicyDenied {
            message: "outside write root".into(),
        };
        assert_eq!(e.to_string(), "policy denied: outside write root");
    }

    #[test]
    fn display_edit_not_found() {
        let e = FsError::EditNotFound {
            path: PathBuf::from("/tmp/foo.rs"),
        };
        assert_eq!(e.to_string(), "old_string not found in /tmp/foo.rs");
    }

    #[test]
    fn display_invalid_pattern() {
        let e = FsError::InvalidPattern {
            message: "unclosed bracket".into(),
        };
        assert_eq!(e.to_string(), "invalid pattern: unclosed bracket");
    }

    #[test]
    fn display_transport() {
        let e = FsError::Transport {
            message: "broken pipe".into(),
        };
        assert_eq!(e.to_string(), "fs worker transport: broken pipe");
    }

    // ── source() ─────────────────────────────────────────────────────────
    // Only the Io variant chains to an underlying cause; all others are
    // leaf errors.

    #[test]
    fn source_io_returns_some() {
        let inner = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let e = FsError::Io(inner);
        assert!(e.source().is_some());
    }

    #[test]
    fn source_non_io_variants_return_none() {
        let variants: &[FsError] = &[
            FsError::PolicyDenied {
                message: "x".into(),
            },
            FsError::EditNotFound {
                path: PathBuf::from("/x"),
            },
            FsError::InvalidPattern {
                message: "x".into(),
            },
            FsError::Transport {
                message: "x".into(),
            },
        ];
        for v in variants {
            assert!(v.source().is_none(), "{v} should have no source");
        }
    }

    // ── From<io::Error> ──────────────────────────────────────────────────

    #[test]
    fn from_io_error_wraps_in_io_variant() {
        let io_err = std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out");
        let fs_err: FsError = io_err.into();
        // Display must chain through — if the variant is wrong, the
        // message won't contain the io error text.
        assert!(fs_err.to_string().contains("timed out"), "got: {fs_err}");
        assert!(fs_err.source().is_some(), "Io variant must chain source");
    }
}
