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
const CTRL_O: &str = "\x0f";

/// The pty's size, which the screen model must match.
const ROWS: u16 = 40;
const COLS: u16 = 120;

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
        Self::start_with_args(home, base_url, &[], IDLE_HINT)
    }

    /// [`Self::start_with_model`] with `extra` arguments, waiting until the
    /// output shows `ready`.
    fn start_with_args(home: &Path, base_url: &str, extra: &[&str], ready: &str) -> Self {
        let (master, slave) = open_pty();
        let mut command = Command::new(env!("CARGO_BIN_EXE_koda"));
        command
            .args(["--provider", "lmstudio"])
            .args(["--base-url", base_url])
            .args(["--model", "stub-model"])
            .args(["--max-context-tokens", "64000"])
            .args(["--mode", "safe"])
            .args(extra)
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
        tui.wait_for_output(ready);
        tui
    }

    /// Everything the TUI has written, escape codes stripped.
    fn output_text(&self) -> String {
        let bytes = self.output.lock().unwrap().clone();
        let text = String::from_utf8_lossy(&bytes);
        // The resume hint is printed after the TUI leaves its screen.
        let mut plain = String::new();
        let mut in_escape = false;
        for c in text.chars() {
            match (in_escape, c) {
                (false, '\x1b') => in_escape = true,
                (true, c) if c.is_ascii_alphabetic() || c == '~' => in_escape = false,
                (true, _) => {}
                (false, c) => plain.push(c),
            }
        }
        plain
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

    /// What the terminal shows now. ratatui redraws only the cells that
    /// changed, so whether something is on screen, or gone from it, can only
    /// be read from a terminal model fed the whole output.
    fn screen(&self) -> String {
        let size = |n| std::num::NonZero::new(n).unwrap();
        let mut parser = vt100::Parser::new(size(ROWS), size(COLS), 0);
        parser.process(&self.output.lock().unwrap());
        parser.screen().contents()
    }

    /// Wait until the screen shows `text`.
    fn wait_for_screen(&self, text: &str) {
        self.wait_for_screen_that(|screen| screen.contains(text), &format!("show {text:?}"));
    }

    /// Wait until the screen no longer shows `text`.
    fn wait_for_screen_without(&self, text: &str) {
        self.wait_for_screen_that(|screen| !screen.contains(text), &format!("drop {text:?}"));
    }

    fn wait_for_screen_that(&self, ready: impl Fn(&str) -> bool, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let screen = self.screen();
            if ready(&screen) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the screen didn't {what}:\n{screen}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Send `text` in one write, as a terminal delivers a paste.
    fn paste(&mut self, text: &str) {
        self.input.write_all(text.as_bytes()).unwrap();
        std::thread::sleep(PASTE_SETTLE);
    }

    /// Answer every approval the TUI asks for with `y` until `done` holds.
    fn approve_until(&mut self, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !done() {
            assert!(Instant::now() < deadline, "timed out:\n{}", self.screen());
            if self.screen().contains("[y] approve") {
                self.type_text("y");
            } else {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
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
        ws_row: ROWS,
        ws_col: COLS,
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

/// Whether a process whose command line matches `pattern` is running.
fn running(pattern: &str) -> bool {
    Command::new("pgrep")
        .args(["-f", pattern])
        .output()
        .is_ok_and(|o| !o.stdout.is_empty())
}

/// Esc stops a running command at once. It used to wait out the engine's
/// 5-second grace and report "Turn failed".
#[tokio::test(flavor = "multi_thread")]
async fn esc_stops_a_running_command_at_once() {
    // 31 s: no other test sleeps that long, so pgrep finds only this one.
    let server = scripted_model(vec![tool_calls(&[(
        "Bash",
        json!({"command": "sleep 31"}),
    )])])
    .await;
    let home = home();
    let mut tui = Tui::start_with_model(home.path(), &server.url());

    tui.submit("run it");
    tui.approve_until(|| running("sleep 31"));
    let pressed = Instant::now();
    tui.stop_turn(ESC);

    assert!(
        pressed.elapsed() < Duration::from_secs(3),
        "took {:?}",
        pressed.elapsed()
    );
    assert!(
        !tui.screen().contains("did not stop within"),
        "{}",
        tui.screen()
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while running("sleep 31") {
        assert!(Instant::now() < deadline, "the command outlived the stop");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A read shows as one summary line; Ctrl+O shows its content and folds it
/// again. Before, a read filled the screen in the reply's colour.
#[tokio::test(flavor = "multi_thread")]
async fn ctrl_o_unfolds_a_read_and_folds_it_again() {
    let server = scripted_model(vec![tool_calls(&[(
        "Read",
        json!({"file_path": "notes.md"}),
    )])])
    .await;
    let home = home();
    std::fs::write(
        home.path().join("notes.md"),
        "alpha line\nbeta line\ngamma line\n",
    )
    .unwrap();
    let mut tui = Tui::start_with_model(home.path(), &server.url());

    tui.submit("read the notes");
    tui.wait_for_screen("All done.");
    tui.wait_for_screen("3 lines");
    assert!(!tui.screen().contains("beta line"), "{}", tui.screen());

    tui.type_text(CTRL_O);
    tui.wait_for_screen("beta line");
    // The full view lines the number up apart from the text.
    assert!(tui.screen().contains("2  beta line"), "{}", tui.screen());
    assert!(!tui.screen().contains("3 lines"), "{}", tui.screen());

    tui.type_text(CTRL_O);
    tui.wait_for_screen_without("beta line");
    tui.wait_for_screen("3 lines");
}

/// A pasted `?` reaches the model. Mid-paste it used to open the shortcuts
/// overlay, which then swallowed the next key.
#[tokio::test(flavor = "multi_thread")]
async fn a_pasted_question_mark_reaches_the_model() {
    let server = scripted_model(vec![]).await;
    let home = home();
    let mut tui = Tui::start_with_model(home.path(), &server.url());

    tui.paste("is it ok? yes, and why? because");
    tui.type_text("\r");

    let sent = wait_for_model_call(&server, 1).await;
    assert!(sent.contains("is it ok? yes, and why? because"), "{sent}");
}

/// Koda stops a background process it started. The sandbox refused the
/// model's `kill`, so stopping one needed a run outside the sandbox.
#[tokio::test(flavor = "multi_thread")]
async fn koda_stops_the_background_process_it_started() {
    let server = FakeLlmServer::spawn().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let next = Arc::clone(&calls);
    server
        .mount_fn("POST", r".*/chat/completions$", move |request| {
            match next.fetch_add(1, Ordering::SeqCst) {
                // 32 s: no other test sleeps that long.
                0 => tool_calls(&[("Bash", json!({"command": "sleep 32", "background": true}))]),
                1 => {
                    let body = String::from_utf8_lossy(&request.body);
                    let pid = body
                        .split("PID:")
                        .nth(1)
                        .and_then(|rest| {
                            rest.split(|c: char| !c.is_ascii_digit() && c != ' ').next()
                        })
                        .map(str::trim)
                        .unwrap_or("0")
                        .to_string();
                    tool_calls(&[("Bash", json!({"command": format!("kill {pid}")}))])
                }
                _ => final_text(),
            }
        })
        .await;
    let home = home();
    let mut tui = Tui::start_with_model(home.path(), &server.url());

    tui.submit("start the server, then stop it");
    // Start, stop, then the final reply.
    tui.approve_until(|| calls.load(Ordering::SeqCst) >= 3);
    let sent = wait_for_model_call(&server, 3).await;
    assert!(sent.contains("Stopped background process"), "{sent}");
    let deadline = Instant::now() + Duration::from_secs(3);
    while running("sleep 32") {
        assert!(
            Instant::now() < deadline,
            "the background process is still running"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A request to run outside the sandbox says so, with the model's reason,
/// and `n` refuses it.
#[tokio::test(flavor = "multi_thread")]
async fn an_outside_sandbox_request_shows_its_warning_and_reason() {
    let server = scripted_model(vec![tool_calls(&[(
        "Bash",
        json!({"command": "echo hi", "outside_sandbox": true, "reason": "Testing the prompt"}),
    )])])
    .await;
    let home = home();
    let mut tui = Tui::start_with_model(home.path(), &server.url());

    tui.submit("go");
    tui.wait_for_screen("[y] approve");
    let screen = tui.screen();
    assert!(screen.contains("outside the sandbox"), "{screen}");
    assert!(screen.contains("why: Testing the prompt"), "{screen}");
    tui.type_text("n");

    let sent = wait_for_model_call(&server, 2).await;
    assert!(sent.contains("User rejected this action."), "{sent}");
}

/// Quit with Ctrl+D and return the session id Koda prints for `--resume`.
fn quit_and_read_session_id(tui: &mut Tui) -> String {
    tui.type_text("\x04");
    assert!(tui.exits(), "koda stayed up after Ctrl+D");
    let text = tui.output_text();
    text.split("koda --resume ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .map(str::to_string)
        .unwrap_or_else(|| panic!("no resume hint in:\n{text}"))
}

/// A resumed session shows the earlier conversation, folded as it was live,
/// and the model gets that history with the next prompt.
#[tokio::test(flavor = "multi_thread")]
async fn a_resumed_session_shows_and_keeps_its_history() {
    let server = scripted_model(vec![tool_calls(&[(
        "Read",
        json!({"file_path": "notes.md"}),
    )])])
    .await;
    let home = home();
    std::fs::write(
        home.path().join("notes.md"),
        "alpha line\nbeta line\ngamma line\n",
    )
    .unwrap();
    let mut tui = Tui::start_with_model(home.path(), &server.url());
    tui.submit("read the notes");
    tui.wait_for_screen("All done.");
    let session = quit_and_read_session_id(&mut tui);

    let mut tui = Tui::start_with_args(
        home.path(),
        &server.url(),
        &["--resume", &session],
        IDLE_HINT,
    );
    tui.wait_for_screen("session resumed");
    let screen = tui.screen();
    assert!(screen.contains("read the notes"), "{screen}");
    assert!(
        screen.contains("3 lines"),
        "the read isn't folded:\n{screen}"
    );
    assert!(!screen.contains("beta line"), "{screen}");
    tui.type_text(CTRL_O);
    tui.wait_for_screen("beta line");
    tui.type_text(CTRL_O);
    tui.wait_for_screen_without("beta line");

    tui.submit("and now?");
    let sent = wait_for_model_call(&server, 3).await;
    assert!(
        sent.contains("read the notes"),
        "the model lost the history: {sent}"
    );
    assert!(
        sent.contains("beta line"),
        "the model lost the read: {sent}"
    );
    assert!(sent.contains("and now?"), "{sent}");
}

/// `/model` switches the model mid-session: the next request asks for it.
#[tokio::test(flavor = "multi_thread")]
async fn model_switches_the_model_for_the_next_turn() {
    let server = scripted_model(vec![]).await;
    let home = home();
    let mut tui = Tui::start_with_model(home.path(), &server.url());

    tui.submit("first");
    let sent = wait_for_model_call(&server, 1).await;
    assert!(sent.contains(r#""model":"stub-model""#), "{sent}");
    tui.wait_for_screen("All done.");

    // Esc would close the slash popup but also clear the line, so Enter goes
    // straight to the typed command.
    tui.type_text("/model stub-two");
    tui.type_text("\r");
    tui.wait_for_screen("stub-two");

    tui.submit("second");
    let sent = wait_for_model_call(&server, 2).await;
    assert!(sent.contains(r#""model":"stub-two""#), "{sent}");
    assert!(sent.contains("second"), "{sent}");
}

/// The first run greets the user and opens the provider picker, and a prompt
/// still gets its answer once the picker is closed. The next start isn't a
/// first run.
#[tokio::test(flavor = "multi_thread")]
async fn the_first_run_greets_and_still_answers() {
    let server = scripted_model(vec![]).await;
    // No `.config/koda`: Koda has never run here.
    let home = tempfile::tempdir().unwrap();
    let mut tui = Tui::start_with_args(home.path(), &server.url(), &[], "Welcome to Koda");
    tui.wait_for_screen("Choose your LLM provider");
    tui.type_text(ESC);
    tui.wait_for_screen_without("Choose your LLM provider");

    tui.submit("hello");
    let sent = wait_for_model_call(&server, 1).await;
    assert!(sent.contains("hello"), "{sent}");
    tui.wait_for_screen("All done.");
    let _ = quit_and_read_session_id(&mut tui);

    let tui = Tui::start_with_model(home.path(), &server.url());
    assert!(
        !tui.screen().contains("Welcome to Koda"),
        "{}",
        tui.screen()
    );
}

/// A key typed into a wizard is taken with one Enter. The wizard's trail
/// box took the first Enter and dropped the key, so it needed two.
#[test]
fn a_wizard_takes_its_answer_with_one_enter() {
    let home = home();
    let mut tui = Tui::start(home.path());

    // `/key` asks for a key without switching provider, so nothing calls an API.
    tui.type_text("/key");
    tui.type_text("\r");
    tui.wait_for_screen("API key");
    tui.type_text("\r");
    tui.wait_for_screen("API key for");
    tui.wait_for_screen("Paste or type it");
    tui.paste("sk-test-not-a-real-key");
    tui.type_text("\r");
    tui.wait_for_screen("set to sk-t");
}

/// The provider picker lists, and switches to, the address Koda is running
/// with. It used the provider's default port, not the one given.
#[tokio::test(flavor = "multi_thread")]
async fn the_provider_picker_uses_the_running_address() {
    let server = scripted_model(vec![]).await;
    let models = json!({"object": "list", "data": [
        {"id": "stub-picked", "object": "model", "type": "llm", "state": "loaded"}
    ]});
    server.mount_ok("GET", r".*/models$", models).await;
    let home = home();
    let mut tui = Tui::start_with_model(home.path(), &server.url());

    tui.type_text("/provider");
    tui.type_text("\r");
    tui.wait_for_screen("Select a provider");
    // LM Studio, the provider in use, comes first.
    tui.type_text("\r");
    tui.wait_for_screen("stub-picked");
    tui.type_text("\r");
    tui.wait_for_screen("Model set to: stub-picked");

    tui.submit("hi");
    let sent = wait_for_model_call(&server, 1).await;
    assert!(sent.contains(r#""model":"stub-picked""#), "{sent}");
}

/// A message that can't be saved isn't sent. The turn reads the conversation
/// back from the database, so an unsaved message went out as a request with
/// no messages, which the API rejected with an error that read like a key
/// problem. Deleting the session under the running TUI makes the save fail
/// the way a mistyped `--resume` id once did.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_that_cant_be_saved_isnt_sent() {
    use koda_core::persistence::Persistence;
    let server = scripted_model(vec![]).await;
    let home = home();
    // Koda stores the project root canonicalized (/var is /private/var on macOS).
    let tui_project = std::fs::canonicalize(home.path()).unwrap();
    let mut tui = Tui::start_with_model(home.path(), &server.url());

    let db = koda_core::db::Database::open(&home.path().join(".config/koda/db/koda.db"))
        .await
        .unwrap();
    let sessions = db.list_sessions(10, &tui_project).await.unwrap();
    assert_eq!(sessions.len(), 1, "one running session");
    assert!(db.delete_session(&sessions[0].id).await.unwrap());

    tui.submit("this one can't be saved");
    tui.wait_for_screen("Couldn't save your message, so it wasn't sent");
    // The text is back in the box to send again.
    tui.wait_for_screen("this one can't be saved");
    assert!(
        model_calls(&server).await.is_empty(),
        "the model was called"
    );
}
