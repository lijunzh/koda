//! What you typed, for Up and Down in the input box: a file the TUI owns.
//!
//! Codex's TUI keeps its input history in `~/.codex/history.jsonl`
//! (`codex-message-history`), not in the app-server's thread store; Koda's
//! TUI does the same, in `~/.config/koda/history.jsonl`, so it doesn't write
//! the host's database. The record is Codex's, one JSON object per line:
//!
//! ```text
//! {"session_id":"<id>","ts":<unix seconds>,"text":"<what was typed>"}
//! ```
//!
//! As in Codex, each line is written whole under an exclusive advisory lock,
//! so two Koda processes appending at once don't interleave, and the file is
//! readable by its owner only. Koda keeps the newest [`MAX_ENTRIES`], where
//! Codex caps the file by bytes, since the TUI reads all of it at startup.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The file's name in Koda's config directory.
const FILE_NAME: &str = "history.jsonl";

/// Entries kept: the newest this many, as the database table kept.
pub(crate) const MAX_ENTRIES: usize = 500;

/// Past this many entries the file is cut back to [`MAX_ENTRIES`], so it
/// isn't rewritten on every append, as Codex's soft cap does.
const TRIM_AT: usize = MAX_ENTRIES + MAX_ENTRIES / 5;

/// Codex's retries for the lock: ten, 100 ms apart.
const LOCK_RETRIES: usize = 10;
const LOCK_WAIT: Duration = Duration::from_millis(100);

/// One line of the file, in Codex's shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Entry {
    session_id: String,
    ts: u64,
    text: String,
}

/// The input history file.
#[derive(Debug, Clone)]
pub(crate) struct InputHistory {
    path: PathBuf,
}

impl InputHistory {
    /// The history in Koda's config directory `dir`.
    pub(crate) fn in_dir(dir: &Path) -> Self {
        Self {
            path: dir.join(FILE_NAME),
        }
    }

    /// Whether the file exists yet.
    pub(crate) fn exists(&self) -> bool {
        self.path.exists()
    }

    /// Every entry's text, oldest first. A line that doesn't parse is
    /// skipped; a file that can't be read is an empty history.
    pub(crate) fn load(&self) -> Vec<String> {
        let Ok(file) = File::open(&self.path) else {
            return Vec::new();
        };
        BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .filter_map(|line| serde_json::from_str::<Entry>(&line).ok())
            .map(|entry| entry.text)
            .collect()
    }

    /// Append `text`, typed in session `session_id`. The write happens off
    /// the async runtime and isn't waited for, as in Codex; a failure is
    /// logged, since history is a convenience.
    pub(crate) fn push(&self, session_id: &str, text: &str) {
        let history = self.clone();
        let entry = Entry {
            session_id: session_id.to_string(),
            ts: now(),
            text: text.to_string(),
        };
        let write = move || {
            if let Err(e) = history.append(&[entry]) {
                tracing::warn!("couldn't save input history: {e}");
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn_blocking(write);
            }
            Err(_) => write(),
        }
    }

    /// Write `texts`, oldest first, as entries without a session: history
    /// brought over from the database Koda kept it in before.
    pub(crate) fn import(&self, texts: Vec<String>) -> std::io::Result<()> {
        let ts = now();
        let entries: Vec<Entry> = texts
            .into_iter()
            .rev()
            .take(MAX_ENTRIES)
            .rev()
            .map(|text| Entry {
                session_id: String::new(),
                ts,
                text,
            })
            .collect();
        self.append(&entries)
    }

    /// Append `entries` under the lock, then cut the file back if it has
    /// grown past [`TRIM_AT`].
    fn append(&self, entries: &[Entry]) -> std::io::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut lines = String::new();
        for entry in entries {
            lines.push_str(&serde_json::to_string(entry)?);
            lines.push('\n');
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            options.mode(0o600);
            let mut file = options.open(&self.path)?;
            let mode = file.metadata()?.permissions().mode() & 0o777;
            if mode != 0o600 {
                file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
            locked(&mut file, |file| write_and_trim(file, lines.as_bytes()))
        }
        #[cfg(not(unix))]
        {
            let mut file = options.open(&self.path)?;
            locked(&mut file, |file| write_and_trim(file, lines.as_bytes()))
        }
    }
}

