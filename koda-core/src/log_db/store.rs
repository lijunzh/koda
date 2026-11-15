//! The log database: `<config>/db/logs_1.db`, Codex's `logs` table and
//! limits with `session_id` where Codex has `thread_id`.

use super::{LogEntry, LogQuery, LogRow};
use crate::db::sqlite::LOGS_DB;
use anyhow::Result;
use sqlx::sqlite::SqlitePool;
use sqlx::{QueryBuilder, Row, Sqlite, SqliteConnection};
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// Log content kept per session, and per process for lines outside any
/// session, as Codex keeps per thread: 10 MiB and 1,000 rows, newest first.
pub const PARTITION_BYTES: i64 = 10 * 1024 * 1024;
/// Rows kept per session, and per process outside any session.
pub const PARTITION_ROWS: i64 = 1_000;
/// How long a line is kept: 10 days, as Codex.
pub const RETENTION_SECS: i64 = 10 * 24 * 60 * 60;
/// The most the file holds before the oldest lines go early: 64 MiB.
pub const BUDGET_BYTES: i64 = 64 * 1024 * 1024;
/// How often a running Koda prunes by age and size, as Codex: 30 minutes.
const MAINTENANCE_PERIOD: Duration = Duration::from_secs(30 * 60);

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS logs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts INTEGER NOT NULL,
    ts_nanos INTEGER NOT NULL,
    level TEXT NOT NULL,
    target TEXT NOT NULL,
    body TEXT,
    module_path TEXT,
    file TEXT,
    line INTEGER,
    session_id TEXT,
    process_uuid TEXT,
    estimated_bytes INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_logs_ts ON logs(ts DESC, ts_nanos DESC, id DESC);
CREATE INDEX IF NOT EXISTS idx_logs_session_id ON logs(session_id);
CREATE INDEX IF NOT EXISTS idx_logs_session_id_ts
    ON logs(session_id, ts DESC, ts_nanos DESC, id DESC);
CREATE INDEX IF NOT EXISTS idx_logs_process_uuid_sessionless_ts
    ON logs(process_uuid, ts DESC, ts_nanos DESC, id DESC)
    WHERE session_id IS NULL;
"#;

/// Koda's diagnostic logs, in a SQLite file of their own.
#[derive(Debug, Clone)]
pub struct LogStore {
    pool: SqlitePool,
}

impl LogStore {
    /// Open the log database under the Koda config directory `config_dir`.
    pub async fn open_in(config_dir: &Path) -> Result<Self> {
        Self::with_pool(LOGS_DB.open_in(config_dir).await?).await
    }

    /// Open the log database at `path`.
    pub async fn open(path: &Path) -> Result<Self> {
        Self::with_pool(LOGS_DB.open(path).await?).await
    }

    async fn with_pool(pool: SqlitePool) -> Result<Self> {
        sqlx::raw_sql(SCHEMA).execute(&pool).await?;
        Ok(Self { pool })
    }

    /// Store `entries`, then trim each session (or, outside sessions, each
    /// process) they touched to its limits, in the same transaction, so no
    /// reader sees a session over them.
    pub async fn insert(&self, entries: &[LogEntry]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await?;
        // SQLite allows 32,766 bound values; 11 per row.
        for chunk in entries.chunks(2_000) {
            let mut insert = QueryBuilder::<Sqlite>::new(
                "INSERT INTO logs (ts, ts_nanos, level, target, body, module_path, file, line, \
                 session_id, process_uuid, estimated_bytes) ",
            );
            insert.push_values(chunk, |mut row, entry| {
                row.push_bind(entry.ts)
                    .push_bind(entry.ts_nanos)
                    .push_bind(&entry.level)
                    .push_bind(&entry.target)
                    .push_bind(&entry.body)
                    .push_bind(&entry.module_path)
                    .push_bind(&entry.file)
                    .push_bind(entry.line)
                    .push_bind(&entry.session_id)
                    .push_bind(&entry.process_uuid)
                    .push_bind(entry.estimated_bytes());
            });
            insert.build().execute(&mut *tx).await?;
        }

        let sessions: BTreeSet<&str> = entries
            .iter()
            .filter_map(|e| e.session_id.as_deref())
            .collect();
        trim(&mut tx, Partition::Session, &sessions).await?;
        let processes: BTreeSet<&str> = entries
            .iter()
            .filter(|e| e.session_id.is_none())
            .filter_map(|e| e.process_uuid.as_deref())
            .collect();
        trim(&mut tx, Partition::Sessionless, &processes).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Drop lines older than [`RETENTION_SECS`], then, while the file holds
    /// more than [`BUDGET_BYTES`], halve the age kept, as Codex does. The
    /// newest second stays whatever its size.
    pub async fn prune(&self, now_secs: i64) -> Result<()> {
        let mut keep_secs = RETENTION_SECS;
        loop {
            sqlx::query("DELETE FROM logs WHERE ts < ?")
                .bind(now_secs.saturating_sub(keep_secs))
                .execute(&self.pool)
                .await?;
            // Free pages are reused, not returned, so they don't count.
            let used: i64 = sqlx::query_scalar(
                "SELECT (page_count - freelist_count) * page_size \
                 FROM pragma_page_count(), pragma_freelist_count(), pragma_page_size()",
            )
            .fetch_one(&self.pool)
            .await?;
            if used <= BUDGET_BYTES || keep_secs == 1 {
                return Ok(());
            }
            keep_secs = (keep_secs / 2).max(1);
        }
    }

    /// Prune now and then every 30 minutes while this store is in use, as
    /// Codex's log maintenance does.
    pub fn start_maintenance(self: &Arc<Self>) {
        let store = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut every = tokio::time::interval(MAINTENANCE_PERIOD);
            every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut first = true;
            loop {
                every.tick().await;
                let Some(store) = store.upgrade() else { break };
                if store.pool.is_closed() {
                    break;
                }
                if let Err(e) = store.prune(super::now().0).await {
                    tracing::warn!("couldn't prune the log database: {e:#}");
                }
                if first {
                    // Fold the WAL left by earlier runs back in, without
                    // waiting on readers or writers.
                    let _ = sqlx::query("PRAGMA wal_checkpoint(PASSIVE)")
                        .execute(&store.pool)
                        .await;
                    first = false;
                }
            }
        });
    }

    /// The lines `query` selects.
    pub async fn query(&self, query: &LogQuery) -> Result<Vec<LogRow>> {
        let mut select = QueryBuilder::<Sqlite>::new(
            "SELECT id, ts, ts_nanos, level, target, body, session_id, process_uuid, file, line \
             FROM logs WHERE 1 = 1",
        );
        filters(&mut select, query);
        select.push(if query.newest_first {
            " ORDER BY id DESC"
        } else {
            " ORDER BY id ASC"
        });
        if let Some(limit) = query.limit {
            select.push(" LIMIT ").push_bind(limit as i64);
        }
        Ok(select
            .build_query_as::<LogRow>()
            .fetch_all(&self.pool)
            .await?)
    }
}

