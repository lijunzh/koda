//! A background command's terminal: what `WriteStdin` types into and reads
//! back, as Codex's `write_stdin` does with an `exec_command` session.
//!
//! A thread drains the terminal for as long as the command runs. Everything
//! goes to the command's log, as a background command's output always has,
//! and to a buffer that `WriteStdin` reads from. Draining also matters on its
//! own: a command can't finish exiting while its terminal holds output that
//! nobody read.
//!
//! The buffer keeps the last
//! [`MAX_UNREAD_BYTES`](crate::tools::terminal::MAX_UNREAD_BYTES) not yet
//! read, as Codex keeps 1 MiB, and counts what it drops.

use std::io::{Read as _, Write as _};
use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

/// How much unread output is kept: Codex's `UNIFIED_EXEC_OUTPUT_MAX_BYTES`.
pub const MAX_UNREAD_BYTES: usize = 1024 * 1024;

/// Output not yet handed to the model.
#[derive(Default)]
struct Unread {
    bytes: Vec<u8>,
    /// Bytes dropped from the front since the last read, to stay in the cap.
    dropped: usize,
    /// The command closed the terminal: it, and all it started, exited.
    closed: bool,
}

/// What a read returned.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Output {
    /// The output since the last read, as the terminal sent it.
    pub bytes: Vec<u8>,
    /// Bytes that came before `bytes` but were dropped to stay in the cap.
    pub dropped: usize,
    /// The command closed the terminal: it, and all it started, exited.
    pub closed: bool,
}

/// One background command's terminal.
pub struct Terminal {
    input: Mutex<std::fs::File>,
    unread: Arc<Mutex<Unread>>,
    changed: Arc<Notify>,
}

impl Terminal {
    /// Start draining `controller`, Koda's end of the command's terminal,
    /// into `log` and the unread buffer.
    pub fn start(controller: OwnedFd, mut log: std::fs::File) -> std::io::Result<Self> {
        let input = std::fs::File::from(controller.try_clone()?);
        let mut output = std::fs::File::from(controller);
        let unread = Arc::new(Mutex::new(Unread::default()));
        let changed = Arc::new(Notify::new());
        let (sink, notify) = (Arc::clone(&unread), Arc::clone(&changed));
        std::thread::Builder::new()
            .name("koda-terminal".into())
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match output.read(&mut buf) {
                        // EIO (Linux) or EOF: the command's end is closed.
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let _ = log.write_all(&buf[..n]);
                            let mut unread = sink.lock().unwrap();
                            unread.bytes.extend_from_slice(&buf[..n]);
                            let over = unread.bytes.len().saturating_sub(MAX_UNREAD_BYTES);
                            if over > 0 {
                                unread.bytes.drain(..over);
                                unread.dropped += over;
                            }
                        }
                    }
                    notify.notify_waiters();
                }
                sink.lock().unwrap().closed = true;
                notify.notify_waiters();
            })?;
        Ok(Self {
            input: Mutex::new(input),
            unread,
            changed,
        })
    }

    /// Type `text` into the terminal.
    pub fn write(&self, text: &[u8]) -> std::io::Result<()> {
        let mut input = self.input.lock().unwrap();
        input.write_all(text)?;
        input.flush()
    }

    /// Whether the command has closed the terminal.
    pub fn closed(&self) -> bool {
        self.unread.lock().unwrap().closed
    }

    /// Wait `wait`, or until the command closes the terminal, then take the
    /// output since the last read.
    pub async fn read(&self, wait: Duration) -> Output {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let changed = self.changed.notified();
            if self.closed() {
                break;
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                break;
            }
        }
        let mut unread = self.unread.lock().unwrap();
        Output {
            bytes: std::mem::take(&mut unread.bytes),
            dropped: std::mem::take(&mut unread.dropped),
            closed: unread.closed,
        }
    }
}

/// `bytes` as text for the model: terminal escape sequences (colours, cursor
/// moves, titles) removed, and line ends as `\n`. A `\r` that rewrites a line
/// (a progress bar) keeps only what was written last on it.
pub fn plain_text(bytes: &[u8]) -> String {
    static ESCAPES: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        // CSI (`ESC [ … final`), OSC (`ESC ] … BEL` or `ESC ] … ESC \`), and
        // the two-byte ones (`ESC =`, `ESC (B`).
        regex::Regex::new(r"\x1b\[[0-9;?<=>!]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(\x07|\x1b\\)|\x1b[()][0-9A-Za-z]|\x1b[=>78DEHMc]")
            .unwrap()
    });
    let text = String::from_utf8_lossy(bytes);
    let text = ESCAPES.replace_all(&text, "");
    text.replace("\r\n", "\n")
        .split('\n')
        .map(|line| {
            line.rsplit('\r')
                .find(|part| !part.is_empty())
                .unwrap_or("")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn start(script: &str) -> (Terminal, tokio::process::Child, tempfile::NamedTempFile) {
        let pty = koda_sandbox::pty::open().unwrap();
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c").arg(script);
        koda_sandbox::pty::attach(&mut cmd, &pty.terminal).unwrap();
        let child = cmd.spawn().unwrap();
        drop(cmd);
        drop(pty.terminal);
        let log = tempfile::NamedTempFile::new().unwrap();
        let terminal = Terminal::start(pty.controller, log.reopen().unwrap()).unwrap();
        (terminal, child, log)
    }

    #[tokio::test]
    async fn typing_is_answered_and_the_log_has_it_all() {
        let (terminal, mut child, log) = start("printf 'Name? '; read name; echo \"Hello, $name\"");
        let first = terminal.read(Duration::from_secs(5)).await;
        // Read waits the whole time unless the command closes the terminal.
        assert!(plain_text(&first.bytes).contains("Name? "));
        assert!(!first.closed);
        terminal.write(b"Ada\n").unwrap();
        let reply = terminal.read(Duration::from_secs(10)).await;
        assert!(plain_text(&reply.bytes).contains("Hello, Ada"));
        assert!(reply.closed, "the command exited, so the read ended early");
        assert!(child.wait().await.unwrap().success());
        let logged = std::fs::read_to_string(log.path()).unwrap();
        assert!(logged.contains("Name? ") && logged.contains("Hello, Ada"));
    }

    #[tokio::test]
    async fn a_read_returns_only_what_is_new() {
        let (terminal, mut child, _log) = start("echo one; read x; echo two");
        assert!(plain_text(&terminal.read(Duration::from_millis(500)).await.bytes).contains("one"));
        terminal.write(b"\n").unwrap();
        let second = plain_text(&terminal.read(Duration::from_secs(10)).await.bytes);
        assert!(
            second.contains("two") && !second.contains("one"),
            "{second:?}"
        );
        child.wait().await.unwrap();
    }

    #[test]
    fn plain_text_drops_escapes_and_rewritten_lines() {
        let raw = b"\x1b[1;32mok\x1b[0m\r\n\x1b]0;title\x07>>> \r\n10%\r50%\r100%\r\ndone";
        assert_eq!(plain_text(raw), "ok\n>>> \n100%\ndone");
    }
}
