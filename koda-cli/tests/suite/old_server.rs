//! A Koda server of another version on Koda's socket, as an update leaves
//! one running: what the TUI and `koda serve --start` replace or leave be.

#![cfg(unix)]

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How a fake server of another version answers `serve/stop`.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum OldServer {
    /// Nothing is running: it stops.
    Stops,
    /// A server from before 0.7.13: it refuses while any window is open,
    /// saying nothing of turns.
    Refuses,
    /// A turn is running in one session: asked only if idle it says so;
    /// told to wait it drains for a moment; told to stop it stops.
    Busy,
}

/// A server of version `version` on Koda's socket, as an update leaves one:
/// it answers `initialize`, and `serve/stop` as `kind` says, keeping each
/// `serve/stop` it was sent in `asked`.
pub(super) fn old_server(
    socket: &Path,
    version: &'static str,
    kind: OldServer,
    asked: Arc<Mutex<Vec<serde_json::Value>>>,
) -> tokio::task::JoinHandle<()> {
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    std::fs::set_permissions(
        socket.parent().unwrap(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    let socket = socket.to_path_buf();
    tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (input, mut output) = stream.into_split();
            let mut lines = tokio::io::BufReader::new(input).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                let params = &request["params"];
                let flag = |name: &str| params[name].as_bool().unwrap_or(false);
                // `Some(delay)`: stop after answering, after `delay`.
                let mut stop = None;
                let result = match request["method"].as_str() {
                    Some("initialize") => serde_json::json!({
                        "serverInfo": {"name": "koda", "version": version},
                    }),
                    Some("serve/stop") => {
                        asked.lock().unwrap().push(params.clone());
                        match kind {
                            OldServer::Stops => {
                                stop = Some(Duration::ZERO);
                                serde_json::json!({"stopped": true})
                            }
                            OldServer::Refuses => serde_json::json!(
                                {"stopped": false, "reason": "session s1 is in use"}
                            ),
                            OldServer::Busy if flag("ifIdle") => serde_json::json!(
                                {"stopped": false, "working": 1, "reason": "1 session(s) working"}
                            ),
                            OldServer::Busy if flag("afterTurns") => {
                                stop = Some(Duration::from_millis(500));
                                serde_json::json!({"stopped": false, "working": 1, "draining": true})
                            }
                            OldServer::Busy => {
                                stop = Some(Duration::ZERO);
                                serde_json::json!({"stopped": true, "working": 1})
                            }
                        }
                    }
                    _ => continue,
                };
                let answer =
                    serde_json::json!({"jsonrpc": "2.0", "id": request["id"], "result": result});
                let _ = output.write_all(format!("{answer}\n").as_bytes()).await;
                if let Some(delay) = stop {
                    tokio::time::sleep(delay).await;
                    let _ = std::fs::remove_file(&socket);
                    return;
                }
            }
        }
    })
}
