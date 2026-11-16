//! The `tracing` layer that stores events in the log database, Codex's
//! `LogDbLayer`: events go to a bounded queue without waiting, and a
//! background task stores them in batches.

use super::{LogEntry, LogStore, process_uuid};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tracing::field::{Field, Visit};
use tracing::level_filters::LevelFilter;
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::field::RecordFields;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::fmt::FormatFields;
use tracing_subscriber::fmt::FormattedFields;
use tracing_subscriber::fmt::format::DefaultFields;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

/// Lines waiting to be stored before new ones are dropped, as Codex's.
const QUEUE_CAPACITY: usize = 2048;
/// Lines stored in one transaction.
const BATCH_SIZE: usize = 512;
/// The longest a line waits to be stored.
const FLUSH_INTERVAL: Duration = Duration::from_secs(10);
/// Targets whose events are never stored: storing them would log more.
const SQLX_TARGETS: [&str; 3] = ["sqlx", "sqlx_core", "sqlx_sqlite"];

/// What the log database records, as Codex's `log_db::default_filter`:
/// everything Koda logs, down to `TRACE`, except what would flood it.
pub fn default_filter() -> Targets {
    Targets::new()
        .with_default(LevelFilter::TRACE)
        .with_targets(SQLX_TARGETS.map(|t| (t, LevelFilter::OFF)))
        .with_target("log", LevelFilter::OFF)
        .with_target("h2", LevelFilter::WARN)
        .with_target("hyper", LevelFilter::WARN)
        .with_target("hyper_util", LevelFilter::WARN)
        .with_target("reqwest", LevelFilter::WARN)
        .with_target("rustls", LevelFilter::WARN)
        .with_target("tower", LevelFilter::WARN)
        .with_target("mio", LevelFilter::WARN)
        .with_target("tokio", LevelFilter::INFO)
        .with_target("runtime", LevelFilter::INFO)
        .with_target("rmcp", LevelFilter::INFO)
        // Whole request bodies are logged at TRACE; they'd crowd out
        // everything else a session keeps. A file at RUST_LOG=trace still
        // has them, as Codex keeps its request bodies out of the database.
        .with_target("koda_providers", LevelFilter::DEBUG)
        .with_target("koda_sandbox::bwrap_proxy", LevelFilter::DEBUG)
}

enum Command {
    Line(Box<LogEntry>),
    Flush(oneshot::Sender<()>),
    /// A flush waited on from outside the runtime, by the panic hook.
    FlushSync(std::sync::mpsc::Sender<()>),
}

/// Stores `tracing` events in a [`LogStore`]. Cloning shares the queue.
#[derive(Clone)]
pub struct LogDbLayer {
    queue: mpsc::Sender<Command>,
    failed: Arc<AtomicBool>,
}

impl LogDbLayer {
    /// Start storing into `store`. Needs a Tokio runtime.
    pub fn start(store: Arc<LogStore>) -> Self {
        let (queue, lines) = mpsc::channel(QUEUE_CAPACITY);
        let failed = Arc::new(AtomicBool::new(false));
        tokio::spawn(store_lines(store, lines, failed.clone()));
        Self { queue, failed }
    }

    /// Whether a batch failed to store, so the database is missing lines.
    pub fn has_write_failure(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    /// Wait until every line queued before this call is stored.
    pub async fn flush(&self) {
        let (done, stored) = oneshot::channel();
        if self.queue.send(Command::Flush(done)).await.is_ok() {
            let _ = stored.await;
        }
    }

    /// [`Self::flush`] for code that can't `.await`, such as a panic hook:
    /// a release build aborts as the hook returns, which would lose a panic
    /// still in the queue. Waits at most `limit`; never panics.
    pub fn flush_blocking(&self, limit: Duration) {
        let deadline = std::time::Instant::now() + limit;
        let (done, stored) = std::sync::mpsc::channel();
        let mut command = Command::FlushSync(done);
        loop {
            match self.queue.try_send(command) {
                Ok(()) => break,
                Err(mpsc::error::TrySendError::Full(back)) => {
                    if std::time::Instant::now() >= deadline {
                        return;
                    }
                    command = back;
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(mpsc::error::TrySendError::Closed(_)) => return,
            }
        }
        let _ = stored.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()));
    }
}

async fn store_lines(
    store: Arc<LogStore>,
    mut commands: mpsc::Receiver<Command>,
    failed: Arc<AtomicBool>,
) {
    let mut batch = Vec::with_capacity(BATCH_SIZE);
    let mut every = tokio::time::interval(FLUSH_INTERVAL);
    every.tick().await;
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(Command::Line(line)) => {
                    batch.push(*line);
                    if batch.len() >= BATCH_SIZE {
                        write(&store, &mut batch, &failed).await;
                    }
                }
                Some(Command::Flush(done)) => {
                    write(&store, &mut batch, &failed).await;
                    let _ = done.send(());
                }
                Some(Command::FlushSync(done)) => {
                    write(&store, &mut batch, &failed).await;
                    let _ = done.send(());
                }
                None => {
                    write(&store, &mut batch, &failed).await;
                    break;
                }
            },
            _ = every.tick() => write(&store, &mut batch, &failed).await,
        }
    }
}

/// Store `batch`. A failure isn't logged, which would queue more lines for
/// the same failing database; it is remembered for the debug report.
async fn write(store: &LogStore, batch: &mut Vec<LogEntry>, failed: &AtomicBool) {
    if batch.is_empty() {
        return;
    }
    let lines = std::mem::take(batch);
    if store.insert(&lines).await.is_err() {
        failed.store(true, Ordering::Relaxed);
    }
}

/// A span's name, fields and session, kept on the span for its events.
struct SpanLog {
    name: &'static str,
    fields: String,
    session_id: Option<String>,
}