/// Run `f` holding an exclusive lock on `file`, waiting for it as Codex does.
fn locked(
    file: &mut File,
    f: impl FnOnce(&mut File) -> std::io::Result<()>,
) -> std::io::Result<()> {
    for _ in 0..LOCK_RETRIES {
        match file.try_lock() {
            Ok(()) => {
                let result = f(file);
                let _ = file.unlock();
                return result;
            }
            Err(std::fs::TryLockError::WouldBlock) => std::thread::sleep(LOCK_WAIT),
            Err(std::fs::TryLockError::Error(e)) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::WouldBlock,
        "another Koda holds the input history file",
    ))
}

/// Write `lines` at the end, then keep only the newest [`MAX_ENTRIES`] if
/// the file has more than [`TRIM_AT`].
fn write_and_trim(file: &mut File, lines: &[u8]) -> std::io::Result<()> {
    file.seek(SeekFrom::End(0))?;
    file.write_all(lines)?;
    file.flush()?;

    file.seek(SeekFrom::Start(0))?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    let count = text.lines().count();
    if count <= TRIM_AT {
        return Ok(());
    }
    let kept: String = text
        .lines()
        .skip(count - MAX_ENTRIES)
        .flat_map(|line| [line, "\n"])
        .collect();
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(kept.as_bytes())?;
    file.flush()
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_come_back_oldest_first_in_codexs_record() {
        let dir = tempfile::tempdir().unwrap();
        let history = InputHistory::in_dir(dir.path());
        assert!(!history.exists());
        assert!(history.load().is_empty());

        history.push("s1", "hello");
        history.push("s1", "/model gpt-4");
        history.push("s2", "line one\nline two");
        assert_eq!(
            history.load(),
            ["hello", "/model gpt-4", "line one\nline two"]
        );

        let text = std::fs::read_to_string(dir.path().join(FILE_NAME)).unwrap();
        let first: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(first["session_id"], "s1");
        assert_eq!(first["text"], "hello");
        assert!(first["ts"].as_u64().unwrap() > 1_700_000_000);
    }

    #[cfg(unix)]
    #[test]
    fn only_its_owner_can_read_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let history = InputHistory::in_dir(dir.path());
        history.push("s", "secret-ish");
        let mode = std::fs::metadata(dir.path().join(FILE_NAME))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn the_newest_entries_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let history = InputHistory::in_dir(dir.path());
        for n in 0..=TRIM_AT {
            history.push("s", &format!("entry {n}"));
        }
        let loaded = history.load();
        assert_eq!(loaded.len(), MAX_ENTRIES);
        assert_eq!(loaded.last().unwrap(), &format!("entry {TRIM_AT}"));
        assert_eq!(
            loaded.first().unwrap(),
            &format!("entry {}", TRIM_AT + 1 - MAX_ENTRIES)
        );
    }

    #[test]
    fn a_line_that_doesnt_parse_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let history = InputHistory::in_dir(dir.path());
        history.push("s", "kept");
        let path = dir.path().join(FILE_NAME);
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("not json\n");
        std::fs::write(&path, text).unwrap();
        history.push("s", "also kept");
        assert_eq!(history.load(), ["kept", "also kept"]);
    }

    #[test]
    fn imported_history_keeps_its_order_and_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let history = InputHistory::in_dir(dir.path());
        let old: Vec<String> = (0..MAX_ENTRIES + 3).map(|n| format!("old {n}")).collect();
        history.import(old).unwrap();
        let loaded = history.load();
        assert_eq!(loaded.len(), MAX_ENTRIES);
        assert_eq!(loaded[0], "old 3");
        assert!(history.exists());
    }
}
