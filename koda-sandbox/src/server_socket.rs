//! Koda's shared server (`koda serve`), closed to sandboxed commands.
//!
//! The server acts for the user: whoever can send it requests can answer an
//! approval or add an MCP server, which Koda starts outside the sandbox. The
//! socket's own checks can't tell a sandboxed command from a client, since
//! both run as the user, so both backends hide the socket, as Codex's hide
//! its daemon's: the folder can't be read, written or connected into, and no
//! folder above it can be renamed to bring it out from under the rule.
//!
//! - **Its usual folder** (`koda/serve` in Koda's config folder) is always
//!   closed, whether or not a server runs, so one that starts during a
//!   command is closed to it too.
//! - **A socket somewhere else** (`koda serve --listen unix://…`) is closed
//!   to the commands of the process listening there ([`protect_socket`]).

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

/// The folder of the server's socket, in a config folder (`~/.config`).
pub const SERVER_SUBDIR: &str = "koda/serve";

static SOCKETS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Close `socket`, which this process listens at, to every sandboxed command
/// it starts from now on.
pub fn protect_socket(socket: &Path) {
    let socket = std::path::absolute(socket).unwrap_or_else(|_| socket.to_path_buf());
    let mut sockets = SOCKETS.lock().unwrap_or_else(PoisonError::into_inner);
    if !sockets.contains(&socket) {
        sockets.push(socket);
    }
}

/// The sockets [`protect_socket`] was given, as the kernel names them.
pub(crate) fn sockets() -> Vec<PathBuf> {
    SOCKETS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .map(|socket| real(socket))
        .collect()
}

/// The server's usual folders for `home`, as the kernel names them.
pub(crate) fn folders(home: &str) -> Vec<PathBuf> {
    crate::defaults::config_folders(home)
        .into_iter()
        .map(|config| real(&config.join(SERVER_SUBDIR)))
        .collect()
}

/// `path` with every symlink in the part that exists resolved, so a rule
/// made before the folder exists still names it once it does.
fn real(path: &Path) -> PathBuf {
    path.ancestors()
        .find_map(|ancestor| {
            let real = ancestor.canonicalize().ok()?;
            let rest = path.strip_prefix(ancestor).ok()?;
            Some(if rest.as_os_str().is_empty() {
                real
            } else {
                real.join(rest)
            })
        })
        .unwrap_or_else(|| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_is_resolved_as_far_as_it_exists() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("there")).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("there"), root.join("link")).unwrap();
            assert_eq!(real(&root.join("link/not/yet")), root.join("there/not/yet"));
        }
        assert_eq!(real(&root.join("there")), root.join("there"));
    }
}
