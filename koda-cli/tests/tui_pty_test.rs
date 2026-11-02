//! The real TUI on a real pseudo-terminal, for what only shows up end to end.
//!
//! `TuiContext` can't be built without a terminal, so these tests run the
//! `koda` binary on a pty and type at it the way a user does.
#![cfg(unix)]

use koda_test_utils::network::{FakeLlmServer, ResponseTemplate};
use serde_json::{Value, json};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Text the composer shows when it is empty and the TUI is idle.
const IDLE_HINT: &str = "Type a message";

/// How long the composer waits before it stops treating fast typing as a paste.
const PASTE_SETTLE: Duration = Duration::from_millis(400);

/// Nothing listens here: tests that use it never reach a model.
const NO_SERVER: &str = "http://127.0.0.1:9/v1";

/// A `koda` process on a pty. Dropping it kills the process.
struct Tui {
    child: Child,
    input: File,
    output: Arc<Mutex<Vec<u8>>>,
}

impl Tui {
    /// Start `koda` in `home` and wait for the idle composer.
    fn start(home: &Path) -> Self {
        Self::start_with_model(home, NO_SERVER)
    }

    /// Start `koda` in `home` against the model at `base_url`.
    fn start_with_model(home: &Path, base_url: &str) -> Self {
        let (master, slave) = open_pty();
        let mut command = Command::new(env!("CARGO_BIN_EXE_koda"));
        command
            .args(["--provider", "lmstudio"])
            .args(["--base-url", base_url])
            .args(["--model", "stub-model"])
            .args(["--max-context-tokens", "64000"])
            .args(["--mode", "safe"])
            .arg("--project-root")
            .arg(home)
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("TERM", "xterm-256color")
            .stdin(stdio(slave))
            .stdout(stdio(slave))
            .stderr(stdio(slave));
        // SAFETY: both calls are async-signal-safe and touch no parent state.
        unsafe {
            command.pre_exec(|| {
                // Make the pty this process's controlling terminal.
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().expect("failed to start koda");
        // SAFETY: `slave` is open, and the child holds its own copies.
        unsafe { libc::close(slave) };

        // SAFETY: `master` is open and nothing else owns it.
        let input = unsafe { File::from_raw_fd(master) };
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut reader = input.try_clone().unwrap();
        let sink = Arc::clone(&output);
        // Keeps the pty drained so the TUI never blocks on a write. It ends
        // when the child closes its side.
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            while let Ok(n @ 1..) = reader.read(&mut chunk) {
                sink.lock().unwrap().extend_from_slice(&chunk[..n]);
            }
        });

        let tui = Self {
            child,
            input,
            output,
        };
        tui.wait_for_output(IDLE_HINT);
        tui
    }

    fn wait_for_output(&self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            let output = self.output.lock().unwrap();
            if String::from_utf8_lossy(&output).contains(text) {
                return;
            }
            drop(output);
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("koda never showed {text:?}");
    }

    /// Type `text`, then give the composer time to take it as typing.
    fn type_text(&mut self, text: &str) {
        self.input.write_all(text.as_bytes()).unwrap();
        std::thread::sleep(PASTE_SETTLE);
    }

    /// Whether `koda` exits within a few seconds.
    fn exits(&mut self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.child.try_wait().unwrap().is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn open_pty() -> (RawFd, RawFd) {
    let (mut master, mut slave) = (0, 0);
    let mut size = libc::winsize {
        ws_row: 40,
        ws_col: 120,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: every pointer is valid for the call, and the name and terminal
    // settings are optional.
    let opened = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            // `*mut` on macOS and `*const` on Linux.
            &raw mut size,
        )
    };
    assert_eq!(opened, 0, "openpty: {}", std::io::Error::last_os_error());
    (master, slave)
}

fn stdio(fd: RawFd) -> Stdio {
    // SAFETY: `fd` is open, and `dup` gives this `Stdio` its own descriptor.
    unsafe { Stdio::from_raw_fd(libc::dup(fd)) }
}

/// A home directory that has been through first-run onboarding.
fn home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".config/koda")).unwrap();
    home
}

#[test]
fn exit_quits_from_the_slash_popup() {
    let home = home();
    let mut tui = Tui::start(home.path());

    // Typing a slash opens the popup, so this is how `/exit` is normally run.
    tui.type_text("/exit");
    tui.type_text("\r");

    assert!(tui.exits(), "koda stayed up after /exit");
}

#[test]
fn exit_quits_with_the_popup_closed() {
    let home = home();
    let mut tui = Tui::start(home.path());

    tui.type_text("/exit");
    tui.type_text("\x1b");
    tui.type_text("\r");

    assert!(tui.exits(), "koda stayed up after /exit");
}

/// A streamed reply that calls `List`.
fn list_call() -> ResponseTemplate {
    let chunks = [
        json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": "call_0",
            "type": "function",
            "function": {"name": "List", "arguments": "{}"},
        }]}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ];
    let mut body: String = chunks
        .iter()
        .map(|c: &Value| format!("data: {c}\n\n"))
        .collect();
    body.push_str("data: [DONE]\n\n");
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_file_keeps_its_loop_cap_across_a_model_switch() {
    let server = FakeLlmServer::spawn().await;
    server
        .mount_fn("POST", r".*/chat/completions$", |_| list_call())
        .await;
    let home = home();
    std::fs::create_dir(home.path().join("agents")).unwrap();
    let agent = json!({"name": "default", "system_prompt": "Test.", "max_iterations": 1});
    std::fs::write(home.path().join("agents/default.json"), agent.to_string()).unwrap();
    let mut tui = Tui::start_with_model(home.path(), &server.url());

    tui.type_text("/model other-model");
    tui.type_text("\r");
    tui.type_text("List the files");
    tui.type_text("\r");

    // One word at a time: a redraw skips the blank cells between words. The
    // stub repeats itself, so at the default cap the loop guard ends the turn
    // long before this line shows.
    tui.wait_for_output("iterations)");
    tui.wait_for_output("(1");
    let calls = server.received_requests().await;
    let call = calls
        .iter()
        .find(|call| call.url.path().ends_with("/chat/completions"))
        .expect("no model call");
    let sent = String::from_utf8_lossy(&call.body);
    assert!(
        sent.contains("other-model"),
        "the switch didn't land: {sent}"
    );
}