/// How lines are grouped for their limits.
#[derive(Clone, Copy)]
enum Partition {
    /// By `session_id`.
    Session,
    /// Lines with no session, by `process_uuid`.
    Sessionless,
}

impl Partition {
    fn column(self) -> &'static str {
        match self {
            Self::Session => "session_id",
            Self::Sessionless => "process_uuid",
        }
    }

    fn scope(self) -> &'static str {
        match self {
            Self::Session => "",
            Self::Sessionless => "session_id IS NULL AND ",
        }
    }
}

/// Trim each of `keys`' partitions to [`PARTITION_BYTES`] and
/// [`PARTITION_ROWS`], newest first. A cheap check first finds the ones
/// over, as Codex's does, so the window query runs only for those.
async fn trim(
    tx: &mut SqliteConnection,
    partition: Partition,
    keys: &BTreeSet<&str>,
) -> Result<()> {
    if keys.is_empty() {
        return Ok(());
    }
    let column = partition.column();
    let mut over = QueryBuilder::<Sqlite>::new(format!(
        "SELECT {column} AS key FROM logs WHERE {}{column} IN (",
        partition.scope()
    ));
    {
        let mut list = over.separated(", ");
        for key in keys {
            list.push_bind(*key);
        }
    }
    over.push(format!(
        ") GROUP BY {column} HAVING SUM(estimated_bytes) > "
    ))
    .push_bind(PARTITION_BYTES)
    .push(" OR COUNT(*) > ")
    .push_bind(PARTITION_ROWS);
    let over: Vec<String> = over
        .build()
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .map(|row| row.try_get("key"))
        .collect::<Result<_, _>>()?;
    if over.is_empty() {
        return Ok(());
    }
    let mut delete = QueryBuilder::<Sqlite>::new(format!(
        "DELETE FROM logs WHERE id IN (SELECT id FROM (SELECT id, \
         SUM(estimated_bytes) OVER (PARTITION BY {column} \
             ORDER BY ts DESC, ts_nanos DESC, id DESC ROWS UNBOUNDED PRECEDING) AS bytes, \
         ROW_NUMBER() OVER (PARTITION BY {column} \
             ORDER BY ts DESC, ts_nanos DESC, id DESC) AS row_number \
         FROM logs WHERE {}{column} IN (",
        partition.scope()
    ));
    {
        let mut list = delete.separated(", ");
        for key in &over {
            list.push_bind(key);
        }
    }
    delete
        .push(")) WHERE bytes > ")
        .push_bind(PARTITION_BYTES)
        .push(" OR row_number > ")
        .push_bind(PARTITION_ROWS)
        .push(")");
    delete.build().execute(&mut *tx).await?;
    Ok(())
}

