//! The real TUI on a real pseudo-terminal, for what only shows up end to end.
//!
//! `TuiContext` can't be built without a terminal, so these tests run the
//! `koda` binary on a pty and type at it the way a user does.
#![cfg(unix)]

use koda_test_utils::network::{FakeLlmServer, Request, ResponseTemplate};
use serde_json::{Value, json};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Text the composer shows when it is empty and the TUI is idle.
const IDLE_HINT: &str = "Type a message";

/// How long the composer waits before it stops treating fast typing as a paste.
const PASTE_SETTLE: Duration = Duration::from_millis(400);

const ESC: &str = "\x1b";
const CTRL_C: &str = "\x03";

/// The engine's warning when a turn stops.
const INTERRUPTED: &str = "Interrupted";

/// What the model reads for a call the user stopped.
const STOPPED: &str = "The user stopped the turn before this ran.";

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
        self.wait_for_output_after(0, text);
    }

    /// How much output there is so far, for [`Self::wait_for_output_after`].
    fn output_len(&self) -> usize {
        self.output.lock().unwrap().len()
    }

    /// Wait for `text` to appear past the first `start` bytes of output.
    fn wait_for_output_after(&self, start: usize, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            let output = self.output.lock().unwrap();
            if String::from_utf8_lossy(&output[start..]).contains(text) {
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

    /// Type `text` and press Enter.
    fn submit(&mut self, text: &str) {
        self.type_text(text);
        self.type_text("\r");
    }

    /// Press `key` and wait for the engine to say the turn stopped.
    fn stop_turn(&mut self, key: &str) {
        let start = self.output_len();
        self.type_text(key);
        self.wait_for_output_after(start, INTERRUPTED);
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

/// A home whose agent file caps a turn at one model call.
fn home_with_a_loop_cap_of_one() -> tempfile::TempDir {
    let home = home();
    std::fs::create_dir(home.path().join("agents")).unwrap();
    let agent = json!({"name": "default", "system_prompt": "Test.", "max_iterations": 1});
    std::fs::write(home.path().join("agents/default.json"), agent.to_string()).unwrap();
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

fn sse(chunks: &[Value]) -> ResponseTemplate {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

/// A reply that makes these tool calls, each a `(name, arguments)` pair.
fn tool_calls(calls: &[(&str, Value)]) -> ResponseTemplate {
    let calls: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(index, (name, args))| {
            json!({
                "index": index,
                "id": format!("call_{index}"),
                "type": "function",
                "function": {"name": name, "arguments": args.to_string()},
            })
        })
        .collect();
    sse(&[
        json!({"choices": [{"delta": {"tool_calls": calls}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ])
}

fn write_file(name: &str) -> (&'static str, Value) {
    ("Write", json!({"file_path": name, "content": "x\n"}))
}

fn ask_user() -> ResponseTemplate {
    tool_calls(&[("AskUser", json!({"question": "Which one?"}))])
}

fn final_text() -> ResponseTemplate {
    sse(&[
        json!({"choices": [{"delta": {"content": "All done."}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
    ])
}

/// A model that sends `replies` in order, then a final reply to anything else.
async fn scripted_model(replies: Vec<ResponseTemplate>) -> FakeLlmServer {
    let server = FakeLlmServer::spawn().await;
    let next = AtomicUsize::new(0);
    server
        .mount_fn("POST", r".*/chat/completions$", move |_| {
            replies
                .get(next.fetch_add(1, Ordering::SeqCst))
                .cloned()
                .unwrap_or_else(final_text)
        })
        .await;
    server
}

async fn model_calls(server: &FakeLlmServer) -> Vec<Request> {
    server
        .received_requests()
        .await
        .into_iter()
        .filter(|request| request.url.path().ends_with("/chat/completions"))
        .collect()
}

/// Wait for model call number `count`, and return what the model was sent.
async fn wait_for_model_call(server: &FakeLlmServer, count: usize) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let calls = model_calls(server).await;
        if let Some(call) = calls.get(count - 1) {
            return String::from_utf8_lossy(&call.body).into_owned();
        }
        assert!(Instant::now() < deadline, "no model call {count}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// After a stop, no model call follows, and the next prompt runs normally.
async fn assert_stopped(tui: &mut Tui, server: &FakeLlmServer, calls_so_far: usize) -> String {
    assert_eq!(
        model_calls(server).await.len(),
        calls_so_far,
        "the turn called the model after the stop"
    );
    tui.submit("second prompt");
    let sent = wait_for_model_call(server, calls_so_far + 1).await;
    assert!(sent.contains("second prompt"), "{sent}");
    sent
}

async fn stops_at_an_approval(key: &str) {
    let server = scripted_model(vec![tool_calls(&[write_file("a.txt")])]).await;
    let home = home();
    let mut tui = Tui::start_with_model(home.path(), &server.url());

    tui.submit("first prompt");
    tui.wait_for_output("approve");
    tui.stop_turn(key);

    assert!(!home.path().join("a.txt").exists(), "the tool ran");
    let sent = assert_stopped(&mut tui, &server, 1).await;
    assert!(sent.contains("a.txt"), "the model lost its call: {sent}");
    assert!(sent.contains(STOPPED), "no word of the stop: {sent}");
}

#[tokio::test(flavor = "multi_thread")]
async fn esc_stops_the_turn_at_an_approval() {
    stops_at_an_approval(ESC).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ctrl_c_stops_the_turn_at_an_approval() {
    stops_at_an_approval(CTRL_C).await;
}

async fn stops_at_a_question(key: &str) {
    let server = scripted_model(vec![ask_user()]).await;
    let home = home();
    let mut tui = Tui::start_with_model(home.path(), &server.url());

    tui.submit("first prompt");
    tui.wait_for_output("Which one?");
    tui.type_text("half an answer");
    tui.stop_turn(key);

    let sent = assert_stopped(&mut tui, &server, 1).await;
    assert!(sent.contains(STOPPED), "no word of the stop: {sent}");
    assert!(!sent.contains("half an answer"), "{sent}");
}

#[tokio::test(flavor = "multi_thread")]
async fn esc_stops_the_turn_at_a_question() {
    stops_at_a_question(ESC).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ctrl_c_stops_the_turn_at_a_question() {
    stops_at_a_question(CTRL_C).await;
}

async fn stops_while_typing_feedback(key: &str) {
    let server = scripted_model(vec![tool_calls(&[write_file("a.txt")])]).await;
    let home = home();
    let mut tui = Tui::start_with_model(home.path(), &server.url());

    tui.submit("first prompt");
    tui.wait_for_output("approve");
    tui.type_text("f");
    tui.type_text("half a reason");
    tui.stop_turn(key);

    let sent = assert_stopped(&mut tui, &server, 1).await;
    assert!(sent.contains(STOPPED), "no word of the stop: {sent}");
    assert!(!sent.contains("half a reason"), "{sent}");
}

#[tokio::test(flavor = "multi_thread")]
async fn esc_stops_the_turn_while_typing_feedback() {
    stops_while_typing_feedback(ESC).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ctrl_c_stops_the_turn_while_typing_feedback() {
    stops_while_typing_feedback(CTRL_C).await;
}

async fn stops_at_the_loop_cap(key: &str) {
    let server = scripted_model(vec![tool_calls(&[("List", json!({}))])]).await;
    let home = home_with_a_loop_cap_of_one();
    let mut tui = Tui::start_with_model(home.path(), &server.url());

    tui.submit("first prompt");
    tui.wait_for_output("Hard cap reached. Continue?");
    tui.stop_turn(key);

    assert_stopped(&mut tui, &server, 1).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_file_keeps_its_loop_cap_across_a_model_switch() {
    let server = scripted_model(vec![tool_calls(&[("List", json!({}))])]).await;
    let home = home_with_a_loop_cap_of_one();
    let mut tui = Tui::start_with_model(home.path(), &server.url());

    tui.submit("/model other-model");
    tui.submit("first prompt");

    // One word at a time: a redraw skips the blank cells between words. At the
    // default cap the turn ends on the stub's final reply, with no dialog.
    tui.wait_for_output("iterations)");
    tui.wait_for_output("(1");
    let sent = wait_for_model_call(&server, 1).await;
    assert!(
        sent.contains("other-model"),
        "the switch didn't land: {sent}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn esc_stops_the_turn_at_the_loop_cap() {
    stops_at_the_loop_cap(ESC).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ctrl_c_stops_the_turn_at_the_loop_cap() {
    stops_at_the_loop_cap(CTRL_C).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn n_rejects_and_lets_the_model_go_on() {
    let server = scripted_model(vec![tool_calls(&[write_file("a.txt")])]).await;
    let home = home();
    let mut tui = Tui::start_with_model(home.path(), &server.url());

    tui.submit("first prompt");
    tui.wait_for_output("approve");
    tui.type_text("n");

    let sent = wait_for_model_call(&server, 2).await;
    assert!(sent.contains("User rejected this action."), "{sent}");
}

/// Stopping at the second call of a batch keeps what the first one did.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_keeps_the_calls_that_already_ran() {
    let batch = tool_calls(&[write_file("a.txt"), write_file("b.txt")]);
    let server = scripted_model(vec![batch]).await;
    let home = home();
    let mut tui = Tui::start_with_model(home.path(), &server.url());

    tui.submit("first prompt");
    tui.wait_for_output("a.txt");
    tui.wait_for_output("approve");
    let start = tui.output_len();
    tui.type_text("y");
    tui.wait_for_output_after(start, "b.txt");
    tui.stop_turn(ESC);

    assert!(home.path().join("a.txt").exists());
    assert!(!home.path().join("b.txt").exists());
    let sent = assert_stopped(&mut tui, &server, 1).await;
    assert!(
        sent.contains("call_0"),
        "the model lost the first call: {sent}"
    );
    assert!(sent.contains(STOPPED), "no word of the stop: {sent}");
}

/// Text typed before the stop comes back to the composer instead of being lost.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_hands_steering_text_back() {
    let reply = tool_calls(&[write_file("a.txt")]).set_delay(Duration::from_secs(3));
    let server = scripted_model(vec![reply]).await;
    let home = home();
    let mut tui = Tui::start_with_model(home.path(), &server.url());

    tui.submit("first prompt");
    wait_for_model_call(&server, 1).await;
    tui.submit("steering text");
    tui.wait_for_output("approve");
    tui.stop_turn(CTRL_C);

    assert_eq!(model_calls(&server).await.len(), 1);
    tui.type_text("\r");
    let sent = wait_for_model_call(&server, 2).await;
    assert!(sent.contains("steering text"), "{sent}");
}
