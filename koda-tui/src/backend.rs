//! Which host the TUI talks to: the shared server (`koda serve`), as Codex's
//! TUI uses its app-server daemon, or a host in its own process.
//!
//! - **The shared server, by default.** The TUI connects to Koda's socket and
//!   starts `koda serve` in the background when nothing answers, as Codex's
//!   TUI starts its daemon. A server the TUI starts stops once it has had no
//!   client and no work for ten minutes.
//! - **Only a server of its own version.** Koda runs one version (Lijun,
//!   2026-11-26). An older server is replaced (`serve/stop` with
//!   `forUpdate`): at once when no turn is running in it, whatever clients
//!   are connected, as Claude Code's supervisor restarts onto a new version;
//!   otherwise the user chooses between waiting for those turns, as Codex's
//!   app-server drains, and stopping them. A newer server means Koda was
//!   updated since this koda started: it isn't used, and nothing replaces
//!   it. Nothing here keeps working with a server from before 0.7.13: the
//!   user is told to stop it (`koda serve --stop`).
//! - **In its own process** with `--no-daemon`, and for runs the shared
//!   server can't serve as asked, as Codex's TUI does: `--oss` (its model
//!   server and key live in this process) and `-c` values (the server's
//!   config is its own).

use std::path::{Path, PathBuf};

use koda_client::InProcessClient;
use koda_client::ensure::{self, Ensure, NotReady, RunningTurns};
use koda_client::protocol::initialize::ClientInfo;
use koda_client::reconnect::Failure;

/// Where the TUI's host is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// A host in the TUI's own process.
    InProcess,
    /// The shared server at `socket`; `start` starts one when none answers
    /// (Koda's own socket), and a server elsewhere (`--remote`) is never
    /// started or replaced.
    Shared {
        /// The socket.
        socket: PathBuf,
        /// Whether this is Koda's own socket, which the TUI manages.
        start: bool,
    },
}

/// The TUI's client on `backend`, and a note to show if it couldn't use the
/// shared server as asked.
pub(crate) async fn client(
    backend: &Backend,
    project_root: &Path,
    home: &Path,
) -> anyhow::Result<(InProcessClient, Option<String>)> {
    let Backend::Shared { socket, start } = backend else {
        return Ok((in_process(project_root, home).await?, None));
    };
    let mut how = how(socket, *start, project_root)?;
    let ours = env!("CARGO_PKG_VERSION");
    loop {
        let tried = if how.running_turns == Some(RunningTurns::Wait) {
            tokio::select! {
                tried = ensure::ensure(&how) => tried,
                _ = tokio::signal::ctrl_c() => anyhow::bail!(
                    "Koda's server restarts once those turns finish; run koda again then."
                ),
            }
        } else {
            ensure::ensure(&how).await
        };
        let not_ready = match tried {
            Ok((client, _)) => return Ok((client, None)),
            Err(not_ready) => not_ready,
        };
        let NotReady::Busy {
            version,
            working,
            draining,
        } = &not_ready
        else {
            anyhow::bail!("{}", not_ready.message(ours, socket));
        };
        if how.running_turns.is_some() {
            anyhow::bail!("{}", not_ready.message(ours, socket));
        }
        let choice = if *draining {
            Restart::AfterTurns
        } else {
            choose(version, ours, *working).await
        };
        how.running_turns = Some(match choice {
            Restart::AfterTurns => {
                eprintln!(
                    "Waiting for the turns running in {working} session(s) to finish before \
                     Koda's server restarts\u{2026} (Ctrl+C leaves; it restarts once they finish)"
                );
                RunningTurns::Wait
            }
            Restart::Now => RunningTurns::Stop,
            Restart::Quit => anyhow::bail!(
                "Left Koda's server {version} running. Run koda again to restart it onto \
                 {ours}, or use --no-daemon."
            ),
        });
    }
}

/// What the TUI makes sure of at `socket`.
fn how<'a>(socket: &'a Path, manage: bool, project_root: &'a Path) -> anyhow::Result<Ensure<'a>> {
    let koda = std::env::current_exe()
        .map_err(|e| anyhow::anyhow!("couldn't find koda to start koda serve: {e}"))?;
    Ok(Ensure {
        socket,
        project_root,
        koda,
        manage,
        running_turns: None,
        client_info: client_info(),
    })
}

