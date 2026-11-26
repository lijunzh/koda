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
use std::time::{Duration, Instant};

use koda_client::InProcessClient;
use koda_client::protocol::initialize::ClientInfo;
use koda_client::protocol::serve::ServeStopParams;
use koda_client::reconnect::{Failure, worth_retrying};
use koda_client::version::{self, ServerVersion};

/// How long a server the TUI starts stays up with no client and no work.
const IDLE_EXIT_SECS: u64 = 600;
/// How long the TUI waits for a server it started, or stopped, to be ready.
const READY_WAIT: Duration = Duration::from_secs(10);
/// How long it waits for a server stopping its running turns to be gone.
const STOP_WAIT: Duration = Duration::from_secs(60);

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
    let connected = match connect(socket, project_root).await {
        Ok(client) => client,
        Err(_) if *start => {
            start_server(socket, project_root)?;
            wait_for(socket, project_root).await?
        }
        Err(e) => anyhow::bail!("{e}"),
    };
    let theirs = connected.server().server_info.version.clone();
    let ours = env!("CARGO_PKG_VERSION");
    match version::compare(&theirs, ours) {
        ServerVersion::Same => return Ok((connected, None)),
        ServerVersion::Newer => {
            connected.shutdown().await;
            anyhow::bail!("{}", newer_server(&theirs, ours));
        }
        ServerVersion::Older => {}
    }
    if !*start {
        connected.shutdown().await;
        anyhow::bail!(
            "koda serve at {} is version {theirs}; this koda is {ours}. \
             Run the same version, or leave out --remote.",
            socket.display()
        );
    }
    make_way(connected, socket, &theirs, ours).await?;
    start_server(socket, project_root)?;
    let replaced = wait_for(socket, project_root).await?;
    let now = replaced.server().server_info.version.clone();
    if now != ours {
        replaced.shutdown().await;
        anyhow::bail!(
            "Koda's server came back as {now}, not {ours}: another koda started it. Quit \
             the other koda windows, then run koda again."
        );
    }
    Ok((replaced, None))
}

