//! Making sure Koda's shared server is up, at this client's version: the
//! one start-up rule every client follows, the TUI in its process and any
//! other client through `koda serve --start`, as Codex's clients start its
//! daemon with `codex app-server daemon start` and Hermes's desktop app
//! runs `hermes gateway ensure`.
//!
//! - **Connect, and start `koda serve` when nothing answers.** A server
//!   started here stops once it has had no client and no work for ten
//!   minutes; no client owns it, so closing one stops nothing.
//! - **Only a server of this version is used** ([`crate::version`]). A
//!   newer one means Koda was updated since this client started: it isn't
//!   used or touched. An older one is replaced (`serve/stop` with
//!   `forUpdate`): at once when no turn runs in it, whatever clients are
//!   connected; otherwise only once the caller says what to do with those
//!   turns ([`RunningTurns`](crate::ensure::RunningTurns)), which it asks its user, as Codex's TUI
//!   confirms a restart that "may interrupt active or queued work".

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use koda_protocol::initialize::ClientInfo;
use koda_protocol::methods::ServeStop;
use koda_protocol::serve::ServeStopParams;

use crate::version::{self, ServerVersion};
use crate::{InProcessClient, RequestError};

/// How long a server started here stays up with no client and no work.
pub const IDLE_EXIT_SECS: u64 = 600;
/// How long to wait for a server started here to answer, and for an idle
/// one asked to stop to be gone: Codex's wait for its daemon on macOS and
/// Linux (`START_TIMEOUT`, 30 s), where a cold start from a slow disk, or a
/// debug build on a busy machine, takes more than ten seconds.
const READY_WAIT: Duration = Duration::from_secs(30);
/// How much longer to wait once the server started here has exited. It may
/// have lost the socket to a server another koda started at the same
/// moment, which then answers; otherwise nothing will.
const EXITED_WAIT: Duration = Duration::from_secs(5);
/// How long to wait for a server stopping its running turns to be gone.
const STOP_WAIT: Duration = Duration::from_secs(60);

/// What to do with the turns running in an older server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunningTurns {
    /// Wait for them to finish, then replace it. New turns there are
    /// refused meanwhile, with Codex's draining error.
    Wait,
    /// Stop them now (they end interrupted, stored) and replace it.
    Stop,
}

/// What to make sure of.
pub struct Ensure<'a> {
    /// The server's socket.
    pub socket: &'a Path,
    /// The folder the connection works in, and the started server's.
    pub project_root: &'a Path,
    /// The `koda` binary that starts a server: this client's own version.
    pub koda: PathBuf,
    /// Whether this is Koda's own socket, to be started and replaced here.
    /// A server named elsewhere (`--remote`) only has its version checked.
    pub manage: bool,
    /// What to do with an older server's running turns; `None` changes
    /// nothing while a turn runs there, and says so.
    pub running_turns: Option<RunningTurns>,
    /// Who connects.
    pub client_info: ClientInfo,
}

/// How the server came to be ready, named as Codex's
/// `codex app-server daemon start` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// One of this version was already running.
    AlreadyRunning,
    /// None answered, so one was started.
    Started,
    /// An older one was replaced.
    Restarted,
}

impl Status {
    /// The name `koda serve --start` prints.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AlreadyRunning => "alreadyRunning",
            Self::Started => "started",
            Self::Restarted => "restarted",
        }
    }
}

/// Why the server isn't ready.
#[derive(Debug)]
pub enum NotReady {
    /// An older server has turns running in `working` sessions, and nothing
    /// was said about them; `draining`, it already waits for them (another
    /// client asked), and only waiting is left.
    Busy {
        /// Its version.
        version: String,
        /// Sessions with a turn running or a request waiting.
        working: u32,
        /// Whether it already waits for them before stopping.
        draining: bool,
    },
    /// The server is newer: Koda was updated since this client started.
    Newer {
        /// Its version.
        version: String,
    },
    /// A server of another version where this client doesn't manage it.
    OtherVersion {
        /// Its version.
        version: String,
    },
    /// A server from before 0.7.13, which refuses `forUpdate` while any
    /// window is open.
    Predates {
        /// Its version.
        version: String,
    },
    /// It couldn't be reached, started or replaced; `retry` when trying
    /// again later may help.
    Failed {
        /// What went wrong.
        message: String,
        /// Whether a later try may succeed.
        retry: bool,
    },
}