/// What the user chose for an older server with turns running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Restart {
    /// Wait for the turns to finish, then restart.
    AfterTurns,
    /// Stop them and restart now.
    Now,
    /// Leave the server be and quit.
    Quit,
}

/// Ask the user what to do with an older server that has `working`
/// sessions with a turn running, as Codex's TUI confirms a restart that
/// "may interrupt active or queued work". Waiting is the answer when there
/// is no terminal to ask on.
async fn choose(theirs: &str, ours: &str, working: u32) -> Restart {
    use std::io::{IsTerminal, Write};
    if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
        return Restart::AfterTurns;
    }
    let sessions = if working == 1 {
        "1 session".to_string()
    } else {
        format!("{working} sessions")
    };
    let mut err = std::io::stderr();
    let _ = write!(
        err,
        "Koda was updated to {ours}. Its server still runs {theirs}, with a turn running in \
         {sessions}.\n  \
         w  wait for them to finish, then restart it (new turns there are refused meanwhile)\n  \
         r  restart it now: those turns stop, saved as interrupted\n  \
         q  quit and leave it running\n\
         Restart Koda's server? [W/r/q] "
    );
    let _ = err.flush();
    let line = tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).map(|_| line)
    })
    .await;
    match line {
        Ok(Ok(line)) => parse_choice(&line),
        _ => Restart::Quit,
    }
}

/// The user's answer: `w` or nothing waits, `r` restarts now, anything
/// else quits.
fn parse_choice(line: &str) -> Restart {
    match line.trim().to_ascii_lowercase().as_str() {
        "" | "w" | "wait" => Restart::AfterTurns,
        "r" | "restart" => Restart::Now,
        _ => Restart::Quit,
    }
}

/// One try at getting back to the shared server after losing it: connect,
/// and start Koda's own server when nothing answers, as at start-up (Codex's
/// TUI only connects, as its daemon is brought back by its own manager;
/// Koda's TUI is what starts Koda's). Only a server of this version is
/// used: a newer one means Koda was updated, and this window stops; an
/// older one is replaced when no turn runs in it, and tried again later
/// when one does, since there is no one to ask here.
pub(crate) async fn reconnect_once(
    backend: &Backend,
    project_root: &Path,
) -> Result<(InProcessClient, Option<String>), Failure<String>> {
    let Backend::Shared { socket, start } = backend else {
        return Err(Failure::Stop(
            "Koda's host ran in this process and has stopped".into(),
        ));
    };
    let how = how(socket, *start, project_root).map_err(|e| Failure::Stop(e.to_string()))?;
    let ours = env!("CARGO_PKG_VERSION");
    match ensure::ensure(&how).await {
        Ok((client, _)) => Ok((client, None)),
        Err(e @ (NotReady::Newer { .. } | NotReady::OtherVersion { .. })) => {
            Err(Failure::Stop(e.message(ours, socket)))
        }
        Err(NotReady::Failed {
            message,
            retry: false,
        }) => Err(Failure::Stop(message)),
        Err(e) => Err(Failure::Retry(e.message(ours, socket))),
    }
}

fn client_info() -> ClientInfo {
    ClientInfo {
        name: "koda-tui".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        title: None,
    }
}

async fn in_process(project_root: &Path, home: &Path) -> anyhow::Result<InProcessClient> {
    InProcessClient::start(koda_client::StartArgs {
        config_file: crate::user_config::file()?,
        home: home.to_path_buf(),
        project_root: project_root.to_path_buf(),
        client_info: client_info(),
    })
    .await
    .map_err(|e| anyhow::anyhow!("couldn't start Koda's host: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Enter or `w` waits for the turns, `r` restarts now, anything else
    /// leaves the server be.
    #[test]
    fn the_answer_to_restarting_an_older_server() {
        for wait in ["\n", "w\n", " W ", "wait"] {
            assert_eq!(parse_choice(wait), Restart::AfterTurns, "{wait:?}");
        }
        for now in ["r\n", "R", "restart"] {
            assert_eq!(parse_choice(now), Restart::Now, "{now:?}");
        }
        for quit in ["q\n", "n", "later"] {
            assert_eq!(parse_choice(quit), Restart::Quit, "{quit:?}");
        }
    }
}
