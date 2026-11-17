//! Which host the TUI talks to: the shared server (`koda serve`), as Codex's
//! TUI uses its app-server daemon, or a host in its own process.
//!
//! - **The shared server, by default.** The TUI connects to Koda's socket and
//!   starts `koda serve` in the background when nothing answers, as Codex's
//!   TUI starts its daemon. A server the TUI starts stops once it has had no
//!   client and no work for ten minutes.
//! - **A server of another version** is stopped and replaced when it has
//!   nothing in use (`serve/stop` with `ifIdle`), as Claude Code's supervisor
//!   restarts onto a new version when idle; a busy one is left alone, and this
//!   run uses its own host, with a note, as Codex falls back to embedded.
//! - **In its own process** with `--no-daemon`, and for runs the shared
//!   server can't serve as asked, as Codex's TUI does: `--oss` (its model
//!   server and key live in this process) and `-c` values (the server's
//!   config is its own).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use koda_client::InProcessClient;
use koda_client::protocol::initialize::ClientInfo;

/// How long a server the TUI starts stays up with no client and no work.
const IDLE_EXIT_SECS: u64 = 600;
/// How long the TUI waits for a server it started, or stopped, to be ready.
const READY_WAIT: Duration = Duration::from_secs(10);

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
    if theirs == ours {
        return Ok((connected, None));
    }
    if !*start {
        connected.shutdown().await;
        anyhow::bail!(
            "koda serve at {} is version {theirs}; this koda is {ours}. \
             Run the same version, or leave out --remote.",
            socket.display()
        );
    }
    // Another version: replace it if nothing is in use, else leave it be.
    let asked = connected
        .request::<koda_client::protocol::methods::ServeStop>(
            koda_client::protocol::serve::ServeStopParams { if_idle: true },
        )
        .await;
    connected.shutdown().await;
    match asked {
        Ok(answer) if answer.stopped => {
            wait_gone(socket).await?;
            start_server(socket, project_root)?;
            Ok((wait_for(socket, project_root).await?, None))
        }
        refused => {
            let why = match refused {
                Ok(answer) => answer.reason.unwrap_or_else(|| "it is in use".into()),
                Err(_) => "it can't be asked to stop".into(),
            };
            let note = format!(
                "Running without the shared background server: koda serve {theirs} is \
                 running ({why}), and this koda is {ours}. It stops once idle for ten \
                 minutes, or with koda serve --stop."
            );
            Ok((in_process(project_root, home).await?, Some(note)))
        }
    }
}

fn client_info() -> ClientInfo {
    ClientInfo {
        name: "koda-tui".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
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

/// Wait until the server at `socket` has stopped and removed it.
async fn wait_gone(socket: &Path) -> anyhow::Result<()> {
    let deadline = Instant::now() + READY_WAIT;
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
