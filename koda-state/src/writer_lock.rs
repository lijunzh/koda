//! One writer per session across processes, as Codex keeps one writer per
//! thread.
//!
//! Every process that writes a session (the shared server, `koda exec`, a
//! TUI in its own process) takes the session's lock file before writing it
//! and holds it while the session is open. A second process that tries to
//! open the same session is refused with "session {id} already has an
//! active writer" rather than appending turns the first never sees.
//! Reading a session takes no lock.
//!
//! Within one process, opening a session this process is still closing
//! waits for it to close (up to 10 s) rather than being refused: Koda's
//! `session/unsubscribe` answers before the session's task has ended, and
//! a client may open the session again at once. Codex answers that case
//! with an error its client retries on; waiting keeps the retry out of
//! every client.
//!
//! The locks are OS advisory locks on files in `session-writer-locks/`
//! beside the session database. A shared coordination lock in the same
//! folder lets one process sweep the files dead writers left while no other
//! process is opening one.
//!
//! ## Provenance
//!
//! Ported from `codex-rs/rollout/src/writer_lock.rs` at upstream commit
//! `06f334aa4b` (codex `main` as of 2026-11-25). Changed: session ids are
//! Koda's (UUIDs as strings), the folder is named for sessions, the error
//! names a session, and publication locking (Codex's
//! `try_acquire_for_publication`) is left out, since Koda has no rollout
//! files to publish; the wait for a session this process is closing is
//! Koda's (above).
//!
//! Original work: Copyright (c) OpenAI / codex contributors,
//! licensed under the Apache License, Version 2.0.
//! See `LICENSES/codex-APACHE-2.0` for the full license text.

use std::collections::HashSet;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use tracing::warn;

/// The lock folder's name, beside the session database.
pub const WRITER_LOCK_DIR: &str = "session-writer-locks";
const COORDINATION_LOCK_FILE: &str = ".coordination.lock";
/// How long a process waits for another that is opening or sweeping locks.
const COORDINATION_WAIT: Duration = Duration::from_secs(30);
/// How long opening a session waits for this process to finish closing it.
const CLOSING_WAIT: Duration = Duration::from_secs(10);

/// Writer ownership for the sessions of one database.
#[derive(Debug)]
pub struct WriterLocks {
    directory: PathBuf,
    cleanup_attempted: AtomicBool,
    /// The sessions this process holds or is taking.
    held: Mutex<HashSet<String>>,
    /// Signalled when this process lets a session go.
    released: Condvar,
}

/// Keeps a session owned by this process until it is dropped.
#[derive(Debug)]
pub struct WriterLockGuard {
    locks: Arc<WriterLocks>,
    session_id: String,
    path: PathBuf,
    file: Option<File>,
}

impl WriterLocks {
    /// The locks for the database in `db_folder`.
    pub fn new(db_folder: &Path) -> Self {
        Self {
            directory: db_folder.join(WRITER_LOCK_DIR),
            cleanup_attempted: AtomicBool::new(false),
            held: Mutex::default(),
            released: Condvar::new(),
        }
    }

    /// Reserve `session_id` in this process, waiting up to [`CLOSING_WAIT`]
    /// for a guard this process is dropping. `false` when it is still held.
    fn reserve(&self, session_id: &str) -> bool {
        let held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        let (mut held, _) = self
            .released
            .wait_timeout_while(held, CLOSING_WAIT, |held| held.contains(session_id))
            .unwrap_or_else(|e| e.into_inner());
        held.insert(session_id.to_string())
    }

    /// This process no longer holds `session_id`.
    fn forget(&self, session_id: &str) {
        self.held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
        self.released.notify_all();
    }