impl<S> Layer<S> for LogDbLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut session = SessionField::default();
        attrs.record(&mut session);
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(SpanLog {
                name: span.metadata().name(),
                fields: format_fields(attrs),
                session_id: session.0,
            });
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let mut session = SessionField::default();
        values.record(&mut session);
        let Some(span) = ctx.span(id) else { return };
        let mut extensions = span.extensions_mut();
        match extensions.get_mut::<SpanLog>() {
            Some(log) => {
                if session.0.is_some() {
                    log.session_id = session.0;
                }
                let mut fields =
                    FormattedFields::<DefaultFields>::new(std::mem::take(&mut log.fields));
                let _ = DefaultFields::default().add_fields(&mut fields, values);
                log.fields = fields.fields;
            }
            None => extensions.insert(SpanLog {
                name: span.metadata().name(),
                fields: format_fields(values),
                session_id: session.0,
            }),
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let target = metadata.target();
        let root = target.split("::").next().unwrap_or(target);
        // Storing sqlx's own events would log the store's writes forever;
        // this holds whatever filter the layer is given.
        if SQLX_TARGETS.contains(&root) {
            return;
        }
        // The queue's own channel and timer polls, with Tokio tracing on.
        if *metadata.level() == tracing::Level::TRACE && matches!(root, "tokio" | "runtime") {
            return;
        }

        let mut session = SessionField::default();
        event.record(&mut session);
        let mut body = String::new();
        let mut span_session = None;
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope.from_root() {
                let extensions = span.extensions();
                match extensions.get::<SpanLog>() {
                    Some(log) => {
                        // The innermost span that names a session wins: a
                        // sub-agent's lines are its own session's.
                        if log.session_id.is_some() {
                            span_session.clone_from(&log.session_id);
                        }
                        body.push_str(log.name);
                        if !log.fields.is_empty() {
                            body.push('{');
                            body.push_str(&log.fields);
                            body.push('}');
                        }
                    }
                    None => body.push_str(span.metadata().name()),
                }
                body.push(':');
            }
            if !body.is_empty() {
                body.push(' ');
            }
        }
        body.push_str(&format_fields(event));

        let (ts, ts_nanos) = super::now();
        let line = LogEntry {
            ts,
            ts_nanos,
            level: metadata.level().as_str().to_string(),
            target: target.to_string(),
            body: Some(body),
            module_path: metadata.module_path().map(str::to_string),
            file: metadata.file().map(str::to_string),
            line: metadata.line().map(i64::from),
            session_id: session.0.or(span_session),
            process_uuid: Some(process_uuid().to_string()),
        };
        // Full: the line is dropped rather than slowing the caller, as in
        // Codex.
        let _ = self.queue.try_send(Command::Line(Box::new(line)));
    }
}

/// Reads a `session_id` field.
#[derive(Default)]
struct SessionField(Option<String>);

impl SessionField {
    fn take(&mut self, field: &Field, value: String) {
        if field.name() == "session_id" && self.0.is_none() && !value.is_empty() {
            self.0 = Some(value);
        }
    }
}

impl Visit for SessionField {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.take(field, value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.take(field, format!("{value:?}"));
    }
}

fn format_fields<R: RecordFields>(fields: R) -> String {
    let mut formatted = FormattedFields::<DefaultFields>::new(String::new());
    let _ = DefaultFields::default().format_fields(formatted.as_writer(), fields);
    formatted.fields
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log_db::LogQuery;
    use tracing_subscriber::prelude::*;

    /// Lines carry the session of the innermost span that names one, the
    /// spans around them, and their fields; lines outside a session have
    /// none. sqlx's own events aren't stored.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn events_are_stored_with_their_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(LogStore::open_in(dir.path()).await.unwrap());
        let layer = LogDbLayer::start(store.clone());
        let subscriber = tracing_subscriber::registry().with(layer.clone());
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("starting");
            let session = tracing::info_span!("session", session_id = "s1");
            let _in_session = session.enter();
            let turn = tracing::info_span!("turn", turn_id = "t1");
            let _in_turn = turn.enter();
            tracing::info!(purpose = "turn", "model call");
            let sub = tracing::info_span!("sub_agent", session_id = tracing::field::Empty);
            sub.record("session_id", "s2");
            let _in_sub = sub.enter();
            tracing::warn!("from the sub-agent");
            tracing::debug!(target: "sqlx::query", "not stored");
        });
        layer.flush().await;

        let rows = store.query(&LogQuery::default()).await.unwrap();
        let seen: Vec<_> = rows
            .iter()
            .map(|r| (r.session_id.as_deref(), r.body.as_deref().unwrap()))
            .collect();
        assert_eq!(
            seen,
            [
                (None, "starting"),
                (
                    Some("s1"),
                    "session{session_id=\"s1\"}:turn{turn_id=\"t1\"}: model call purpose=\"turn\""
                ),
                (
                    Some("s2"),
                    "session{session_id=\"s1\"}:turn{turn_id=\"t1\"}:sub_agent{session_id=\"s2\"}: from the sub-agent"
                ),
            ]
        );
        assert_eq!(rows[0].process_uuid.as_deref(), Some(process_uuid()));
        assert!(
            rows[1]
                .render()
                .contains(" INFO koda_state::log_db::layer::tests: session{")
        );
    }

    /// The panic hook's flush stores what's queued without `.await`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_blocking_flush_stores_the_queue() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(LogStore::open_in(dir.path()).await.unwrap());
        let layer = LogDbLayer::start(store.clone());
        let subscriber = tracing_subscriber::registry().with(layer.clone());
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!("panicked at here");
            layer.flush_blocking(Duration::from_secs(5));
        });
        let rows = store.query(&LogQuery::default()).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(!layer.has_write_failure());
    }
}
