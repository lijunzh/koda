//! Writing to SQLite when another connection, or another process, holds the
//! write lock: Codex's rules (`codex-rs/state/src/sqlite/busy_retry.rs`,
//! #54369).
//!
//! SQLite in WAL mode lets one connection write at a time. Every connection
//! waits up to five seconds for the lock (`busy_timeout`), but two cases
//! still fail with "database is locked":
//!
//! - **A transaction that reads, then writes.** A plain `BEGIN` takes a read
//!   snapshot; if another connection commits before the first write, SQLite
//!   can't upgrade the snapshot and fails at once, without waiting. So every
//!   transaction that writes starts with `BEGIN IMMEDIATE`
//!   ([`PoolWriteExt::begin_immediate`]), taking the write lock first.
//! - **A wait longer than five seconds,** when a slow disk keeps another
//!   writer busy. So a write that got "busy" is tried twice more, 100 and
//!   200 ms later ([`WriteQueryExt::execute_write`], [`retry_on_busy`]).
//!
//! What is retried is always a whole unit: opening a transaction (before it
//! did anything), one autocommit statement, or one operation that is a
//! single transaction. A transaction already under way is never replayed,
//! and an error that isn't "busy", a cancellation among them, is never
//! retried: a commit whose answer was lost may have happened.

use std::future::Future;
use std::time::Duration;

use sqlx::sqlite::{SqliteArguments, SqliteQueryResult};
use sqlx::{Execute, Sqlite, SqlitePool, Transaction};

/// The pauses between attempts: Codex's two retries.
const RETRY_DELAYS: [Duration; 2] = [Duration::from_millis(100), Duration::from_millis(200)];

/// SQLite's primary result code for a busy database. The extended codes
/// (`SQLITE_BUSY_RECOVERY`, `_SNAPSHOT`, `_TIMEOUT`) keep it in their low byte.
const SQLITE_BUSY: i32 = 5;

/// Opening a transaction that will write.
pub trait PoolWriteExt {
    /// `BEGIN IMMEDIATE`: the write lock before any read, so the transaction
    /// waits its turn rather than failing when it first writes. Only opening
    /// it is retried.
    fn begin_immediate(
        &self,
    ) -> impl Future<Output = Result<Transaction<'static, Sqlite>, sqlx::Error>> + Send;
}

impl PoolWriteExt for SqlitePool {
    async fn begin_immediate(&self) -> Result<Transaction<'static, Sqlite>, sqlx::Error> {
        retry_when(|| self.begin_with("BEGIN IMMEDIATE"), is_busy).await
    }
}

/// One autocommit write on the pool.
pub trait WriteQueryExt<'q> {
    /// Run this statement on `pool`, again with the same values if SQLite
    /// was busy. Only for a single statement outside a transaction; inside
    /// one, keep the transaction's connection.
    fn execute_write<'e>(
        self,
        pool: &'e SqlitePool,
    ) -> impl Future<Output = Result<SqliteQueryResult, sqlx::Error>> + Send + 'e
    where
        'q: 'e;
}

impl<'q> WriteQueryExt<'q> for sqlx::query::Query<'q, Sqlite, SqliteArguments> {
    async fn execute_write<'e>(
        mut self,
        pool: &'e SqlitePool,
    ) -> Result<SqliteQueryResult, sqlx::Error>
    where
        'q: 'e,
    {
        let persistent = Execute::persistent(&self);
        let arguments = self
            .take_arguments()
            .map_err(sqlx::Error::Encode)?
            .unwrap_or_default();
        let sql = self.sql();
        retry_when(
            || {
                sqlx::query_with(sql.clone(), arguments.clone())
                    .persistent(persistent)
                    .execute(pool)
            },
            is_busy,
        )
        .await
    }
}

/// Run `operation` again, up to twice, while it fails because SQLite was
/// busy. `operation` must be one transaction (or a run of statements that
/// can each be repeated) and must drop or roll back what it began before
/// returning an error.
pub async fn retry_on_busy<T, F, Fut>(operation: F) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    retry_when(operation, |error: &anyhow::Error| {
        error
            .chain()
            .filter_map(|cause| cause.downcast_ref::<sqlx::Error>())
            .any(is_busy)
    })
    .await
}