impl NotReady {
    fn failed(message: impl Into<String>) -> Self {
        Self::Failed {
            message: message.into(),
            retry: true,
        }
    }

    /// The name `koda serve --start` prints.
    pub fn status(&self) -> &'static str {
        match self {
            Self::Busy { .. } => "busy",
            Self::Newer { .. } => "newerServer",
            Self::OtherVersion { .. } => "otherVersion",
            Self::Predates { .. } => "predatesUpdate",
            Self::Failed { .. } => "failed",
        }
    }

    /// The server's version, when one answered.
    pub fn server_version(&self) -> Option<&str> {
        match self {
            Self::Busy { version, .. }
            | Self::Newer { version }
            | Self::OtherVersion { version }
            | Self::Predates { version } => Some(version),
            Self::Failed { .. } => None,
        }
    }

    /// What to tell a person, for a client of version `ours` at `socket`.
    pub fn message(&self, ours: &str, socket: &Path) -> String {
        match self {
            Self::Busy {
                version,
                working,
                draining: false,
            } => format!(
                "Koda's server {version}, older than this koda ({ours}), has a turn running \
                 in {working} session(s): wait for them, or stop them, to replace it"
            ),
            Self::Busy {
                version, working, ..
            } => format!(
                "Koda's server {version} restarts once the turns running in {working} \
                 session(s) finish"
            ),
            Self::Newer { version } => format!(
                "Koda was updated to {version}, and its server runs that version; this koda \
                 is {ours}. Run the updated koda (check `which koda`), or use --no-daemon."
            ),
            Self::OtherVersion { version } => format!(
                "koda serve at {} is version {version}; this koda is {ours}. Run the same \
                 version, or leave out --remote.",
                socket.display()
            ),
            Self::Predates { version } => format!(
                "Koda's server {version} predates updating in place and has windows open. Run \
                 `koda serve --stop` (it stops their turns), then koda again."
            ),
            Self::Failed { message, .. } => message.clone(),
        }
    }
}

/// Make sure a server of this client's version answers at `how.socket`,
/// and return a connection to it.
pub async fn ensure(how: &Ensure<'_>) -> Result<(InProcessClient, Status), NotReady> {
    let ours = how.client_info.version.clone();
    let (connected, mut status) = match connect(how).await {
        Ok(client) => (client, Status::AlreadyRunning),
        Err(e) if !how.manage || !crate::reconnect::worth_retrying(&e) => {
            return Err(NotReady::Failed {
                message: e.to_string(),
                retry: crate::reconnect::worth_retrying(&e),
            });
        }
        Err(_) => {
            let server = start_server(how)?;
            (wait_for(how, server).await?, Status::Started)
        }
    };
    let theirs = connected.server().server_info.version.clone();
    match version::compare(&theirs, &ours) {
        ServerVersion::Same => return Ok((connected, status)),
        ServerVersion::Newer => {
            connected.shutdown().await;
            return Err(NotReady::Newer { version: theirs });
        }
        ServerVersion::Older if !how.manage => {
            connected.shutdown().await;
            return Err(NotReady::OtherVersion { version: theirs });
        }
        // One just started can be older only if another koda started it
        // first; it isn't replaced in a loop.
        ServerVersion::Older if status == Status::Started => {
            connected.shutdown().await;
            return Err(NotReady::failed(format!(
                "Koda's server came up as {theirs}, not {ours}: another koda started it. \
                 Quit the other koda windows, then run koda again."
            )));
        }
        ServerVersion::Older => {}
    }
    make_way(connected, how, &theirs, &ours).await?;
    let server = start_server(how)?;
    let replaced = wait_for(how, server).await?;
    let now = replaced.server().server_info.version.clone();
    if now != ours {
        replaced.shutdown().await;
        return Err(NotReady::failed(format!(
            "Koda's server came back as {now}, not {ours}: another koda started it. Quit the \
             other koda windows, then run koda again."
        )));
    }
    status = Status::Restarted;
    Ok((replaced, status))
}

