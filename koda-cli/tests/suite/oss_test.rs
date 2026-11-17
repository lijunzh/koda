//! `koda --oss` with a stand-in `llama-server` on `PATH`: a small Python
//! server that answers what Koda asks a real one (`/health`, `/props`,
//! `/v1/models`, chat completions) and records each start. Koda starts it,
//! runs the turn on it, and stops it on exit, unless another Koda still
//! uses it. Skips without `python3`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn koda_bin() -> String {
    env!("CARGO_BIN_EXE_koda").to_string()
}

const MODEL: &str = "LiquidAI/LFM3-1.7B-GGUF:Q4_K_M";
/// `MODEL` as Koda names its files under `<config dir>/oss/`.
const SLUG: &str = "LiquidAI_LFM3-1.7B-GGUF_Q4_K_M";

const STAND_IN: &str = r#"#!/usr/bin/env python3
# A stand-in llama-server for Koda's --oss tests.
import json, os, socketserver, sys
from http.server import BaseHTTPRequestHandler

port = int(sys.argv[sys.argv.index("--port") + 1])
with open(os.environ["STAND_IN_STARTS"], "a") as f:
    f.write(f"{os.getpid()} {' '.join(sys.argv[1:])}\n")

class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass
    def reply(self, body, kind="application/json"):
        data = body.encode()
        self.send_response(200)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)
    def do_GET(self):
        if self.path == "/health":
            self.reply('{"status":"ok"}')
        elif self.path == "/props":
            self.reply(json.dumps({"default_generation_settings": {"n_ctx": 65536},
                                   "modalities": {"vision": False}}))
        elif self.path.endswith("/models"):
            self.reply(json.dumps({"object": "list", "data": [{"id": "LFM3-1.7B", "object": "model"}]}))
        else:
            self.send_response(404); self.end_headers()
    def do_POST(self):
        with open(os.environ["STAND_IN_STARTS"] + ".auth", "a") as f:
            f.write(f"{self.headers.get('Authorization')}\n")
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])) or b"{}")
        import time; time.sleep(float(os.environ.get("STAND_IN_DELAY", "0")))
        text = "Hello from the stand-in."
        if body.get("stream"):
            chunks = [
                {"choices": [{"index": 0, "delta": {"role": "assistant", "content": text}}]},
                {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                 "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}},
            ]
            self.reply("".join(f"data: {json.dumps(c)}\n\n" for c in chunks) + "data: [DONE]\n\n",
                       "text/event-stream")
        else:
            self.reply(json.dumps({"choices": [{"index": 0, "message": {"role": "assistant", "content": text},
                                                "finish_reason": "stop"}]}))

# socketserver's TCPServer, not http.server's: HTTPServer looks up the host's
# name (socket.getfqdn), a reverse DNS lookup that has stalled for 30 s on
# CI's macOS runners.
class Server(socketserver.ThreadingMixIn, socketserver.TCPServer):
    daemon_threads = True

Server(("127.0.0.1", port), Handler).serve_forever()
"#;

struct Setup {
    _tmp: tempfile::TempDir,
    bin: PathBuf,
    home: PathBuf,
    project: PathBuf,
    starts: PathBuf,
}

fn setup() -> Option<Setup> {
    if Command::new("python3").arg("--version").output().is_err() {
        eprintln!("python3 not installed; skipping");
        return None;
    }
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let server = bin.join("llama-server");
    std::fs::write(&server, STAND_IN).unwrap();
    std::fs::set_permissions(&server, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let home = tmp.path().join("config");
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let starts = tmp.path().join("starts.log");
    Some(Setup {
        bin,
        home,
        project,
        starts,
        _tmp: tmp,
    })
}

/// `koda exec --oss` with the stand-in first on `PATH`, started.
fn spawn(setup: &Setup, delay: &str) -> std::process::Child {
    let path = format!(
        "{}:{}",
        setup.bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    Command::new(koda_bin())
        .args(["exec", "--oss", "say hello"])
        .args(["-c", &format!("oss_model={MODEL}")])
        // The tests run stand-ins side by side, which the busy check counts.
        .args(["-c", "oss_check_gpu=false"])
        .arg("--project-root")
        .arg(&setup.project)
        .env("XDG_CONFIG_HOME", &setup.home)
        .env("PATH", path)
        .env("STAND_IN_STARTS", &setup.starts)
        .env("STAND_IN_DELAY", delay)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to run koda")
}

/// `koda exec --oss` with the stand-in first on `PATH`, to the end.
fn run(setup: &Setup) -> std::process::Output {
    let mut child = spawn(setup, "0");
    let deadline = Instant::now() + Duration::from_secs(120);
    while child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "koda hung");
        std::thread::sleep(Duration::from_millis(100));
    }
    child.wait_with_output().unwrap()
}

/// The stand-in's starts: one pid per start.
fn starts(setup: &Setup) -> Vec<u32> {
    std::fs::read_to_string(&setup.starts)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.split_whitespace().next()?.parse().ok())
        .collect()
}

fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|s| s.success())
}

fn wait_gone(pid: u32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(15);
    while alive(pid) {
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

fn oss_dir(home: &Path) -> PathBuf {
    home.join("koda").join("oss")
}

/// Koda starts the server with the model, runs the turn on it, and stops
/// it when the run ends.
#[test]
fn oss_starts_the_server_runs_on_it_and_stops_it() {
    let Some(setup) = setup() else { return };
    let output = run(&setup);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stdout.contains("Hello from the stand-in."),
        "{stdout}\n{stderr}"
    );
    assert!(stderr.contains("too small for real coding"), "{stderr}");

    let started = std::fs::read_to_string(&setup.starts).unwrap();
    assert!(
        started.contains(&format!("-hf {MODEL} --jinja -c 65536 -ngl 999")),
        "{started}"
    );
    let [pid] = starts(&setup)[..] else {
        panic!("one start expected: {started}")
    };
    // The server got a key, and Koda sent it.
    let key = started
        .split_whitespace()
        .skip_while(|word| *word != "--api-key")
        .nth(1)
        .expect("no --api-key")
        .to_string();
    assert_eq!(key.len(), 32, "{started}");
    let auth = std::fs::read_to_string(setup.starts.with_extension("log.auth")).unwrap();
    assert!(
        auth.lines().all(|line| line == format!("Bearer {key}")),
        "{auth}"
    );
    assert!(wait_gone(pid), "the server outlived koda");
    assert!(!oss_dir(&setup.home).join(format!("{SLUG}.json")).exists());
}

/// A server another Koda still uses stays up when this one exits, and the
/// next `koda --oss` joins it instead of starting a second; once its user
/// is gone, the last Koda stops it.
#[test]
fn oss_shares_one_server_and_the_last_koda_stops_it() {
    let Some(setup) = setup() else { return };
    // Another Koda, still running: a process whose pid is registered.
    let mut other = Command::new("sleep").arg("60").spawn().unwrap();
    let clients = oss_dir(&setup.home).join(format!("{SLUG}.clients"));
    std::fs::create_dir_all(&clients).unwrap();
    std::fs::write(clients.join(other.id().to_string()), b"").unwrap();

    let first = run(&setup);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let [pid] = starts(&setup)[..] else {
        panic!("one start expected")
    };
    assert!(alive(pid), "stopped while another koda used it");

    // The other Koda exits without a word, as a killed one would.
    other.kill().unwrap();
    other.wait().unwrap();

    let second = run(&setup);
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(starts(&setup).len(), 1, "a second server was started");
    assert!(wait_gone(pid), "the last koda left the server running");
}

/// A Koda ended by a signal (a closed terminal, a `kill`) runs no
/// destructors; it still stops the server it started.
#[test]
fn oss_stops_the_server_when_koda_is_signalled() {
    let Some(setup) = setup() else { return };
    for signal in ["-TERM", "-HUP"] {
        let mut child = spawn(&setup, "30");
        // Wait for the server, then for the turn to be under way.
        let deadline = Instant::now() + Duration::from_secs(30);
        let pid = loop {
            if let [.., pid] = starts(&setup)[..]
                && alive(pid)
            {
                break pid;
            }
            assert!(Instant::now() < deadline, "the server never started");
            std::thread::sleep(Duration::from_millis(100));
        };
        std::thread::sleep(Duration::from_secs(2));
        Command::new("kill")
            .args([signal, &child.id().to_string()])
            .status()
            .unwrap();
        child.wait().unwrap();
        assert!(wait_gone(pid), "{signal}: the server outlived koda");
    }
}