    /// Take session `session_id` for this process. Another writer holding it
    /// gives an error of kind [`io::ErrorKind::WouldBlock`]; see
    /// [`is_active_writer`].
    ///
    /// Blocks on file locks, for up to 30 s while another process is
    /// opening or sweeping: call it off the async runtime's threads.
    pub fn acquire(self: &Arc<Self>, session_id: &str) -> io::Result<WriterLockGuard> {
        if !is_session_id(session_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{session_id:?} isn't a session id"),
            ));
        }
        if !self.reserve(session_id) {
            return Err(active_writer(session_id));
        }
        let taken = self.take(session_id);
        if taken.is_err() {
            self.forget(session_id);
        }
        taken
    }

    /// [`Self::acquire`] once this process has reserved `session_id`.
    fn take(self: &Arc<Self>, session_id: &str) -> io::Result<WriterLockGuard> {
        let file = self.open_coordination()?;
        // Never queue a sweep behind another writer. Removal stays exclusive.
        let have_exclusive = !self.cleanup_attempted.swap(true, Ordering::Relaxed)
            && match file.try_lock() {
                Ok(()) => true,
                Err(fs::TryLockError::WouldBlock) => false,
                Err(fs::TryLockError::Error(err)) => return Err(err),
            };
        if have_exclusive {
            if let Err(err) = self.remove_stale_locks() {
                warn!("couldn't clean up stale session writer locks: {err}");
            }
        } else {
            Self::wait_for_coordination(|| file.try_lock_shared())?;
        }
        let _coordination_lock = file;

        let path = self.directory.join(format!("{session_id}.lock"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|err| {
                io::Error::other(format!(
                    "couldn't open session writer lock {}: {err}",
                    path.display()
                ))
            })?;

        match file.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => return Err(active_writer(session_id)),
            Err(fs::TryLockError::Error(err)) => {
                return Err(io::Error::other(format!(
                    "couldn't take session writer lock {}: {err}",
                    path.display()
                )));
            }
        }

        Ok(WriterLockGuard {
            locks: Arc::clone(self),
            session_id: session_id.to_string(),
            path,
            file: Some(file),
        })
    }

    fn open_coordination(&self) -> io::Result<File> {
        fs::create_dir_all(&self.directory)?;
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.directory.join(COORDINATION_LOCK_FILE))
    }

    fn wait_for_coordination(
        try_lock: impl Fn() -> Result<(), fs::TryLockError>,
    ) -> io::Result<()> {
        let deadline = Instant::now() + COORDINATION_WAIT;
        loop {
            match try_lock() {
                Ok(()) => return Ok(()),
                Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(fs::TryLockError::WouldBlock) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "timed out waiting for the session writer coordination lock; \
                         another Koda process may be suspended",
                    ));
                }
                Err(fs::TryLockError::Error(err)) => return Err(err),
            }
        }
    }

    /// Remove the lock files no process holds: their writers have ended.
    fn remove_stale_locks(&self) -> io::Result<()> {
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(session_id) = file_name.strip_suffix(".lock") else {
                continue;
            };
            if !is_session_id(session_id) {
                continue;
            }

            let path = entry.path();
            let file = match OpenOptions::new().read(true).write(true).open(&path) {
                Ok(file) => file,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => {
                    warn!(
                        "couldn't inspect session writer lock {}: {err}",
                        path.display()
                    );
                    continue;
                }
            };
            match file.try_lock() {
                Ok(()) => {
                    drop(file);
                    if let Err(err) = fs::remove_file(&path)
                        && err.kind() != io::ErrorKind::NotFound
                    {
                        warn!(
                            "couldn't remove stale session writer lock {}: {err}",
                            path.display()
                        );
                    }
                }
                Err(fs::TryLockError::WouldBlock) => {}
                Err(fs::TryLockError::Error(err)) => {
                    warn!(
                        "couldn't inspect session writer lock {}: {err}",
                        path.display()
                    );
                }
            }
        }
        Ok(())
    }
}

impl Drop for WriterLockGuard {
    fn drop(&mut self) {
        self.remove_file();
        // Closing the file releases the lock; only then may this process
        // take the session again.
        drop(self.file.take());
        self.locks.forget(&self.session_id);
    }
}

impl WriterLockGuard {
    /// Close and remove the lock file, unless another process is opening a
    /// writer, as Codex's guard does.
    fn remove_file(&mut self) {
        let coordination_lock = match self.locks.open_coordination() {
            Ok(lock) => lock,
            Err(err) => {
                warn!("couldn't coordinate session writer lock cleanup: {err}");
                return;
            }
        };

        match coordination_lock.try_lock() {
            Ok(()) => {}
            // Leave the file for the next writer. Dropping the guard still
            // closes it, which releases the lock.
            Err(fs::TryLockError::WouldBlock) => return,
            Err(fs::TryLockError::Error(err)) => {
                warn!("couldn't coordinate session writer lock cleanup: {err}");
                return;
            }
        }

        // Close the lock before deleting it, so cleanup works on Windows too.
        drop(self.file.take());
        if let Err(err) = fs::remove_file(&self.path)
            && err.kind() != io::ErrorKind::NotFound
        {
            warn!(
                "couldn't remove session writer lock {}: {err}",
                self.path.display()
            );
        }
        drop(coordination_lock);
    }
}