/// Ask the older server on `connected` to make way for version `ours`, and
/// wait until it has gone: at once when no turn runs in it; otherwise as
/// `how.running_turns` says.
async fn make_way(
    connected: InProcessClient,
    how: &Ensure<'_>,
    theirs: &str,
    ours: &str,
) -> Result<(), NotReady> {
    // Which process it is, before it is asked to go: another client may
    // start the next server at this socket as soon as it has.
    let server = server_pid(how.socket).await;
    let gone = |limit| async move {
        if wait_until_gone(how.socket, server, limit).await {
            Ok(())
        } else {
            Err(NotReady::failed(format!(
                "koda serve at {} didn't stop in time",
                how.socket.display()
            )))
        }
    };
    let ask = |if_idle, after_turns| {
        connected.request::<ServeStop>(ServeStopParams {
            if_idle,
            for_update: Some(ours.to_string()),
            after_turns,
        })
    };
    let answer = match ask(true, false).await {
        Ok(answer) => answer,
        Err(e) => {
            connected.shutdown().await;
            return Err(NotReady::failed(format!(
                "Koda's server {theirs} couldn't be asked to make way: {e}"
            )));
        }
    };
    if answer.stopped {
        connected.shutdown().await;
        return gone(Some(READY_WAIT)).await;
    }
    // A server that predates `forUpdate` refuses while any window is open.
    if answer.working == 0 && !answer.draining {
        connected.shutdown().await;
        return Err(NotReady::Predates {
            version: theirs.to_string(),
        });
    }
    let busy = NotReady::Busy {
        version: theirs.to_string(),
        working: answer.working,
        draining: answer.draining,
    };
    let Some(turns) = how.running_turns else {
        connected.shutdown().await;
        return Err(busy);
    };
    // Another client asked it already: only waiting is left, as long as
    // its turns take. The caller can drop the future to stop waiting; the
    // server stops after those turns all the same.
    if answer.draining {
        connected.shutdown().await;
        return gone(None).await;
    }
    let answer = ask(false, turns == RunningTurns::Wait).await;
    connected.shutdown().await;
    let answer =
        answer.map_err(|e| NotReady::failed(format!("Koda's server didn't make way: {e}")))?;
    if answer.draining {
        gone(None).await
    } else {
        gone(Some(STOP_WAIT)).await
    }
}

async fn connect(how: &Ensure<'_>) -> Result<InProcessClient, RequestError> {
    InProcessClient::connect(
        how.socket,
        how.project_root.to_path_buf(),
        how.client_info.clone(),
    )
    .await
}

