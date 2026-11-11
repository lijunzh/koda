//! Reaching the servers of Koda's own background commands under bwrap.
//!
//! ## Why this exists
//!
//! bwrap gives every sandboxed command its own network namespace, so a dev
//! server one Bash call starts in the background listens on a loopback no
//! later call can see. Seatbelt shares the host's loopback and opens just the
//! ports Koda's background commands listen on; this is the same promise on
//! Linux: a command reaches Koda's own servers, and nothing else on the host
//! (the user's own servers, databases and Docker stay closed).
//!
//! Codex and sandbox-runtime both isolate each command's network on Linux
//! too, and neither bridges them.
//!
//! ## How
//!
//! Unix sockets cross network namespaces, as for the egress proxy
//! ([`crate::bwrap_proxy`]):
//!
//! 1. A background command's stage 2 **serves** its namespace on a Unix
//!    socket. The socket answers one request line per connection:
//!    - `ports`: the TCP ports listening in the namespace, one per line.
//!    - `connect PORT`: connects to `localhost:PORT` inside the namespace,
//!      then carries bytes both ways.
//! 2. Before each Bash call, Koda asks every running background command's
//!    socket for its ports ([`listening_ports`]).
//! 3. The new command's stage 2 listens on each of those ports on its own
//!    loopback and **forwards** every connection with `connect PORT`.
//!
//! The sockets live in a directory only the user can open, bound into the
//! sandbox. A port is looked up when a command starts, so a server that
//! starts listening later is reachable from the next command, as on macOS.

#![cfg(target_os = "linux")]

use crate::LoopbackForward;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, TcpListener, TcpStream};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Env var naming the socket a background command's stage 2 serves on.
pub const SERVE_ENV_KEY: &str = "KODA_SANDBOX_STAGE2_SERVE_LOOPBACK";

/// Env var listing the ports stage 2 forwards, as `PORT=SOCKET` lines.
pub const FORWARDS_ENV_KEY: &str = "KODA_SANDBOX_STAGE2_LOOPBACK_FORWARDS";

/// How long Koda waits on a background command's socket for its ports.
const QUERY_TIMEOUT: Duration = Duration::from_secs(1);

/// The directory this Koda process keeps its loopback sockets in.
pub fn socket_dir() -> PathBuf {
    std::env::temp_dir().join(format!("koda-loopback-{}", std::process::id()))
}

/// A new socket path for a background command to serve on, creating
/// [`socket_dir`] (only the user can open it) if needed.
pub fn new_socket_path() -> std::io::Result<PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = socket_dir();
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    Ok(dir.join(format!("{}.sock", NEXT.fetch_add(1, Ordering::Relaxed))))
}

/// The ports a background command's servers listen on, asked of the socket
/// it serves its namespace on.
pub fn listening_ports(socket: &Path) -> std::io::Result<Vec<u16>> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(QUERY_TIMEOUT))?;
    stream.set_write_timeout(Some(QUERY_TIMEOUT))?;
    stream.write_all(b"ports\n")?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply)?;
    Ok(reply
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect())
}

/// [`FORWARDS_ENV_KEY`]'s value for `forwards`.
pub(crate) fn encode_forwards(forwards: &[LoopbackForward]) -> String {
    forwards
        .iter()
        .map(|f| format!("{}={}\n", f.port, f.socket.display()))
        .collect()
}

fn decode_forwards(value: &str) -> Vec<(u16, PathBuf)> {
    value
        .lines()
        .filter_map(|line| {
            let (port, socket) = line.split_once('=')?;
            Some((port.parse().ok()?, PathBuf::from(socket)))
        })
        .collect()
}

/// TCP ports in `LISTEN` state in a `/proc/net/tcp` or `/proc/net/tcp6` table.
fn parse_proc_net_tcp(table: &str) -> Vec<u16> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let local = fields.nth(1)?;
            let state = fields.nth(1)?;
            (state == "0A").then_some(())?;
            u16::from_str_radix(local.rsplit_once(':')?.1, 16).ok()
        })
        .collect()
}

/// The TCP ports listening in this process's network namespace.
fn namespace_listening_ports() -> Vec<u16> {
    let mut ports: Vec<u16> = ["/proc/net/tcp", "/proc/net/tcp6"]
        .iter()
        .filter_map(|table| std::fs::read_to_string(table).ok())
        .flat_map(|table| parse_proc_net_tcp(&table))
        .collect();
    ports.sort_unstable();
    ports.dedup();
    ports
}

// ── Stage 2 ─────────────────────────────────────────────────────────────

/// What stage 2 binds for the loopback bridge, before it forks.
#[derive(Default)]
pub(crate) struct Bridge {
    forwards: Vec<(u16, TcpListener, PathBuf)>,
    serve: Option<UnixListener>,
}

