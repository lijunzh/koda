//! `koda serve` as a process: a client on its stdin and stdout (`--stdio`)
//! or on its socket runs a turn against a scripted model, and the process
//! ends when stdin closes or a signal stops it. The transport's scenarios are in
//! `koda-client/tests/serve.rs`.

use std::process::Stdio;
use std::time::Duration;

use koda_test_utils::network::{FakeLlmServer, ResponseTemplate};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};

const WAIT: Duration = Duration::from_secs(30);

fn koda_bin() -> String {
    env!("CARGO_BIN_EXE_koda").to_string()
}

fn reply(text: &str) -> ResponseTemplate {
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices": [{"delta": {"content": text}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 20, "completion_tokens": 5}}),
    );
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

/// `koda serve --stdio` on a scratch config folder, which `root` holds.
struct Served {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Lines<BufReader<ChildStdout>>,
}

impl Served {
    fn spawn(root: &std::path::Path) -> Self {
        let mut child = tokio::process::Command::new(koda_bin())
            .args(["serve", "--stdio", "--project-root"])
            .arg(root)
            .env("XDG_CONFIG_HOME", root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        Self {
            child,
            stdin,
            stdout,
        }
    }

    async fn send(&mut self, id: i64, method: &str, params: Value) {
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let stdin = self.stdin.as_mut().unwrap();
        stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
        stdin.flush().await.unwrap();
    }

    /// Lines up to and including the first `pick` matches.
    async fn until(&mut self, pick: impl Fn(&Value) -> bool) -> Vec<Value> {
        let mut seen = Vec::new();
        loop {
            let line = tokio::time::timeout(WAIT, self.stdout.next_line())
                .await
                .expect("koda serve wrote nothing in time")
                .unwrap()
                .unwrap_or_else(|| panic!("stdout ended; saw {seen:?}"));
            let line: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(line["jsonrpc"], "2.0", "{line}");
            let done = pick(&line);
            seen.push(line);
            if done {
                return seen;
            }
        }
    }

    async fn result(&mut self, id: i64) -> Value {
        let lines = self
            .until(|l| l["id"] == id && l.get("method").is_none())
            .await;
        let answer = lines.last().unwrap().clone();
        assert!(answer.get("error").is_none(), "{answer}");
        answer["result"].clone()
    }

    async fn exits_in_time(mut self) -> std::process::ExitStatus {
        tokio::time::timeout(Duration::from_secs(15), self.child.wait())
            .await
            .expect("koda serve didn't exit")
            .unwrap()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_runs_a_turn_and_eof_ends_the_process() {
    let server = FakeLlmServer::spawn().await;
    server
        .mount_fn("POST", r".*/chat/completions$", |_| reply("Served."))
        .await;
    let root = tempfile::tempdir().unwrap();
    let mut koda = Served::spawn(root.path());

    koda.send(
        1,
        "initialize",
        json!({"clientInfo": {"name": "serve-test", "version": "0"}}),
    )
    .await;
    koda.send(
        2,
        "session/start",
        json!({"modelProvider": "lm-studio", "baseUrl": server.url(), "model": "stub"}),
    )
    .await;
    let init = koda.result(1).await;
    assert_eq!(init["serverInfo"]["name"], "koda");
    let session = koda.result(2).await["session"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    koda.send(
        3,
        "turn/start",
        json!({"sessionId": session, "input": "Say something"}),
    )
    .await;
    let lines = koda.until(|l| l["method"] == "turn/completed").await;
    let turn = &lines.last().unwrap()["params"]["turn"];
    assert_eq!(turn["status"], "completed", "{turn}");
    assert!(
        lines
            .iter()
            .any(|l| l["method"] == "item/completed" && l["params"]["item"]["text"] == "Served."),
        "{lines:?}"
    );

    drop(koda.stdin.take());
    let status = koda.exits_in_time().await;
    assert!(status.success(), "{status:?}");
    // Its state is in the scratch folder.
    assert!(root.path().join("koda").exists());
}

/// A client may keep stdin open; SIGTERM still stops the process.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn sigterm_ends_the_process_with_stdin_open() {
    let root = tempfile::tempdir().unwrap();
    let mut koda = Served::spawn(root.path());
    koda.send(
        1,
        "initialize",
        json!({"clientInfo": {"name": "serve-test", "version": "0"}}),
    )
    .await;
    koda.result(1).await;

    let pid = koda.child.id().unwrap().to_string();
    let sent = std::process::Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .unwrap();
    assert!(sent.success());
    let status = koda.exits_in_time().await;
    assert!(status.success(), "{status:?}");
}

/// `koda serve` alone is the shared server on Koda's socket: a client
/// connects, runs a turn, and SIGTERM stops the server and removes the
/// socket. `--listen unix://PATH` puts it elsewhere.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn serve_listens_on_a_socket_until_sigterm() {
    use tokio::net::UnixStream;
    let server = FakeLlmServer::spawn().await;
    server
        .mount_fn("POST", r".*/chat/completions$", |_| {
            reply("Over the socket.")
        })
        .await;
    let root = tempfile::tempdir().unwrap();
    let spawn = |extra: &[&str]| {
        tokio::process::Command::new(koda_bin())
            .arg("serve")
            .args(extra)
            .arg("--project-root")
            .arg(root.path())
            .env("XDG_CONFIG_HOME", root.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    };
    let wait_for = |socket: std::path::PathBuf| async move {
        let deadline = std::time::Instant::now() + WAIT;
        loop {
            if let Ok(stream) = UnixStream::connect(&socket).await {
                return stream;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no socket at {socket:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    let stop = |child: &tokio::process::Child| {
        let pid = child.id().unwrap().to_string();
        assert!(
            std::process::Command::new("kill")
                .args(["-TERM", &pid])
                .status()
                .unwrap()
                .success()
        );
    };

    let socket = root.path().join("koda/serve/koda.sock");
    let mut koda = spawn(&[]);
    let stream = wait_for(socket.clone()).await;
    let (output, mut input) = stream.into_split();
    let mut lines = BufReader::new(output).lines();
    for (id, method, params) in [
        (
            1,
            "initialize",
            json!({"clientInfo": {"name": "socket", "version": "0"}}),
        ),
        (
            2,
            "session/start",
            json!({"modelProvider": "lm-studio", "baseUrl": server.url(), "model": "stub"}),
        ),
    ] {
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        input
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
    }
    async fn read(
        lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    ) -> Value {
        let line = tokio::time::timeout(WAIT, lines.next_line())
            .await
            .expect("koda serve wrote nothing in time")
            .unwrap()
            .expect("the socket closed");
        serde_json::from_str::<Value>(&line).unwrap()
    }
    let session = loop {
        let line = read(&mut lines).await;
        if line["id"] == 2 {
            break line["result"]["session"]["id"]
                .as_str()
                .unwrap()
                .to_string();
        }
    };
    let line = json!({"jsonrpc": "2.0", "id": 3, "method": "turn/start",
        "params": {"sessionId": session, "input": "Say something"}});
    input
        .write_all(format!("{line}\n").as_bytes())
        .await
        .unwrap();
    let completed = loop {
        let line = read(&mut lines).await;
        if line["method"] == "turn/completed" {
            break line;
        }
    };
    assert_eq!(completed["params"]["turn"]["status"], "completed");

    stop(&koda);
    let status = tokio::time::timeout(Duration::from_secs(15), koda.wait())
        .await
        .expect("koda serve didn't exit")
        .unwrap();
    assert!(status.success(), "{status:?}");
    assert!(!socket.exists(), "the socket is removed");

    // Elsewhere, with --listen.
    let other = root.path().join("elsewhere/k.sock");
    let url = format!("unix://{}", other.display());
    let mut koda = spawn(&["--listen", &url]);
    drop(wait_for(other.clone()).await);
    stop(&koda);
    let status = tokio::time::timeout(Duration::from_secs(15), koda.wait())
        .await
        .expect("koda serve didn't exit")
        .unwrap();
    assert!(status.success(), "{status:?}");
}

#[test]
fn serve_refuses_an_unknown_listen_and_a_sessions_flags() {
    let root = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        std::process::Command::new(koda_bin())
            .args(args)
            .env("XDG_CONFIG_HOME", root.path())
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    let out = run(&["serve", "--listen", "tcp://127.0.0.1:9"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unix://"));
    for flag in [
        &["--model", "x"][..],
        &["--provider", "lm-studio"],
        &["--base-url", "http://127.0.0.1:9/v1"],
        &["--resume", "abc"],
    ] {
        // After `serve`, it isn't one of serve's flags.
        let mut args = vec!["serve", "--stdio"];
        args.extend_from_slice(flag);
        let out = run(&args);
        assert_eq!(out.status.code(), Some(2), "{flag:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(flag[0]), "{flag:?}: {stderr}");
        // Before it, Koda says where it goes instead.
        let mut args = flag.to_vec();
        args.extend_from_slice(&["serve", "--stdio"]);
        let out = run(&args);
        assert_eq!(out.status.code(), Some(2), "{flag:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("session/start"), "{flag:?}: {stderr}");
    }
    // Nothing was created for a refused start.
    assert!(!root.path().join("koda").exists());
}
