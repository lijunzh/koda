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

/// Sessions: messages, turns, settings and keys (`koda_core::db::Database`).
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
    pub async fn open(self, path: &Path) -> Result<SqlitePool> {
        let url = format!("sqlite:{}?mode=rwc", path.display());
        let options = SqliteConnectOptions::from_str(&url)?
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .auto_vacuum(sqlx::sqlite::SqliteAutoVacuum::Incremental)
            .foreign_keys(true)
            .create_if_missing(true)
            // Retry for up to 5 s when another connection holds the write
            // lock. Without this, concurrent writes from parallel sub-agents
            // (#595) return SQLITE_BUSY immediately and the insert is silently
            // dropped. Individual writes are ~1 ms so the retry resolves fast.
            .busy_timeout(std::time::Duration::from_millis(5000));
        SqlitePoolOptions::new()
            .max_connections(self.max_connections)
            .connect_with(options)
            .await
            .with_context(|| format!("Failed to open the {}: {url}", self.label))
    }
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
