//! Koda's diagnostic logs in a SQLite database of their own, as Codex keeps
//! its logs in `logs_2.sqlite`.
//!
//! A `tracing` layer ([`LogDbLayer`]) queues every event and a background
//! task stores them in batches ([`LogStore`]). Each line carries the session
//! it was logged in, read from the `session_id` field of the spans around
//! it, so one session's lines can be read alone, as Codex reads a thread's
//! for `/feedback`. Lines outside any session belong to the process.
//!
//! Limits, Codex's: 1,000 lines and 10 MiB per session (and per process for
//! lines outside sessions), 10 days, 64 MiB in all. A complete record of a
//! long run is the opt-in log file (`log_dir` in `config.toml`), as Codex's
//! is.
//!
//! `koda-logs` reads it.

mod layer;
mod store;

pub use layer::{LogDbLayer, default_filter};
pub use store::{BUDGET_BYTES, LogStore, PARTITION_BYTES, PARTITION_ROWS, RETENTION_SECS};

use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// The log database this process writes, once [`install`] has run.
static INSTALLED: OnceLock<(LogDbLayer, Arc<LogStore>)> = OnceLock::new();

/// Start storing this process's log in `store`, and keep the handle so the
/// rest of the process can flush and read it: the panic hook, and the host
/// for `feedback/save`. Returns the layer to add to the subscriber. Needs a
/// Tokio runtime. A second call starts another writer but keeps the first
/// handle.
pub fn install(store: Arc<LogStore>) -> LogDbLayer {
    store.start_maintenance();
    let layer = LogDbLayer::start(store.clone());
    let _ = INSTALLED.set((layer.clone(), store));
    layer
}

/// The installed writer and store, if [`install`] has run.
pub fn installed() -> Option<(&'static LogDbLayer, &'static Arc<LogStore>)> {
    INSTALLED.get().map(|(layer, store)| (layer, store))
}

/// Store what's queued, as before reading the log back.
pub async fn flush() {
    if let Some((layer, _)) = installed() {
        layer.flush().await;
    }
}

/// [`flush`] for code that can't `.await`, waiting at most `limit`.
pub fn flush_blocking(limit: Duration) {
    if let Some((layer, _)) = installed() {
        layer.flush_blocking(limit);
    }
}

/// The lines of `session_ids`, with this process's lines outside any
/// session (startup, the TUI, a panic), in the order they were stored: what
/// a report about those sessions needs, as Codex's feedback logs take a
/// thread's lines and its process's threadless ones. Empty without an
/// installed log database.
pub async fn session_lines(session_ids: &[String]) -> anyhow::Result<Vec<LogRow>> {
    let Some((_, store)) = installed() else {
        return Ok(Vec::new());
    };
    flush().await;
    let mut rows = Vec::new();
    if !session_ids.is_empty() {
        rows = store
            .query(&LogQuery {
                session_ids: session_ids.to_vec(),
                ..LogQuery::default()
            })
            .await?;
    }
    rows.extend(
        store
            .query(&LogQuery {
                sessionless: true,
                process_uuid: Some(process_uuid().to_string()),
                ..LogQuery::default()
            })
            .await?,
    );
    rows.sort_by_key(|row| row.id);
    Ok(rows)
}

/// One line to store.
#[derive(Debug, Clone, PartialEq)]
pub struct LogEntry {
    /// Seconds since the Unix epoch.
    pub ts: i64,
    /// Nanoseconds past `ts`.
    pub ts_nanos: i64,
    /// `TRACE` to `ERROR`.
    pub level: String,
    /// The `tracing` target, such as `koda_core::inference`.
    pub target: String,
    /// The spans around it with their fields, then its message and
    /// fields: `turn{turn_id=t1}: model call purpose=turn`.
    pub body: Option<String>,
    /// The module it was logged from.
    pub module_path: Option<String>,
    /// The source file it was logged from.
    pub file: Option<String>,
    /// The source line it was logged from.
    pub line: Option<i64>,
    /// The session it was logged in, if any.
    pub session_id: Option<String>,
    /// The process that logged it ([`process_uuid`]).
    pub process_uuid: Option<String>,
}

impl LogEntry {
    /// The bytes it counts against its session's limit, as Codex counts.
    pub(crate) fn estimated_bytes(&self) -> i64 {
        [
            self.body.as_deref(),
            Some(self.level.as_str()),
            Some(self.target.as_str()),
            self.module_path.as_deref(),
            self.file.as_deref(),
        ]
        .into_iter()
        .flatten()
        .map(|s| s.len() as i64)
        .sum()
    }
}

/// One stored line.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct LogRow {
    /// Its row id, increasing in the order lines were stored.
    pub id: i64,
    /// Seconds since the Unix epoch.
    pub ts: i64,
    /// Nanoseconds past `ts`.
    pub ts_nanos: i64,
    /// `TRACE` to `ERROR`.
    pub level: String,
    /// The `tracing` target.
    pub target: String,
    /// Spans, message and fields.
    pub body: Option<String>,
    /// The session it was logged in, if any.
    pub session_id: Option<String>,
    /// The process that logged it.
    pub process_uuid: Option<String>,
    /// The source file it was logged from.
    pub file: Option<String>,
    /// The source line it was logged from.
    pub line: Option<i64>,
}

impl LogRow {
    /// The line as text, as the `tracing` formatter writes one:
    /// `2026-11-15T12:00:00.123456Z  INFO koda_core::inference: …`.
    pub fn render(&self) -> String {
        let time = match time::OffsetDateTime::from_unix_timestamp(self.ts) {
            Ok(t) => format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
                t.year(),
                u8::from(t.month()),
                t.day(),
                t.hour(),
                t.minute(),
                t.second(),
                self.ts_nanos / 1_000
            ),
            Err(_) => format!("{}.{:09}", self.ts, self.ts_nanos),
        };
        format!(
            "{time} {:>5} {}: {}",
            self.level,
            self.target,
            self.body.as_deref().unwrap_or("")
        )
    }
}

/// Which lines to read. Empty fields don't filter.
#[derive(Debug, Clone, Default)]
pub struct LogQuery {
    /// Levels to keep, any case.
    pub levels: Vec<String>,
    /// Lines at or after this time, in Unix seconds.
    pub from_ts: Option<i64>,
    /// Lines at or before this time, in Unix seconds.
    pub to_ts: Option<i64>,
    /// Targets to keep, each a prefix, such as `koda_providers`.
    pub targets: Vec<String>,
    /// Sessions to keep.
    pub session_ids: Vec<String>,
    /// Keep lines outside any session too.
    pub sessionless: bool,
    /// Lines from this process only.
    pub process_uuid: Option<String>,
    /// Text the line must contain.
    pub search: Option<String>,
    /// Lines stored after this row id, for following.
    pub after_id: Option<i64>,
    /// At most this many.
    pub limit: Option<usize>,
    /// Newest first.
    pub newest_first: bool,
}

/// This process's id in the log, `pid:<pid>:<uuid>`, as Codex's: a pid
/// alone repeats across runs.
pub fn process_uuid() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| format!("pid:{}:{}", std::process::id(), uuid::Uuid::new_v4()))
}

/// Now, as Unix seconds and nanoseconds.
pub(crate) fn now() -> (i64, i64) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (now.as_secs() as i64, now.subsec_nanos() as i64)
}