/// [`retry_on_busy`] for an operation that returns sqlx's error: opening a
/// pool, or one transaction.
pub async fn retry_sqlx<T, F, Fut>(operation: F) -> Result<T, sqlx::Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, sqlx::Error>>,
{
    retry_when(operation, is_busy).await
}

async fn retry_when<T, E, F, Fut>(mut operation: F, retryable: impl Fn(&E) -> bool) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut delays = RETRY_DELAYS.into_iter();
    loop {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(error) => match delays.next() {
                Some(delay) if retryable(&error) => {
                    tracing::debug!("SQLite was busy; trying again in {delay:?}");
                    tokio::time::sleep(delay).await;
                }
                _ => return Err(error),
            },
        }
    }
}

/// Whether SQLite answered "busy" (database is locked), in any of its forms.
pub fn is_busy(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .and_then(|e| e.code())
        .and_then(|code| code.parse::<i32>().ok())
        .is_some_and(|code| code & 0xff == SQLITE_BUSY)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool(path: &std::path::Path) -> SqlitePool {
        let pool = crate::sqlite::SESSIONS_DB.open(path).await.unwrap();
        sqlx::query("CREATE TABLE IF NOT EXISTS t (v INTEGER)")
            .execute(&pool)
            .await
            .unwrap();
        pool
    }

    /// A pool whose lock wait is zero, so contention shows at once.
    async fn impatient(path: &std::path::Path) -> SqlitePool {
        use std::str::FromStr as _;
        let options =
            sqlx::sqlite::SqliteConnectOptions::from_str(&format!("sqlite:{}", path.display()))
                .unwrap()
                .busy_timeout(Duration::ZERO);
        sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_held_lock_is_busy_and_a_write_after_it_is_released_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let holder = pool(&path).await;
        let other = impatient(&path).await;

        let mut held = holder.begin_immediate().await.unwrap();
        sqlx::query("INSERT INTO t VALUES (1)")
            .execute(&mut *held)
            .await
            .unwrap();
        let refused = sqlx::query("INSERT INTO t VALUES (2)")
            .execute(&other)
            .await
            .unwrap_err();
        assert!(is_busy(&refused), "{refused}");

        // The first attempt meets the lock, which is let go right after it:
        // the retry finds it free and the write goes through. Released by
        // the attempt itself, not after a sleep, so a busy machine can't
        // push the release past the retries (it once did on Linux CI).
        let held = tokio::sync::Mutex::new(Some(held));
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let (held, attempts, other) = (&held, &attempts, &other);
        retry_sqlx(move || async move {
            let attempt = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let written = sqlx::query("INSERT INTO t VALUES (3)").execute(other).await;
            if attempt == 0 {
                assert!(
                    written.as_ref().is_err_and(is_busy),
                    "the first attempt meets the lock: {written:?}"
                );
                held.lock().await.take().unwrap().commit().await.unwrap();
            }
            written
        })
        .await
        .unwrap();
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM t")
            .fetch_one(&holder)
            .await
            .unwrap();
        assert_eq!(n, 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn only_busy_is_retried_and_only_twice() {
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let failed: anyhow::Result<()> = retry_on_busy(|| async {
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(anyhow::anyhow!("not a database error"))
        })
        .await;
        assert!(failed.is_err());
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);

        // A lock held throughout: three attempts, then the busy error.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let holder = pool(&path).await;
        let other = impatient(&path).await;
        let _held = holder.begin_immediate().await.unwrap();
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let started = std::time::Instant::now();
        let failed = retry_on_busy(|| async {
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            sqlx::query("INSERT INTO t VALUES (1)")
                .execute(&other)
                .await
                .map_err(anyhow::Error::from)
        })
        .await
        .unwrap_err();
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert!(started.elapsed() >= Duration::from_millis(300));
        assert!(is_busy(failed.downcast_ref::<sqlx::Error>().unwrap()));
    }
}
