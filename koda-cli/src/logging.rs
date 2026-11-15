//! Where Koda's `tracing` output goes, as Codex's TUI sets it up:
//!
//! - **The log database** (`<config>/db/logs_1.db`), always: every line,
//!   filed under the session it was logged in, kept within Codex's limits.
//!   `koda-logs` reads it.
//! - **A log file**, only when `log_dir` is set in `config.toml`, as Codex
//!   writes `codex-tui.log` only then: `<log_dir>/koda.log`, appended to,
//!   owner-only, filtered by `RUST_LOG` (every Koda crate at `info` without
//!   it). It is the complete record of a long run, which the database's
//!   per-session limits aren't.

use koda_state::log_db::{LogStore, default_filter};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::prelude::*;

/// The log file's name in `log_dir`.
pub(crate) const LOG_FILE_NAME: &str = "koda.log";

/// Every Koda crate at `info`: the file's level without `RUST_LOG`. Before
/// the log database, the default named only `koda_core` and `koda_cli`, so
/// the host's, the sandbox's and the config's lines were dropped.
const DEFAULT_FILE_FILTER: &str = "koda_core=info,koda_cli=info,koda_client=info,\
koda_sandbox=info,koda_config=info,koda_protocol=info";

/// What keeps logging running: drop it last. Dropping it stores what's
/// queued for the log database, since the runtime ends without waiting for
/// the writer, then writes out the file's lines.
pub(crate) struct Logging {
    _file: Option<WorkerGuard>,
}

impl Drop for Logging {
    fn drop(&mut self) {
        koda_state::log_db::flush_blocking(Duration::from_secs(2));
    }
}

/// Send `tracing` output to the log database under `config_dir` and, when
/// `log_dir` is set, to `<log_dir>/koda.log`. A database that won't open
/// leaves the file (if any) and is said on stderr: Koda runs without it.
pub(crate) async fn init(config_dir: &Path, log_dir: Option<PathBuf>) -> anyhow::Result<Logging> {
    let db_layer = match LogStore::open_in(config_dir).await {
        Ok(store) => Some(koda_state::log_db::install(Arc::new(store))),
        Err(e) => {
            eprintln!("koda: the log database didn't open, so this run isn't logged there: {e:#}");
            None
        }
    };

    let (file_layer, guard) = match log_dir {
        Some(dir) => {
            let file = open_log_file(&dir)?;
            let (writer, guard) = tracing_appender::non_blocking(file);
            let filter = tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(DEFAULT_FILE_FILTER));
            let layer = tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_ansi(false)
                .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
                .with_filter(filter);
            (Some(layer), Some(guard))
        }
        None => (None, None),
    };

    tracing_subscriber::registry()
        .with(db_layer.map(|layer| layer.with_filter(default_filter())))
        .with(file_layer)
        .init();
    Ok(Logging { _file: guard })
}

/// Open `<dir>/koda.log` for appending, creating `dir`; owner-only on Unix,
/// as Codex opens its log file, since lines can quote prompts and output.
fn open_log_file(dir: &Path) -> anyhow::Result<std::fs::File> {
    std::fs::create_dir_all(dir).map_err(|e| anyhow::anyhow!("log_dir {}: {e}", dir.display()))?;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let path = dir.join(LOG_FILE_NAME);
    options
        .open(&path)
        .map_err(|e| anyhow::anyhow!("log file {}: {e}", path.display()))
}

/// `log_dir` from the config file: `~/` is the home directory, and a
/// relative path is under the Koda config directory.
pub(crate) fn resolve_log_dir(value: &str, config_dir: &Path) -> PathBuf {
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = koda_config::runtime_env::get("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        config_dir.join(path)
    }
}

/// Store what's queued for the log database now, waiting at most two
/// seconds: for the panic hook, since a release build aborts right after.
pub(crate) fn flush_for_panic() {
    koda_state::log_db::flush_blocking(Duration::from_secs(2));
}

/// Store what's queued for the log database, as before `exit`.
pub(crate) async fn flush() {
    koda_state::log_db::flush().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_dir_resolves_home_and_relative_paths() {
        let config = Path::new("/cfg/koda");
        assert_eq!(
            resolve_log_dir("/var/log/koda", config),
            PathBuf::from("/var/log/koda")
        );
        assert_eq!(
            resolve_log_dir("logs", config),
            PathBuf::from("/cfg/koda/logs")
        );
        if let Some(home) = koda_config::runtime_env::get("HOME") {
            assert_eq!(
                resolve_log_dir("~/koda-logs", config),
                PathBuf::from(home).join("koda-logs")
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_log_file_is_owner_only_and_appended_to() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        open_log_file(&logs).unwrap().write_all(b"one\n").unwrap();
        open_log_file(&logs).unwrap().write_all(b"two\n").unwrap();
        let path = logs.join(LOG_FILE_NAME);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\n");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