/// Why a newer server isn't used.
fn newer_server(theirs: &str, ours: &str) -> String {
    format!(
        "Koda was updated to {theirs}, and its server runs that version; this koda is \
         {ours}. Run the updated koda (check `which koda`), or use --no-daemon."
    )
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

/// Ask the older server on `connected` to make way for this version, and
/// wait until it has gone: at once when no turn runs in it; otherwise after
/// the user chooses, or after the turns when nobody can be asked.
async fn make_way(
    connected: InProcessClient,
    socket: &Path,
    theirs: &str,
    ours: &str,
) -> anyhow::Result<()> {
    let ask = |if_idle, after_turns| {
        connected.request::<koda_client::protocol::methods::ServeStop>(ServeStopParams {
            if_idle,
            for_update: Some(ours.to_string()),
            after_turns,
        })
    };
    let answer = ask(true, false).await;
    let answer = match answer {
        Ok(answer) => answer,
        Err(e) => {
            connected.shutdown().await;
            anyhow::bail!("Koda's server {theirs} couldn't be asked to make way: {e}");
        }
    };
    if answer.stopped {
        connected.shutdown().await;
        return wait_gone(socket, READY_WAIT).await;
    }
    // Another client asked it already.
    if answer.draining {
        connected.shutdown().await;
        return wait_for_turns(socket, answer.working).await;
    }
    // A server that predates `forUpdate` refuses while any window is open.
    if answer.working == 0 {
        connected.shutdown().await;
        anyhow::bail!(
            "Koda's server {theirs} predates updating in place and has windows open. Run \
             `koda serve --stop` (it stops their turns), then koda again."
        );
    }
    let choice = choose(theirs, ours, answer.working).await;
    if choice == Restart::Quit {
        connected.shutdown().await;
        anyhow::bail!(
            "Left Koda's server {theirs} running. Run koda again to restart it onto {ours}, \
             or use --no-daemon."
        );
    }
    let answer = ask(false, choice == Restart::AfterTurns).await;
    connected.shutdown().await;
    let answer = answer.map_err(|e| anyhow::anyhow!("Koda's server didn't make way: {e}"))?;
    if answer.draining {
        wait_for_turns(socket, answer.working).await
    } else {
        wait_gone(socket, STOP_WAIT).await
    }
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

/// Wait, as long as it takes, for a server waiting on `working` sessions'
/// turns to stop. Ctrl+C leaves; it stops after them all the same.
async fn wait_for_turns(socket: &Path, working: u32) -> anyhow::Result<()> {
    eprintln!(
        "Waiting for the turns running in {working} session(s) to finish before Koda's \
         server restarts\u{2026} (Ctrl+C leaves; it restarts once they finish)"
    );
    let gone = async {
        while socket.exists() {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    };
    tokio::select! {
        () = gone => Ok(()),
        _ = tokio::signal::ctrl_c() => anyhow::bail!(
            "Koda's server restarts once those turns finish; run koda again then."
        ),
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
    let connected =
        InProcessClient::connect(socket, project_root.to_path_buf(), client_info()).await;
    let client = match connected {
        Ok(client) => client,
        Err(e) if !worth_retrying(&e) => return Err(Failure::Stop(e.to_string())),
        Err(_) if *start => {
            start_server(socket, project_root).map_err(|e| Failure::Retry(e.to_string()))?;
            wait_for(socket, project_root)
                .await
                .map_err(|e| Failure::Retry(e.to_string()))?
        }
        Err(e) => return Err(Failure::Retry(e.to_string())),
    };
    let theirs = client.server().server_info.version.clone();
    let ours = env!("CARGO_PKG_VERSION");
    match version::compare(&theirs, ours) {
        ServerVersion::Same => Ok((client, None)),
        ServerVersion::Newer => {
            client.shutdown().await;
            Err(Failure::Stop(newer_server(&theirs, ours)))
        }
        ServerVersion::Older if !*start => {
            client.shutdown().await;
            Err(Failure::Stop(format!(
                "koda serve at {} is now version {theirs}; this koda is {ours}",
                socket.display()
            )))
        }
        ServerVersion::Older => {
            let asked = client
                .request::<koda_client::protocol::methods::ServeStop>(ServeStopParams {
                    if_idle: true,
                    for_update: Some(ours.to_string()),
                    after_turns: false,
                })
                .await;
            client.shutdown().await;
            match asked {
                Ok(answer) if answer.stopped => {
                    wait_gone(socket, READY_WAIT)
                        .await
                        .map_err(|e| Failure::Retry(e.to_string()))?;
                    start_server(socket, project_root)
                        .map_err(|e| Failure::Retry(e.to_string()))?;
                    let client = wait_for(socket, project_root)
                        .await
                        .map_err(|e| Failure::Retry(e.to_string()))?;
                    Ok((client, None))
                }
                _ => Err(Failure::Retry(format!(
                    "koda serve {theirs}, older than this koda ({ours}), is still running turns"
                ))),
            }
        }
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

async fn connect(socket: &Path, project_root: &Path) -> anyhow::Result<InProcessClient> {
    InProcessClient::connect(socket, project_root.to_path_buf(), client_info())
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Start `koda serve` on `socket` in the background, in its own process
/// group so Ctrl+C in this terminal doesn't reach it, stopping once idle.
fn start_server(socket: &Path, project_root: &Path) -> anyhow::Result<()> {
    let exe = std::env::current_exe()
        .map_err(|e| anyhow::anyhow!("couldn't find koda to start koda serve: {e}"))?;
    let mut command = tokio::process::Command::new(exe);
    command
        .arg("serve")
        .arg("--listen")
        .arg(format!("unix://{}", socket.display()))
        .arg("--idle-exit-secs")
        .arg(IDLE_EXIT_SECS.to_string())
        .arg("--project-root")
        .arg(project_root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(false);
    #[cfg(unix)]
    command.process_group(0);
    command
        .spawn()
        .map_err(|e| anyhow::anyhow!("couldn't start koda serve: {e}"))?;
    Ok(())
}

/// Connect once the server at `socket` answers.
async fn wait_for(socket: &Path, project_root: &Path) -> anyhow::Result<InProcessClient> {
    let deadline = Instant::now() + READY_WAIT;
    loop {
        match connect(socket, project_root).await {
            Ok(client) => return Ok(client),
            Err(e) if Instant::now() >= deadline => anyhow::bail!(
                "koda serve didn't start at {}: {e}. To work without the background \
                 server, rerun with --no-daemon.",
                socket.display()
            ),
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
}

/// Wait up to `limit` for the server at `socket` to stop and remove it.
async fn wait_gone(socket: &Path, limit: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + limit;
    while socket.exists() {
        anyhow::ensure!(
            Instant::now() < deadline,
            "koda serve at {} didn't stop in time",
            socket.display()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
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

    #[test]
    fn a_newer_server_says_koda_was_updated() {
        let said = newer_server("0.7.14", "0.7.13");
        assert!(said.contains("updated to 0.7.14"), "{said}");
        assert!(said.contains("this koda is 0.7.13"), "{said}");
    }
}