impl Bridge {
    /// Bind what the env asks for. A port that can't be bound here (the
    /// command's own namespace is empty, so that's rare) is left out.
    pub(crate) fn bind_from_env() -> anyhow::Result<Self> {
        use anyhow::Context as _;
        let mut bridge = Self::default();
        for (port, socket) in decode_forwards(&std::env::var(FORWARDS_ENV_KEY).unwrap_or_default())
        {
            if let Ok(listener) = TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
                bridge.forwards.push((port, listener, socket));
            }
        }
        if let Some(path) = std::env::var_os(SERVE_ENV_KEY) {
            let path = PathBuf::from(path);
            let _ = std::fs::remove_file(&path);
            let listener = UnixListener::bind(&path)
                .with_context(|| format!("bind loopback socket {}", path.display()))?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .context("restrict loopback socket")?;
            bridge.serve = Some(listener);
        }
        // SAFETY: stage 2 is single-threaded until it forks.
        unsafe {
            std::env::remove_var(FORWARDS_ENV_KEY);
            std::env::remove_var(SERVE_ENV_KEY);
        }
        Ok(bridge)
    }

    /// Ports stage 2 listens on itself, which a `ports` reply leaves out.
    pub(crate) fn own_ports(&self) -> impl Iterator<Item = u16> + '_ {
        self.forwards.iter().map(|(port, _, _)| *port)
    }

    /// Start the bridge's threads in the stage 2 child. `own_ports` are left
    /// out of `ports` replies.
    pub(crate) fn spawn(self, own_ports: Vec<u16>) {
        for (port, listener, socket) in self.forwards {
            std::thread::spawn(move || {
                for tcp in listener.incoming().flatten() {
                    let socket = socket.clone();
                    std::thread::spawn(move || forward(tcp, &socket, port));
                }
            });
        }
        if let Some(listener) = self.serve {
            std::thread::spawn(move || {
                for client in listener.incoming().flatten() {
                    let own_ports = own_ports.clone();
                    std::thread::spawn(move || serve(client, &own_ports));
                }
            });
        }
    }
}

/// Carry one connection to `port` into the namespace behind `socket`.
fn forward(tcp: TcpStream, socket: &Path, port: u16) {
    let Ok(mut uds) = UnixStream::connect(socket) else {
        return;
    };
    if uds
        .write_all(format!("connect {port}\n").as_bytes())
        .is_ok()
    {
        let _ = crate::stage2::bridge_two_streams(tcp, uds);
    }
}

/// Answer one request on the socket this namespace is served on.
fn serve(mut client: UnixStream, own_ports: &[u16]) {
    let Some(request) = read_request_line(&mut client) else {
        return;
    };
    match request.split_once(' ') {
        None if request == "ports" => {
            let reply: String = namespace_listening_ports()
                .into_iter()
                .filter(|p| !own_ports.contains(p))
                .map(|p| format!("{p}\n"))
                .collect();
            let _ = client.write_all(reply.as_bytes());
        }
        Some(("connect", port)) => {
            let Ok(port) = port.parse::<u16>() else {
                return;
            };
            let tcp = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
                .or_else(|_| TcpStream::connect((Ipv6Addr::LOCALHOST, port)));
            if let Ok(tcp) = tcp {
                let _ = crate::stage2::bridge_two_streams(tcp, client);
            }
        }
        _ => {}
    }
}

/// The request line, read a byte at a time so nothing after it is consumed.
fn read_request_line(stream: &mut UnixStream) -> Option<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while line.len() < 32 {
        stream.read_exact(&mut byte).ok()?;
        if byte[0] == b'\n' {
            return String::from_utf8(line).ok();
        }
        line.push(byte[0]);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listening_ports_come_from_listen_rows() {
        let table = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 1 1
   1: 0100007F:9C40 0100007F:1F90 01 00000000:00000000 00:00000000 00000000  1000        0 2 1
   2: 00000000:0BB8 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 3 1
";
        assert_eq!(parse_proc_net_tcp(table), vec![8080, 3000]);
        assert!(parse_proc_net_tcp("").is_empty());
    }

    #[test]
    fn forwards_round_trip_through_the_env_value() {
        let forwards = vec![
            LoopbackForward {
                port: 8080,
                socket: PathBuf::from("/tmp/koda-loopback-1/0.sock"),
            },
            LoopbackForward {
                port: 5173,
                socket: PathBuf::from("/tmp/koda-loopback-1/1.sock"),
            },
        ];
        assert_eq!(
            decode_forwards(&encode_forwards(&forwards)),
            vec![
                (8080, PathBuf::from("/tmp/koda-loopback-1/0.sock")),
                (5173, PathBuf::from("/tmp/koda-loopback-1/1.sock")),
            ]
        );
        assert!(decode_forwards("not a forward\n=x\n").is_empty());
    }

    /// The socket protocol end to end, in this process's own namespace: a
    /// served socket reports a listening port, leaves out its own, and a
    /// forward through it reaches the server.
    #[test]
    fn a_served_socket_reports_ports_and_forwards_connections() {
        let server = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = server.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut conn in server.incoming().flatten() {
                let mut buf = [0u8; 4];
                conn.read_exact(&mut buf).unwrap();
                conn.write_all(b"pong").unwrap();
            }
        });

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("bg.sock");
        let own = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let own_port = own.local_addr().unwrap().port();
        Bridge {
            forwards: Vec::new(),
            serve: Some(UnixListener::bind(&socket).unwrap()),
        }
        .spawn(vec![own_port]);

        let ports = listening_ports(&socket).unwrap();
        assert!(ports.contains(&port), "{ports:?}");
        assert!(!ports.contains(&own_port), "{ports:?}");

        let front = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let front_port = front.local_addr().unwrap().port();
        Bridge {
            forwards: vec![(port, front, socket)],
            serve: None,
        }
        .spawn(Vec::new());
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, front_port)).unwrap();
        client.write_all(b"ping").unwrap();
        let mut reply = [0u8; 4];
        client.read_exact(&mut reply).unwrap();
        assert_eq!(&reply, b"pong");
    }
}