/// Start `koda serve` on the socket in the background, in its own process
/// group so a signal to the caller's terminal doesn't reach it, stopping
/// once idle.
fn start_server(how: &Ensure<'_>) -> Result<tokio::process::Child, NotReady> {
    let mut command = tokio::process::Command::new(&how.koda);
    command
        .arg("serve")
        .arg("--listen")
        .arg(format!("unix://{}", how.socket.display()))
        .arg("--idle-exit-secs")
        .arg(IDLE_EXIT_SECS.to_string())
        .arg("--project-root")
        .arg(how.project_root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(false);
    #[cfg(unix)]
    command.process_group(0);
    command
        .spawn()
        .map_err(|e| NotReady::failed(format!("couldn't start koda serve: {e}")))
}

/// Connect once a server answers at `how.socket`: `server`, just started
/// here, or one another koda started at the same moment. Codex's
/// `wait_until_ready` waits the same way, and stops waiting when its
/// server has exited; Koda waits a little longer then, since nothing keeps
/// two kodas from starting a server at once.
async fn wait_for(
    how: &Ensure<'_>,
    mut server: tokio::process::Child,
) -> Result<InProcessClient, NotReady> {
    let mut deadline = Instant::now() + READY_WAIT;
    let mut exited = None;
    loop {
        // A server that listens and never answers doesn't hold the wait
        // past its end.
        let left = deadline.saturating_duration_since(Instant::now());
        let why = match tokio::time::timeout(left, connect(how)).await {
            Ok(Ok(client)) => return Ok(client),
            Ok(Err(e)) => e.to_string(),
            Err(_) => "it didn't answer".to_string(),
        };
        if Instant::now() >= deadline {
            let socket = how.socket.display();
            let what = match exited {
                Some(status) => {
                    format!("koda serve exited ({status}) before it answered at {socket}: {why}")
                }
                None => format!(
                    "koda serve didn't start at {socket} within {} s: {why}",
                    READY_WAIT.as_secs()
                ),
            };
            return Err(NotReady::failed(format!(
                "{what}. Run `koda serve` in a terminal to see why; to work without the \
                 background server, rerun with --no-daemon."
            )));
        }
        if exited.is_none()
            && let Ok(Some(status)) = server.try_wait()
        {
            exited = Some(status);
            deadline = deadline.min(Instant::now() + EXITED_WAIT);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The process listening at `socket`, from the connection's peer
/// credentials: what tells one server from the next one at the same path,
/// as the pid record tells Codex's daemon which process it stops. `None`
/// when nothing listens there or the system doesn't say.
pub async fn server_pid(socket: &Path) -> Option<u32> {
    let stream = tokio::net::UnixStream::connect(socket).await.ok()?;
    peer_pid(&stream)
}

fn peer_pid(stream: &tokio::net::UnixStream) -> Option<u32> {
    let pid = stream.peer_cred().ok()?.pid()?;
    u32::try_from(pid).ok()
}

/// Wait until the server that was process `server` (from [`server_pid`],
/// asked before it was told to stop) has gone from `socket`, for up to
/// `limit`, or as long as it takes without one. Whether it has gone.
///
/// Gone is: nothing listens at `socket`, or another process does, as when a
/// client that lost the server has started the next one already. Codex's
/// daemon waits for the process it stopped in the same way, not for the
/// socket's path to be free. A server that still listens hangs up on a
/// connection it hasn't taken only as it closes its socket, which it does
/// as it lets go of its lock: so once this returns, a server started next
/// finds the lock free, as it did when this waited for the path to go.
pub async fn wait_until_gone(socket: &Path, server: Option<u32>, limit: Option<Duration>) -> bool {
    use std::io::ErrorKind::{ConnectionRefused, NotFound};
    use tokio::io::AsyncReadExt;
    let gone = async {
        loop {
            match tokio::net::UnixStream::connect(socket).await {
                Ok(mut stream) => {
                    if let (Some(was), Some(now)) = (server, peer_pid(&stream))
                        && was != now
                    {
                        return;
                    }
                    // Still its socket, or no telling whose: wait for it to
                    // hang up, then look again.
                    let mut sent = [0u8; 256];
                    while let Ok(1..) = stream.read(&mut sent).await {}
                }
                // It removed its socket, or left a dead one behind.
                Err(e) if matches!(e.kind(), NotFound | ConnectionRefused) => return,
                Err(_) => {}
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    match limit {
        Some(limit) => tokio::time::timeout(limit, gone).await.is_ok(),
        None => {
            gone.await;
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_are_named_as_codexs_daemon_names_them() {
        assert_eq!(Status::AlreadyRunning.as_str(), "alreadyRunning");
        assert_eq!(Status::Started.as_str(), "started");
        assert_eq!(Status::Restarted.as_str(), "restarted");
    }

    /// A server that exits without answering fails the wait as one that
    /// exited, soon after it has, not as one still slow to come up at the
    /// end of the whole wait.
    #[tokio::test]
    async fn a_server_that_exits_before_it_answers_is_said_to_have_exited() {
        let folder = tempfile::tempdir().unwrap();
        let socket = folder.path().join("koda.sock");
        let how = Ensure {
            socket: &socket,
            project_root: folder.path(),
            // In place of `koda serve`: it exits at once, with status 1.
            koda: PathBuf::from("false"),
            manage: true,
            running_turns: None,
            client_info: ClientInfo {
                name: "ensure-test".to_string(),
                version: "0".to_string(),
                title: None,
            },
        };
        let said = match ensure(&how).await {
            Ok(_) => panic!("ready with no server"),
            Err(not_ready) => not_ready.message("0", &socket),
        };
        assert!(
            said.contains("koda serve exited (exit status: 1) before it answered"),
            "{said}"
        );
    }

    #[test]
    fn a_newer_server_says_koda_was_updated() {
        let said = NotReady::Newer {
            version: "0.7.14".into(),
        }
        .message("0.7.13", Path::new("/s"));
        assert!(said.contains("updated to 0.7.14"), "{said}");
        assert!(said.contains("this koda is 0.7.13"), "{said}");
    }
}