fn filters(select: &mut QueryBuilder<Sqlite>, query: &LogQuery) {
    if !query.levels.is_empty() {
        select.push(" AND UPPER(level) IN (");
        let mut list = select.separated(", ");
        for level in &query.levels {
            list.push_bind(level.to_uppercase());
        }
        select.push(")");
    }
    if let Some(from) = query.from_ts {
        select.push(" AND ts >= ").push_bind(from);
    }
    if let Some(to) = query.to_ts {
        select.push(" AND ts <= ").push_bind(to);
    }
    if !query.targets.is_empty() {
        select.push(" AND (");
        for (i, target) in query.targets.iter().enumerate() {
            if i > 0 {
                select.push(" OR ");
            }
            select.push("target LIKE ").push_bind(format!("{target}%"));
        }
        select.push(")");
    }
    if !query.session_ids.is_empty() || query.sessionless {
        select.push(" AND (");
        let mut first = true;
        for id in &query.session_ids {
            if !first {
                select.push(" OR ");
            }
            select.push("session_id = ").push_bind(id.clone());
            first = false;
        }
        if query.sessionless {
            if !first {
                select.push(" OR ");
            }
            select.push("session_id IS NULL");
        }
        select.push(")");
    }
    if let Some(process) = &query.process_uuid {
        select
            .push(" AND process_uuid = ")
            .push_bind(process.clone());
    }
    if let Some(after) = query.after_id {
        select.push(" AND id > ").push_bind(after);
    }
    if let Some(text) = &query.search {
        select
            .push(" AND INSTR(COALESCE(body, ''), ")
            .push_bind(text.clone())
            .push(") > 0");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(session: Option<&str>, ts: i64, body: &str) -> LogEntry {
        LogEntry {
            ts,
            ts_nanos: 0,
            level: "INFO".into(),
            target: "koda_core::inference".into(),
            body: Some(body.into()),
            module_path: None,
            file: None,
            line: None,
            session_id: session.map(str::to_string),
            process_uuid: Some("pid:1:a".into()),
        }
    }

    async fn store() -> (tempfile::TempDir, LogStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = LogStore::open_in(dir.path()).await.unwrap();
        (dir, store)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lines_are_read_back_by_session_and_filter() {
        let (dir, store) = store().await;
        assert!(dir.path().join("db/logs_1.db").exists());
        let mut warn = entry(Some("s1"), 2, "model call purpose=turn");
        warn.level = "WARN".into();
        store
            .insert(&[entry(Some("s1"), 1, "one"), warn, entry(None, 3, "startup")])
            .await
            .unwrap();

        let s1 = store
            .query(&LogQuery {
                session_ids: vec!["s1".into()],
                ..LogQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(s1.len(), 2);
        let warnings = store
            .query(&LogQuery {
                levels: vec!["warn".into()],
                ..LogQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(warnings.len(), 1);
        let found = store
            .query(&LogQuery {
                search: Some("purpose=turn".into()),
                ..LogQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
        let with_startup = store
            .query(&LogQuery {
                session_ids: vec!["s1".into()],
                sessionless: true,
                after_id: Some(s1[0].id),
                ..LogQuery::default()
            })
            .await
            .unwrap();
        let bodies: Vec<_> = with_startup
            .iter()
            .map(|r| r.body.clone().unwrap())
            .collect();
        assert_eq!(bodies, ["model call purpose=turn", "startup"]);
    }

    /// A session keeps its newest 1,000 lines; another session and lines
    /// outside sessions aren't touched by its trimming.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_session_keeps_its_newest_rows() {
        let (_dir, store) = store().await;
        store
            .insert(&[entry(Some("other"), 0, "kept")])
            .await
            .unwrap();
        let many: Vec<_> = (0..PARTITION_ROWS + 5)
            .map(|i| entry(Some("s1"), i, &format!("line {i}")))
            .collect();
        store.insert(&many).await.unwrap();
        let s1 = store
            .query(&LogQuery {
                session_ids: vec!["s1".into()],
                ..LogQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(s1.len() as i64, PARTITION_ROWS);
        assert_eq!(s1[0].body.as_deref(), Some("line 5"));
        let other = store
            .query(&LogQuery {
                session_ids: vec!["other".into()],
                ..LogQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(other.len(), 1);
    }

    /// A session over 10 MiB loses its oldest lines, by bytes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_session_keeps_its_newest_bytes() {
        let (_dir, store) = store().await;
        let big = "x".repeat(4 * 1024 * 1024);
        let rows: Vec<_> = (0..4).map(|i| entry(Some("s1"), i, &big)).collect();
        store.insert(&rows).await.unwrap();
        let kept = store
            .query(&LogQuery {
                session_ids: vec!["s1".into()],
                ..LogQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(kept.len(), 2, "two 4 MiB lines fit in 10 MiB");
        assert_eq!(kept[0].ts, 2);
    }

    /// Lines older than ten days go.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn old_lines_are_pruned() {
        let (_dir, store) = store().await;
        let now = 1_000_000_000;
        store
            .insert(&[
                entry(Some("s1"), now - RETENTION_SECS - 1, "old"),
                entry(Some("s1"), now - 60, "new"),
            ])
            .await
            .unwrap();
        store.prune(now).await.unwrap();
        let left = store.query(&LogQuery::default()).await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].body.as_deref(), Some("new"));
    }
}