/// The end of the refusal's message, after "session {id}", as Codex words
/// its "thread {id} already has an active writer".
pub const ACTIVE_WRITER: &str = "already has an active writer";

fn active_writer(session_id: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        format!("session {session_id} {ACTIVE_WRITER}"),
    )
}

/// Whether `message` is the refusal [`WriterLocks::acquire`] gives when
/// another process holds the session, as a client matches it in an error
/// (Codex's TUI matches its own the same way).
pub fn is_active_writer(message: &str) -> bool {
    message.contains(ACTIVE_WRITER)
}

/// A session id names a lock file: a UUID, as Koda makes them. Anything
/// else could name a path outside the folder.
fn is_session_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn new_id() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    #[test]
    fn a_second_writer_is_refused_until_the_first_lets_go() {
        let dir = tempfile::tempdir().unwrap();
        let first = Arc::new(WriterLocks::new(dir.path()));
        // Another process's view of the same folder.
        let second = Arc::new(WriterLocks::new(dir.path()));
        let id = new_id();

        let owner = first.acquire(&id).unwrap();
        let path = dir.path().join(WRITER_LOCK_DIR).join(format!("{id}.lock"));
        assert!(path.exists());

        let err = second.acquire(&id).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(
            err.to_string(),
            format!("session {id} already has an active writer")
        );
        assert!(is_active_writer(&err.to_string()));
        // Another session is free.
        let other = second.acquire(&new_id()).unwrap();

        drop(owner);
        assert!(!path.exists(), "a released lock's file is removed");
        let next = second.acquire(&id).unwrap();
        drop((other, next));
    }

    #[test]
    fn a_writer_closing_while_another_opens_leaves_its_file_and_frees_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let locks = Arc::new(WriterLocks::new(dir.path()));
        let id = new_id();
        let owner = locks.acquire(&id).unwrap();

        // Another process is opening a writer: the closing one can't take
        // the coordination lock, so it leaves its file, but closing it still
        // releases the session.
        let opening = locks.open_coordination().unwrap();
        opening.lock_shared().unwrap();
        let (closed, wait) = mpsc::channel();
        std::thread::spawn(move || {
            drop(owner);
            let _ = closed.send(());
        });
        wait.recv_timeout(Duration::from_secs(10)).unwrap();
        let path = dir.path().join(WRITER_LOCK_DIR).join(format!("{id}.lock"));
        assert!(path.exists());
        let again = Arc::new(WriterLocks::new(dir.path())).acquire(&id).unwrap();
        drop(opening);
        drop(again);
    }

    #[test]
    fn a_dead_writers_file_is_swept_by_the_next_process() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join(WRITER_LOCK_DIR);
        fs::create_dir_all(&folder).unwrap();
        let stale = folder.join(format!("{}.lock", new_id()));
        fs::write(&stale, b"").unwrap();
        // Not a session's lock: left alone.
        let other = folder.join("notes.lock");
        fs::write(&other, b"").unwrap();

        let locks = Arc::new(WriterLocks::new(dir.path()));
        let _owner = locks.acquire(&new_id()).unwrap();
        assert!(!stale.exists());
        assert!(other.exists());
    }

    #[test]
    fn this_process_waits_for_a_session_it_is_closing() {
        let dir = tempfile::tempdir().unwrap();
        let locks = Arc::new(WriterLocks::new(dir.path()));
        let id = new_id();
        let owner = locks.acquire(&id).unwrap();
        let closing = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(owner);
        });
        // Taken once the first guard has gone, not refused.
        let again = locks.acquire(&id).unwrap();
        closing.join().unwrap();
        drop(again);
    }

    #[test]
    fn only_a_session_id_names_a_lock() {
        let dir = tempfile::tempdir().unwrap();
        let locks = Arc::new(WriterLocks::new(dir.path()));
        let err = locks.acquire("../escape").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
