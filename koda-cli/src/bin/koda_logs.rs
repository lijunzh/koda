//! `koda-logs`: read Koda's log database, as Codex's `logs_client` reads
//! its own. Prints the newest matching lines oldest first, then, with
//! `--follow`, new ones as they're stored.
//!
//! ```text
//! koda-logs --session 3f2a… --level warn
//! koda-logs --follow                     # `tail -f` for every session
//! koda-logs --search "model call" --since 2h
//! ```

use clap::{Parser, ValueEnum};
use koda_state::log_db::{LogQuery, LogStore};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Parser)]
#[command(name = "koda-logs", version)]
#[command(about = "Read Koda's log database, with filters, and follow it")]
struct Args {
    /// The log database to read. Defaults to `<config>/db/logs_1.db`.
    #[arg(long)]
    db: Option<PathBuf>,

    /// Lines at this level and above.
    #[arg(long, value_enum, ignore_case = true)]
    level: Option<Level>,

    /// Lines from the last `30m`, `2h` or `3d`, or since a Unix time in
    /// seconds.
    #[arg(long, value_name = "DURATION|UNIX")]
    since: Option<String>,

    /// Lines whose target starts with this, such as `koda_core::providers`.
    /// Repeat for several.
    #[arg(long)]
    target: Vec<String>,

    /// Lines from this session (its full id). Repeat for several.
    #[arg(long)]
    session: Vec<String>,

    /// With `--session`, also lines logged outside any session (startup,
    /// the TUI). Alone, only those.
    #[arg(long)]
    sessionless: bool,

    /// Lines containing this text.
    #[arg(long)]
    search: Option<String>,

    /// How many of the newest matching lines to print first.
    #[arg(long, default_value_t = 200)]
    backfill: usize,

    /// Keep printing new lines as they're stored, until interrupted.
    #[arg(long, short)]
    follow: bool,

    /// How often `--follow` looks for new lines, in milliseconds.
    #[arg(long, default_value_t = 500)]
    poll_ms: u64,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    /// The levels at and above this one.
    fn and_above(self) -> Vec<String> {
        let all = ["ERROR", "WARN", "INFO", "DEBUG", "TRACE"];
        let keep = match self {
            Level::Error => 1,
            Level::Warn => 2,
            Level::Info => 3,
            Level::Debug => 4,
            Level::Trace => 5,
        };
        all[..keep].iter().map(|l| l.to_string()).collect()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let path = match &args.db {
        Some(path) => path.clone(),
        None => koda_config::config_dir()?.join("db").join("logs_1.db"),
    };
    if !path.exists() {
        anyhow::bail!("no log database at {}: has Koda run here?", path.display());
    }
    let store = LogStore::open(&path).await?;
    let mut query = LogQuery {
        levels: args.level.map(Level::and_above).unwrap_or_default(),
        from_ts: args.since.as_deref().map(since).transpose()?,
        targets: args.target.clone(),
        session_ids: args.session.clone(),
        sessionless: args.sessionless,
        search: args.search.clone(),
        ..LogQuery::default()
    };

    let mut last = 0;
    if args.backfill > 0 {
        let mut rows = store
            .query(&LogQuery {
                limit: Some(args.backfill),
                newest_first: true,
                ..query.clone()
            })
            .await?;
        rows.reverse();
        for row in &rows {
            println!("{}", row.render());
        }
        last = rows.last().map_or(0, |row| row.id);
    }
    if !args.follow {
        return Ok(());
    }
    if last == 0 {
        // Nothing printed: follow from the newest line, not the oldest.
        last = store
            .query(&LogQuery {
                limit: Some(1),
                newest_first: true,
                ..query.clone()
            })
            .await?
            .first()
            .map_or(0, |row| row.id);
    }
    loop {
        query.after_id = Some(last);
        for row in store.query(&query).await? {
            println!("{}", row.render());
            last = row.id;
        }
        tokio::time::sleep(Duration::from_millis(args.poll_ms)).await;
    }
}

/// `--since`: `30m`, `2h`, `3d` or `45s` ago, or a Unix time in seconds.
fn since(value: &str) -> anyhow::Result<i64> {
    if let Ok(unix) = value.parse::<i64>() {
        return Ok(unix);
    }
    let (number, unit) = value.split_at(value.len().saturating_sub(1));
    let number: i64 = number.parse().map_err(|_| {
        anyhow::anyhow!("--since {value}: expected 30m, 2h, 3d, 45s or a Unix time")
    })?;
    let seconds = match unit {
        "s" => number,
        "m" => number * 60,
        "h" => number * 3600,
        "d" => number * 86_400,
        _ => anyhow::bail!("--since {value}: expected 30m, 2h, 3d, 45s or a Unix time"),
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    Ok(now - seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_include_those_above() {
        assert_eq!(Level::Warn.and_above(), ["ERROR", "WARN"]);
        assert_eq!(Level::Trace.and_above().len(), 5);
    }

    #[test]
    fn since_takes_durations_and_unix_times() {
        assert_eq!(since("1700000000").unwrap(), 1_700_000_000);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let two_hours = since("2h").unwrap();
        assert!((now - 7200 - two_hours).abs() <= 1);
        assert!(since("2w").is_err());
        assert!(since("soon").is_err());
    }
}
