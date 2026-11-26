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

/// `koda serve --stop` stops the shared server, as `claude daemon stop`
/// does, and says so when none is running; one started with an idle exit,
/// as the TUI starts it, stops on its own.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn serve_stop_and_the_idle_exit() {
    let root = tempfile::tempdir().unwrap();
    let koda = |args: &[&str]| {
        let mut command = tokio::process::Command::new(koda_bin());
        command
            .arg("serve")
            .args(args)
            .arg("--project-root")
            .arg(root.path())
            .env("XDG_CONFIG_HOME", root.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    };
    let socket = root.path().join("koda/serve/koda.sock");
    let wait_for_socket = || async {
        let deadline = std::time::Instant::now() + WAIT;
        while tokio::net::UnixStream::connect(&socket).await.is_err() {
            assert!(std::time::Instant::now() < deadline, "no socket");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };

    let mut server = koda(&[]).spawn().unwrap();
    wait_for_socket().await;
    let stopped = koda(&["--stop"]).output().await.unwrap();
    let said = String::from_utf8_lossy(&stopped.stderr);
    assert!(stopped.status.success(), "{said}");
    assert!(said.contains("Stopped koda serve"), "{said}");
    let status = tokio::time::timeout(Duration::from_secs(15), server.wait())
        .await
        .expect("the server didn't exit")
        .unwrap();
    assert!(status.success(), "{status:?}");
    assert!(!socket.exists());

    let none = koda(&["--stop"]).output().await.unwrap();
    assert_eq!(none.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&none.stderr).contains("no koda serve is listening"));

    let mut idle = koda(&["--idle-exit-secs", "1"]).spawn().unwrap();
    wait_for_socket().await;
    let status = tokio::time::timeout(Duration::from_secs(15), idle.wait())
        .await
        .expect("an idle server didn't exit")
        .unwrap();
    assert!(status.success(), "{status:?}");
    assert!(!socket.exists());
}

