//! File lifecycle tracker — tracks files created by Koda during a session.
//!
//! Inspired by Rust's ownership model (#465):
//! - **Ownership**: files created via `Write` are "owned" by the session.
//! - **Auto-approve cleanup**: deleting an owned file skips the destructive
//!   confirmation gate (the net effect is zero — Koda created it, Koda removes it).
//! - **Persistence**: state is backed by SQLite so it survives compaction,
//!   token limits, and process crashes.
//!
//! Ownership is deliberately narrow: only `Write` (create) confers ownership.
//! `Edit` of a user's file does not. Editing an already-owned file preserves
//! ownership (Koda still created the file).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::db::Database;

/// Tracks files created by Koda in the current session.
///
/// In-memory `HashSet` for fast lookups, with DB persistence for
/// crash recovery and session resume.
#[derive(Debug)]
pub struct FileTracker {
    /// Files owned (created) by Koda in this session.
    owned: HashSet<PathBuf>,
    /// Session ID for DB persistence.
    session_id: String,
    /// Database handle.
    db: Database,
}

impl FileTracker {
    /// Create a new tracker, loading any persisted state from a previous run.
    pub async fn new(session_id: &str, db: Database) -> Self {
        let owned = db.load_owned_files(session_id).await.unwrap_or_default();
        Self {
            owned,
            session_id: session_id.to_string(),
            db,
        }
    }

    /// Record that Koda created a file via `Write`.
    ///
    /// The path should be the resolved absolute path.
    pub async fn track_created(&mut self, path: PathBuf) {
        if self.owned.insert(path.clone()) {
            let _ = self.db.insert_owned_file(&self.session_id, &path).await;
        }
    }

    /// Remove a file from the owned set (after successful deletion).
    pub async fn untrack(&mut self, path: &Path) {
        if self.owned.remove(path) {
            let _ = self.db.delete_owned_file(&self.session_id, path).await;
        }
    }

    /// Check whether Koda owns (created) this file.
    ///
    /// Used by the approval system to auto-approve deletion of
    /// files that Koda itself created.
    pub fn is_owned(&self, path: &Path) -> bool {
        self.owned.contains(path)
    }

    /// Return the number of currently owned files (for diagnostics).
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.owned.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::Persistence;
    use tempfile::TempDir;

    async fn test_db() -> (Database, TempDir) {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("test.db")).await.unwrap();
        db.create_session("test-agent", dir.path()).await.unwrap();
        (db, dir)
    }

    #[tokio::test]
    async fn track_and_check_ownership() {
        let (db, _dir) = test_db().await;
        let mut tracker = FileTracker::new("test-session", db).await;

        let path = PathBuf::from("/tmp/koda_test_file.md");
        assert!(!tracker.is_owned(&path));

        tracker.track_created(path.clone()).await;
        assert!(tracker.is_owned(&path));
        assert_eq!(tracker.len(), 1);
    }

    #[tokio::test]
    async fn untrack_removes_ownership() {
        let (db, _dir) = test_db().await;
        let mut tracker = FileTracker::new("test-session", db).await;

        let path = PathBuf::from("/tmp/koda_test_file.md");
        tracker.track_created(path.clone()).await;
        assert!(tracker.is_owned(&path));

        tracker.untrack(&path).await;
        assert!(!tracker.is_owned(&path));
        assert_eq!(tracker.len(), 0);
    }

    #[tokio::test]
    async fn persists_across_tracker_instances() {
        let (db, _dir) = test_db().await;
        let session_id = "persist-test";
        let path = PathBuf::from("/tmp/koda_persist.md");

        // Create and track
        {
            let mut tracker = FileTracker::new(session_id, db.clone()).await;
            tracker.track_created(path.clone()).await;
        }

        // New tracker for same session — should see the file
        {
            let tracker = FileTracker::new(session_id, db.clone()).await;
            assert!(tracker.is_owned(&path));
        }
    }

    #[tokio::test]
    async fn different_sessions_isolated() {
        let (db, _dir) = test_db().await;
        let path = PathBuf::from("/tmp/koda_isolated.md");

        let mut tracker_a = FileTracker::new("session-a", db.clone()).await;
        tracker_a.track_created(path.clone()).await;

        let tracker_b = FileTracker::new("session-b", db).await;
        assert!(!tracker_b.is_owned(&path));
    }

    #[tokio::test]
    async fn duplicate_track_is_idempotent() {
        let (db, _dir) = test_db().await;
        let mut tracker = FileTracker::new("test-session", db).await;

        let path = PathBuf::from("/tmp/koda_dup.md");
        tracker.track_created(path.clone()).await;
        tracker.track_created(path.clone()).await;
        assert_eq!(tracker.len(), 1);
    }

    #[tokio::test]
    async fn untrack_nonexistent_is_noop() {
        let (db, _dir) = test_db().await;
        let mut tracker = FileTracker::new("test-session", db).await;

        let path = PathBuf::from("/tmp/never_tracked.md");
        tracker.untrack(&path).await; // should not panic
        assert_eq!(tracker.len(), 0);
    }
}
