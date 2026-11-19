//! How Koda opens its SQLite files.
//!
//! Each kind of stored data has a file of its own under
//! `<config>/db/`, as Codex keeps `state`, `logs` and others apart: a busy
//! or damaged log file can't slow down or harm the sessions. They share how
//! they're opened, which this module holds as one spec per file, as Codex's
//! `RuntimeDbSpec` table does. A version in the file name lets a schema
//! change that can't migrate start a fresh file.

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// One of Koda's SQLite files.
#[derive(Debug, Clone, Copy)]
pub struct DbSpec {
    /// What it is called in errors.
    pub label: &'static str,
    /// Its file name under `<config>/db/`.
    pub filename: &'static str,
    /// The most connections its pool opens.
    pub max_connections: u32,
}

/// Sessions: messages, turns, settings and keys ([`crate::db::Database`]).
pub const SESSIONS_DB: DbSpec = DbSpec {
    label: "session database",
    filename: "koda.db",
    max_connections: 5,
};

/// Diagnostic logs ([`crate::log_db`]), Codex's `logs_2.sqlite`.
pub const LOGS_DB: DbSpec = DbSpec {
    label: "log database",
    filename: "logs_1.db",
    max_connections: 2,
};

impl DbSpec {
    /// Its path under the Koda config directory `config_dir`.
    pub fn path(self, config_dir: &Path) -> PathBuf {
        config_dir.join("db").join(self.filename)
    }

    /// Open it under `config_dir`, creating it and its directory if needed.
    /// Owner-only on Unix: sessions hold keys and conversations, and logs
    /// can quote them.
    pub async fn open_in(self, config_dir: &Path) -> Result<SqlitePool> {
        let path = self.path(config_dir);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("Failed to create DB dir: {}", dir.display()))?;
        }
        let pool = self.open(&path).await?;
        restrict_permissions(&path);
        Ok(pool)
    }

    /// Open it at `path`, as tests do.
    ///
    /// As Codex opens its state database (`codex-rs/state/src/sqlite/
    /// connection.rs`): settings stored in the file (WAL, incremental
    /// auto-vacuum) are set once, on one connection, when the file is new;
    /// every connection gets only the settings that are per connection.
    /// Setting `auto_vacuum` on every connection, as Koda did, writes the
    /// file's header each time, so opening a connection, even to read, took
    /// the write lock and failed when another process held it past the busy
    /// timeout. Opening is retried while SQLite is busy, as Codex retries it.
    pub async fn open(self, path: &Path) -> Result<SqlitePool> {
        let url = format!("sqlite:{}?mode=rwc", path.display());
        let options = SqliteConnectOptions::from_str(&url)?
            // NORMAL, as Codex opens its state database: with WAL, a commit
            // no longer waits for an fsync, only a checkpoint does, so the
            // write lock is held for the write and not the disk. FULL (sqlx's
            // default) held it through every fsync, and on a slow disk that
            // outlasted the busy timeout below: a sub-agent's first write
            // failed with "database is locked" on a starved macOS CI runner
            // (run 2510). The trade: a power loss can lose the last
            // commits, never corrupt the file; a crash of Koda loses nothing.
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
            .foreign_keys(true)
            .create_if_missing(true)
            // Wait up to 5 s when another connection holds the write lock,
            // as Codex does; writes then retry twice more
            // ([`crate::busy_retry`]).
            .busy_timeout(std::time::Duration::from_millis(5000));
        let pool = crate::busy_retry::retry_sqlx(|| {
            SqlitePoolOptions::new()
                .max_connections(self.max_connections)
                .connect_with(options.clone())
        })
        .await
        .with_context(|| format!("Failed to open the {}: {url}", self.label))?;
        crate::busy_retry::retry_sqlx(|| initialize_file(&pool))
            .await
            .with_context(|| format!("Failed to set up the {}: {url}", self.label))?;
        Ok(pool)
    }
}

/// The settings a database file keeps: incremental auto-vacuum, which only a
/// file with no tables yet takes, and WAL. Each needs the write lock, so only
/// a file that lacks them is changed.
async fn initialize_file(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    let mut conn = pool.acquire().await?;
    let empty: bool = sqlx::query_scalar("SELECT NOT EXISTS (SELECT 1 FROM sqlite_schema)")
        .fetch_one(&mut *conn)
        .await?;
    if empty {
        let mode: i64 = sqlx::query_scalar("PRAGMA auto_vacuum")
            .fetch_one(&mut *conn)
            .await?;
        if mode == 0 {
            sqlx::query("PRAGMA auto_vacuum = INCREMENTAL")
                .execute(&mut *conn)
                .await?;
        }
    }
    let journal: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(&mut *conn)
        .await?;
    if journal != "wal" {
        sqlx::query("PRAGMA journal_mode = WAL")
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// Make `path` readable and writable by its owner only (#693).
fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        if let Err(e) = std::fs::set_permissions(path, perms) {
            tracing::warn!("Failed to set 0600 on {}: {e}", path.display());
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WAL with synchronous NORMAL on every connection, as Codex opens its
    /// state database, so a commit holds the write lock only for the write.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_connection_is_wal_with_synchronous_normal() {
        let dir = tempfile::tempdir().unwrap();
        let pool = SESSIONS_DB.open(&dir.path().join("koda.db")).await.unwrap();
        for _ in 0..3 {
            let mut conn = pool.acquire().await.unwrap();
            let (mode,): (String,) = sqlx::query_as("PRAGMA journal_mode")
                .fetch_one(&mut *conn)
                .await
                .unwrap();
            let (sync,): (i64,) = sqlx::query_as("PRAGMA synchronous")
                .fetch_one(&mut *conn)
                .await
                .unwrap();
            assert_eq!(mode, "wal");
            assert_eq!(sync, 1, "1 is NORMAL, 2 FULL");
        }
        let (vacuum,): (i64,) = sqlx::query_as("PRAGMA auto_vacuum")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(vacuum, 2, "a new file is incremental");
    }

    /// Opening the database writes nothing, so it doesn't wait for a writer
    /// in another process. It used to set `auto_vacuum` on every connection,
    /// which writes the file's header: an open behind a held write lock
    /// waited out the busy timeout and failed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn opening_takes_no_write_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("koda.db");
        let first = SESSIONS_DB.open(&path).await.unwrap();
        sqlx::query("CREATE TABLE t (v INTEGER)")
            .execute(&first)
            .await
            .unwrap();
        let mut held = first.begin_with("BEGIN IMMEDIATE").await.unwrap();
        sqlx::query("INSERT INTO t VALUES (1)")
            .execute(&mut *held)
            .await
            .unwrap();

        let started = std::time::Instant::now();
        let second = SESSIONS_DB.open(&path).await.unwrap();
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM t")
            .fetch_one(&second)
            .await
            .unwrap();
        assert_eq!(n, 0, "the held write isn't committed");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "opening waited {:?} for the write lock",
            started.elapsed()
        );
        held.rollback().await.unwrap();
    }
}