/// `--no-daemon` and `--remote` are the TUI's: `koda exec` keeps its host
/// in its own process and says so, and `--remote` takes `unix://PATH`.
#[test]
fn the_tuis_server_flags_are_refused_where_they_dont_apply() {
    let root = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        std::process::Command::new(koda_bin())
            .args(args)
            .env("XDG_CONFIG_HOME", root.path())
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    for args in [
        &["exec", "--no-daemon", "hi"][..],
        &["exec", "--remote", "unix:///tmp/k.sock", "hi"],
    ] {
        let out = run(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("for the TUI"),
            "{args:?}"
        );
    }
    let out = run(&["--remote", "tcp://127.0.0.1:9"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unix://PATH"));
}

/// `koda serve` killed in the middle of a turn (a crash, `kill -9`, the Mac
/// losing power): the next server opens the session from what was stored,
/// says how the cut-off turn ended, and runs the next turn.
#[tokio::test(flavor = "multi_thread")]
async fn a_server_killed_mid_turn_leaves_the_session_to_go_on() {
    use tokio::net::UnixStream;
    let server = FakeLlmServer::spawn().await;
    // The first turn's call never answers in time; any other (the second
    // turn's, a capability probe) answers at once.
    let first_turn = |body: &[u8]| {
        let body = String::from_utf8_lossy(body);
        body.contains("First question") && !body.contains("Second question")
    };
    server
        .mount_fn("POST", r".*/chat/completions$", move |request| {
            if first_turn(&request.body) {
                reply("Too late.").set_delay(Duration::from_secs(300))
            } else {
                reply("Back again.")
            }
        })
        .await;
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("koda/serve/koda.sock");
    let spawn = || {
        tokio::process::Command::new(koda_bin())
            .arg("serve")
            .arg("--project-root")
            .arg(root.path())
            .env("XDG_CONFIG_HOME", root.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    };
    struct Conn {
        lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
        input: tokio::net::unix::OwnedWriteHalf,
        next: i64,
        /// Lines read while waiting for a response, for `until`: the server
        /// can send a turn's `turn/started` before its `turn/start` response.
        backlog: std::collections::VecDeque<Value>,
    }
    impl Conn {
        async fn open(socket: &std::path::Path) -> Self {
            let deadline = std::time::Instant::now() + WAIT;
            let stream = loop {
                if let Ok(stream) = UnixStream::connect(socket).await {
                    break stream;
                }
                assert!(std::time::Instant::now() < deadline, "no socket");
                tokio::time::sleep(Duration::from_millis(50)).await;
            };
            let (output, input) = stream.into_split();
            let mut conn = Self {
                lines: BufReader::new(output).lines(),
                input,
                next: 1,
                backlog: std::collections::VecDeque::new(),
            };
            conn.request(
                "initialize",
                json!({"clientInfo": {"name": "kill-test", "version": "0"}}),
            )
            .await;
            conn
        }
        async fn send(&mut self, method: &str, params: Value) -> i64 {
            let id = self.next;
            self.next += 1;
            let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
            self.input
                .write_all(format!("{line}\n").as_bytes())
                .await
                .unwrap();
            id
        }
        async fn read(&mut self) -> Value {
            let line = tokio::time::timeout(WAIT, self.lines.next_line())
                .await
                .expect("koda serve wrote nothing in time")
                .unwrap()
                .expect("the socket closed");
            serde_json::from_str(&line).unwrap()
        }
        async fn request(&mut self, method: &str, params: Value) -> Value {
            let id = self.send(method, params).await;
            loop {
                let line = self.read().await;
                if line["id"] == id && line.get("method").is_none() {
                    assert!(line.get("error").is_none(), "{method}: {line}");
                    return line["result"].clone();
                }
                self.backlog.push_back(line);
            }
        }
        async fn until(&mut self, method: &str) -> Value {
            while let Some(line) = self.backlog.pop_front() {
                if line["method"] == method {
                    return line;
                }
            }
            loop {
                let line = self.read().await;
                if line["method"] == method {
                    return line;
                }
            }
        }
    }

    let mut koda = spawn();
    let mut conn = Conn::open(&socket).await;
    let started = conn
        .request(
            "session/start",
            json!({"modelProvider": "lm-studio", "baseUrl": server.url(), "model": "stub"}),
        )
        .await;
    let session = started["session"]["id"].as_str().unwrap().to_string();
    conn.request(
        "turn/start",
        json!({"sessionId": session, "input": "First question"}),
    )
    .await;
    conn.until("turn/started").await;
    // Wait for the first turn's model call to be under way, then kill the
    // server.
    let deadline = std::time::Instant::now() + WAIT;
    while !server
        .received_requests()
        .await
        .iter()
        .any(|r| r.url.path().ends_with("/chat/completions") && first_turn(&r.body))
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the model was never called"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    koda.start_kill().unwrap();
    koda.wait().await.unwrap();
    drop(conn);

    // A new server takes over the socket the dead one left behind.
    let _koda = spawn();
    let mut conn = Conn::open(&socket).await;
    let opened = conn
        .request("session/resume", json!({"sessionId": session}))
        .await;
    assert_eq!(opened["session"]["id"], session.as_str(), "{opened}");
    conn.request(
        "turn/start",
        json!({"sessionId": session, "input": "Second question"}),
    )
    .await;
    let completed = conn.until("turn/completed").await;
    assert_eq!(
        completed["params"]["turn"]["status"], "completed",
        "{completed}"
    );
    // The cut-off turn reads as interrupted, as Codex marks a turn its
    // server didn't finish; its question stays in the conversation.
    let turns = conn
        .request("session/turns/list", json!({"sessionId": session}))
        .await;
    let statuses: Vec<&str> = turns["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|turn| turn["status"].as_str().unwrap())
        .collect();
    assert_eq!(statuses, ["completed", "interrupted"], "{turns}");
    let sent = server.chat_messages().await;
    let last = Value::from(sent.last().unwrap().clone()).to_string();
    assert!(
        last.contains("First question") && last.contains("Second question"),
        "{last}"
    );
}
