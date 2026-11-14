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

/// The prompt while a turn runs. It is drawn until the turn loop is done,
/// which can be after the turn's last words are on screen.
const BUSY_PROMPT: &str = "\u{23f3}>";

/// How long the composer waits before it stops treating fast typing as a paste.
const PASTE_SETTLE: Duration = Duration::from_millis(400);

const ESC: &str = "\x1b";
const CTRL_C: &str = "\x03";
const CTRL_O: &str = "\x0f";
const CTRL_D: &str = "\x04";

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
        let mut args = vec!["--provider", "lmstudio"];
        args.extend(extra);
        Self::start_with_flags(home, base_url, &args, ready)
    }

    /// [`Self::start_with_args`] with no `--provider` unless `flags` has one.
    fn start_with_flags(home: &Path, base_url: &str, flags: &[&str], ready: &str) -> Self {
        Self::start_full(home, base_url, flags, Some("stub-model"), ready)
    }

    /// Start `koda` with only `--base-url` naming the model choice, so the
    /// saved choice fills in the rest.
    fn start_without_model(home: &Path, base_url: &str) -> Self {
        Self::start_full(home, base_url, &[], None, IDLE_HINT)
    }

    fn start_full(
        home: &Path,
        base_url: &str,
        flags: &[&str],
        model: Option<&str>,
        ready: &str,
    ) -> Self {
        let (master, slave) = open_pty();
        let mut command = Command::new(env!("CARGO_BIN_EXE_koda"));
        command
            .args(["--base-url", base_url])
            .args(model.map(|m| ["--model", m]).into_iter().flatten())
            .args(["--max-context-tokens", "64000"])
            .args(["--mode", "safe"])
            .args(flags)
            .arg("--project-root")
            .arg(home)
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("TERM", "xterm-256color")
            // No real provider is ever called: a key from the developer's
            // shell would send the startup's Models API lookup to Anthropic.
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("GEMINI_API_KEY")
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

    /// Submit `prompt` and wait for its turn to end, which shows `reply`.
    ///
    /// Waiting only for the busy prompt to go isn't enough: on a slow machine
    /// the model can have the call before the screen shows the busy prompt,
    /// and keys typed then go to the running turn. A frame that shows the
    /// reply one more time was drawn during the turn or after it, so the
    /// prompt it shows says whether the turn has ended.
    fn submit_and_finish(&mut self, prompt: &str, reply: &str) {
        let before = self.screen().matches(reply).count();
        self.submit(prompt);
        self.wait_for_screen_that(
            |screen| screen.matches(reply).count() > before && !screen.contains(BUSY_PROMPT),
            &format!("finish the turn for {prompt:?}"),
        );
    }

    /// Press `key` and wait for the turn to stop: the engine says so, and
    /// the turn ends a moment later, when its `turn/completed` reaches the
    /// TUI. Keys typed before that still go to the turn.
    fn stop_turn(&mut self, key: &str) {
        let start = self.output_len();
        self.type_text(key);
        self.wait_for_output_after(start, INTERRUPTED);
        self.wait_for_screen_without(BUSY_PROMPT);
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
    sse(&[
        tool_call_delta(calls),
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ])
}

fn tool_call_delta(calls: &[(&str, Value)]) -> Value {
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
    json!({"choices": [{"delta": {"tool_calls": calls}}]})
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

fn bash(command: &str) -> ResponseTemplate {
    tool_calls(&[("Bash", json!({ "command": command }))])
}

/// `s` approves a local change for the rest of the session: the same command
/// runs again without a prompt. A destructive command doesn't offer `s`, and
/// pressing it there does nothing.
#[tokio::test(flavor = "multi_thread")]
async fn s_allows_a_command_for_the_session_but_not_a_destructive_one() {
    let server = scripted_model(vec![
        bash("touch a.txt"),
        bash("touch a.txt"),
        bash("rm -rf build"),
    ])
    .await;
    let home = home();
    let mut tui = Tui::start_with_model(home.path(), &server.url());

    tui.submit("first prompt");
    tui.wait_for_screen("[s] this session");
    tui.type_text("s");

    // The repeat runs unasked, so the next prompt is the destructive one.
    tui.wait_for_screen("rm -rf build");
    tui.wait_for_screen("[y] approve");
    assert!(
        !tui.screen().contains("[s] this session"),
        "{}",
        tui.screen()
    );
    tui.type_text("s");
    assert!(tui.screen().contains("[y] approve"), "{}", tui.screen());
    assert_eq!(model_calls(&server).await.len(), 3);

    tui.type_text("y");
    tui.wait_for_screen("All done.");
    assert!(home.path().join("a.txt").exists());
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
    tui.wait_for_screen("Not sent to the model");
    assert!(
        input_box(&tui.screen()).contains("steeringtext"),
        "{}",
        tui.screen()
    );
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
///
/// Ctrl+D quits only at idle: during a turn it goes to the composer, where it
/// deletes nothing. So it waits for the turn to end, not just its last words.
fn quit_and_read_session_id(tui: &mut Tui) -> String {
    tui.wait_for_screen_without(BUSY_PROMPT);
    tui.type_text(CTRL_D);
    assert!(tui.exits(), "koda stayed up after Ctrl+D");
    resume_hint_session_id(tui)
}

/// The session id in the resume hint Koda printed as it quit.
fn resume_hint_session_id(tui: &Tui) -> String {
    let text = tui.output_text();
    text.split("koda --resume ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .map(str::to_string)
        .unwrap_or_else(|| panic!("no resume hint in:\n{text}"))
}

/// What you typed comes back with Up after a restart. The TUI keeps it in
/// its own `history.jsonl`, in Codex's record shape, not in the database.
#[tokio::test(flavor = "multi_thread")]
async fn typed_prompts_come_back_with_up_after_a_restart() {
    let server = scripted_model(vec![final_text()]).await;
    let home = home();
    let mut tui = Tui::start_with_model(home.path(), &server.url());
    tui.submit("remember this prompt");
    wait_for_model_call(&server, 1).await;
    let session = quit_and_read_session_id(&mut tui);

    let text = std::fs::read_to_string(home.path().join(".config/koda/history.jsonl")).unwrap();
    let entry: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    assert_eq!(entry["text"], "remember this prompt");
    assert_eq!(entry["session_id"], session.as_str());

    let mut tui = Tui::start_with_model(home.path(), &server.url());
    tui.type_text("\x1b[A");
    tui.wait_for_screen("remember this prompt");
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

/// Ctrl+D on an empty composer quits during a turn too, as in Codex. It
/// used to go to the composer and do nothing. The turn stops first, so its
/// command ends and the session can be resumed.
#[tokio::test(flavor = "multi_thread")]
async fn ctrl_d_quits_during_a_turn() {
    // 33 s: no other test sleeps that long, so pgrep finds only this one.
    let server = scripted_model(vec![tool_calls(&[(
        "Bash",
        json!({"command": "sleep 33"}),
    )])])
    .await;
    let home = home();
    let mut tui = Tui::start_with_model(home.path(), &server.url());

    tui.submit("run it");
    tui.approve_until(|| running("sleep 33"));
    tui.type_text(CTRL_D);
    assert!(tui.exits(), "koda stayed up after Ctrl+D during a turn");
    let session = resume_hint_session_id(&tui);
    let deadline = Instant::now() + Duration::from_secs(3);
    while running("sleep 33") {
        assert!(Instant::now() < deadline, "the command outlived the quit");
        std::thread::sleep(Duration::from_millis(50));
    }

    let tui = Tui::start_with_args(
        home.path(),
        &server.url(),
        &["--resume", &session],
        IDLE_HINT,
    );
    tui.wait_for_screen("session resumed");
    assert!(tui.screen().contains("run it"), "{}", tui.screen());
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
    // No `--provider`: the first run asks which one.
    let mut tui = Tui::start_with_flags(home.path(), &server.url(), &[], "Welcome to Koda");
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

/// A first run that names its provider on the command line goes straight to
/// the composer, as Codex skips the screens a start doesn't need: the picker
/// would only ask again what `--provider` said.
#[tokio::test(flavor = "multi_thread")]
async fn a_first_run_with_a_provider_flag_skips_the_picker() {
    let server = scripted_model(vec![]).await;
    // No `.config/koda`: Koda has never run here.
    let home = tempfile::tempdir().unwrap();
    let mut tui = Tui::start_with_model(home.path(), &server.url());
    let screen = tui.screen();
    assert!(!screen.contains("Welcome to Koda"), "{screen}");
    assert!(!screen.contains("Choose your LLM provider"), "{screen}");

    tui.submit("hello");
    let sent = wait_for_model_call(&server, 1).await;
    assert!(sent.contains("hello"), "{sent}");
    tui.wait_for_screen("All done.");
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
    tui.wait_for_screen("Model: stub-picked");

    tui.submit("hi");
    let sent = wait_for_model_call(&server, 1).await;
    assert!(sent.contains(r#""model":"stub-picked""#), "{sent}");
}

/// A model picked in `/model` is the next start's choice: the host saves it
/// (`config/batchWrite`) and a new session starts from it, with the flags
/// winning where they name something.
#[tokio::test(flavor = "multi_thread")]
async fn a_model_choice_is_the_next_starts_choice() {
    let server = scripted_model(vec![]).await;
    let models = json!({"object": "list", "data": [
        {"id": "stub-local", "object": "model", "type": "llm", "state": "loaded"}
    ]});
    server.mount_ok("GET", r".*/models$", models).await;
    let home = home();
    let mut tui = Tui::start_with_model(home.path(), &server.url());
    run_command(&mut tui, "/model");
    tui.wait_for_screen("Select a model");
    highlight(&mut tui, "stub-local");
    tui.type_text("\r");
    tui.wait_for_screen("Model: stub-local");
    quit_and_read_session_id(&mut tui);

    // Only `--base-url`, LM Studio's like the saved one: the saved model.
    let mut tui = Tui::start_without_model(home.path(), &server.url());
    tui.submit("which model");
    let sent = wait_for_model_call(&server, 1).await;
    assert!(sent.contains(r#""model":"stub-local""#), "{sent}");
}

/// Shift+Tab changes the mode on the host, which records it with the
/// session (`session/settings/update`); the TUI no longer writes it. Safe
/// goes to Auto, which needs the sandbox.
#[tokio::test(flavor = "multi_thread")]
async fn shift_tab_sends_the_mode_to_the_host() {
    use koda_core::persistence::Persistence;
    if !koda_core::sandbox::is_available() {
        eprintln!("skipped: Auto needs the sandbox, which isn't here");
        return;
    }
    let server = scripted_model(vec![]).await;
    let home = home();
    let tui_project = std::fs::canonicalize(home.path()).unwrap();
    let mut tui = Tui::start_with_model(home.path(), &server.url());
    let db = koda_core::db::Database::open(&home.path().join(".config/koda/db/koda.db"))
        .await
        .unwrap();
    let session = db.list_sessions(10, &tui_project).await.unwrap()[0]
        .id
        .clone();
    assert_eq!(
        db.get_session_mode(&session).await.unwrap().as_deref(),
        Some("safe")
    );

    tui.type_text("\x1b[Z");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mode = db.get_session_mode(&session).await.unwrap();
        if mode.as_deref() == Some("auto") {
            break;
        }
        assert!(Instant::now() < deadline, "the mode stayed {mode:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
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

// ── Slash commands, before the client API moves them ──────────────────────
//
// Each test checks what a command changes, mostly what the model is sent
// next, not only what the screen says, so the move behind one client API
// (vault: "Koda - Client API Design") can't break one quietly.

/// A plain JSON reply, for the calls Koda doesn't stream (compaction).
fn json_reply(text: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "application/json")
        .set_body_string(
            json!({
                "id": "c", "object": "chat.completion", "model": "stub-model",
                "choices": [{"index": 0, "finish_reason": "stop",
                             "message": {"role": "assistant", "content": text}}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 10, "total_tokens": 20},
            })
            .to_string(),
        )
}

/// A reply with this text and nothing else.
fn text_reply(text: &str) -> ResponseTemplate {
    sse(&[
        json!({"choices": [{"delta": {"content": text}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
    ])
}

/// Run `command`: type it, wait until the input box shows all of it, then
/// Enter. Typed in one write, a command can reach Koda as a fast burst that
/// Koda holds as a paste until it goes quiet; an Enter that arrives while
/// the paste is still held is a newline in the paste, not a submit.
fn run_command(tui: &mut Tui, command: &str) {
    tui.type_text(command);
    let want: String = command.split_whitespace().collect();
    tui.wait_for_screen_that(
        |screen| input_box(screen).ends_with(&want),
        &format!("show {command:?} in the input box"),
    );
    tui.type_text("\r");
}

/// The input box's text, prompt glyph included, whitespace removed so a
/// wrapped line compares equal: the lines between the last two horizontal
/// rules.
fn input_box(screen: &str) -> String {
    let lines: Vec<&str> = screen.lines().collect();
    let rules: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.matches('\u{2500}').count() >= 20)
        .map(|(i, _)| i)
        .collect();
    let [.., top, bottom] = rules[..] else {
        return String::new();
    };
    lines[top + 1..bottom]
        .iter()
        .flat_map(|line| line.split_whitespace())
        .collect()
}

const DOWN: &str = "\x1b[B";

/// In an open picker, press Down until the highlighted line shows `text`.
/// Pickers move with the arrow keys only, and Down wraps.
fn highlight(tui: &mut Tui, text: &str) {
    for _ in 0..40 {
        let screen = tui.screen();
        if screen
            .lines()
            .any(|line| line.trim_start().starts_with('\u{203a}') && line.contains(text))
        {
            return;
        }
        tui.type_text(DOWN);
    }
    panic!("never highlighted {text:?}:\n{}", tui.screen());
}

/// The lines of the screen from the open picker's title down.
fn picker_lines(screen: &str, title: &str) -> String {
    screen
        .lines()
        .skip_while(|line| !line.contains(title))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A command whose start is typed and whose rest arrives as a paste,
/// Enter included, runs whole. This is also what a busy Koda sees when it
/// reads typed keys late, in one batch. The typed `/me` opens the slash popup;
/// the open popup used to take Enter while the rest was still held as a
/// paste, complete `/memory `, and the held text then landed after it:
/// `/memory mory add …`.
#[tokio::test(flavor = "multi_thread")]
async fn enter_during_a_held_paste_does_not_reach_the_slash_popup() {
    let server = scripted_model(vec![]).await;
    let home = home();
    let mut tui = Tui::start_with_model(home.path(), &server.url());
    for ch in ["/", "m", "e"] {
        tui.type_text(ch);
    }
    tui.wait_for_screen("/memory");
    tui.type_text("mory add the code word is zebrafish\r");
    tui.wait_for_screen("Saved to Koda's notes for this project");
    tui.wait_for_screen("/memory add the code word is zebrafish");
}

/// `/model` lists the session's provider's models, as Codex's `/model`
/// lists its thread's: the local server's, and no other provider's alias.
/// Esc changes nothing.
#[tokio::test(flavor = "multi_thread")]
async fn model_picker_lists_the_sessions_provider_and_esc_changes_nothing() {
    let server = scripted_model(vec![]).await;
    let models = json!({"object": "list", "data": [
        {"id": "stub-local", "object": "model", "type": "llm", "state": "loaded"}
    ]});
    server.mount_ok("GET", r".*/models$", models).await;
    let home = home();
    let mut tui = Tui::start_with_model(home.path(), &server.url());

    run_command(&mut tui, "/model");
    tui.wait_for_screen("Select a model");
    highlight(&mut tui, "stub-local");
    let listed = picker_lines(&tui.screen(), "Select a model");
    assert!(!listed.contains("claude-haiku"), "{listed}");
    tui.type_text(ESC);
    tui.wait_for_screen_without("Select a model");

    tui.submit("hi");
    let sent = wait_for_model_call(&server, 1).await;
    assert!(sent.contains(r#""model":"stub-model""#), "{sent}");
}

/// Picking a model in `/model` changes it on the host, for the same
/// session and server; another provider's model is refused with the way to
/// switch provider.
#[tokio::test(flavor = "multi_thread")]
async fn model_picker_pick_changes_the_model_on_the_host() {
    let server = scripted_model(vec![text_reply("From stub-local.")]).await;
    let models = json!({"object": "list", "data": [
        {"id": "stub-local", "object": "model", "type": "llm", "state": "loaded"}
    ]});
    server.mount_ok("GET", r".*/models$", models).await;
    let home = home();
    let mut tui = Tui::start_with_model(home.path(), &server.url());

    run_command(&mut tui, "/model");
    tui.wait_for_screen("Select a model");
    highlight(&mut tui, "stub-local");
    tui.type_text("\r");
    tui.wait_for_screen("Model: stub-local");

    run_command(&mut tui, "/model gemini-flash");
    tui.wait_for_screen("(/provider)");

    tui.submit("which model");
    let sent = wait_for_model_call(&server, 1).await;
    assert!(sent.contains(r#""model":"stub-local""#), "{sent}");
    tui.wait_for_screen("From stub-local.");
}

/// `/provider <url>` moves the session to the server at that address: the
/// next request goes there, not to the one Koda started with.
#[tokio::test(flavor = "multi_thread")]
async fn provider_with_an_address_moves_the_next_turn_there() {
    let first = scripted_model(vec![]).await;
    let second = scripted_model(vec![text_reply("From the second server.")]).await;
    let models = json!({"object": "list", "data": [
        {"id": "stub-model", "object": "model", "type": "llm", "state": "loaded"}
    ]});
    second.mount_ok("GET", r".*/models$", models).await;
    let home = home();
    let mut tui = Tui::start_with_model(home.path(), &first.url());

    run_command(&mut tui, &format!("/provider {}", second.url()));
    // A session's provider and server don't change: this is a new one.
    tui.wait_for_screen("New session on");
    tui.submit("where are you");
    let sent = wait_for_model_call(&second, 1).await;
    assert!(sent.contains("where are you"), "{sent}");
    tui.wait_for_screen("From the second server.");
    assert!(
        model_calls(&first).await.is_empty(),
        "the first server still got the turn"
    );
}

/// `/resume` lists earlier sessions; picking one resumes it, and the model
/// gets its history.
#[tokio::test(flavor = "multi_thread")]
async fn sessions_picker_resumes_an_earlier_session() {
    let server = scripted_model(vec![]).await;
    let home = home();
    let mut tui = Tui::start_with_model(home.path(), &server.url());
    tui.submit("alpha topic please");
    tui.wait_for_screen("All done.");
    quit_and_read_session_id(&mut tui);

    let mut tui = Tui::start_with_model(home.path(), &server.url());
    // The old name still works, and says the new one.
    run_command(&mut tui, "/sessions");
    tui.wait_for_screen("/sessions is now /resume.");
    tui.wait_for_screen("Sessions");
    highlight(&mut tui, "alpha topic");
    tui.type_text("\r");
    tui.wait_for_screen("Resumed session");

    tui.submit("continue it");
    let sent = wait_for_model_call(&server, 2).await;
    assert!(sent.contains("alpha topic please"), "history lost: {sent}");
    assert!(sent.contains("continue it"), "{sent}");
}

/// `/resume <id>` resumes by id; `/resume delete <id>` removes a
/// session, and the picker no longer offers it.
#[tokio::test(flavor = "multi_thread")]
async fn sessions_resume_and_delete_by_id() {
    let server = scripted_model(vec![]).await;
    let home = home();
    let mut tui = Tui::start_with_model(home.path(), &server.url());
    tui.submit("bravo session words");
    tui.wait_for_screen("All done.");
    let bravo = quit_and_read_session_id(&mut tui);
    let mut tui = Tui::start_with_model(home.path(), &server.url());
    tui.submit("charlie session words");
    tui.wait_for_screen("All done.");
    let charlie = quit_and_read_session_id(&mut tui);

    let mut tui = Tui::start_with_model(home.path(), &server.url());
    run_command(&mut tui, &format!("/resume {bravo}"));
    tui.wait_for_screen("bravo session words");
    tui.submit_and_finish("bravo again", "All done.");
    let sent = wait_for_model_call(&server, 3).await;
    assert!(sent.contains("bravo session words"), "{sent}");
    assert!(!sent.contains("charlie session words"), "{sent}");

    run_command(&mut tui, &format!("/resume delete {charlie}"));
    tui.wait_for_screen_without(BUSY_PROMPT);
    run_command(&mut tui, "/resume");
    tui.wait_for_screen("Sessions");
    let listed = picker_lines(&tui.screen(), "Sessions");
    assert!(
        !listed.contains(&charlie[..8]),
        "deleted session listed:\n{listed}"
    );
    assert!(listed.contains(&bravo[..8]), "{listed}");
}

/// A stdio MCP server with one tool, `lookup`.
const MCP_SERVER: &str = r#"while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"notes","version":"0.1.0"}}}\n' "$id" ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"lookup","description":"Look a note up","inputSchema":{"type":"object","properties":{}}}]}}\n' "$id" ;;
    *'"method":"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"lookup-ran"}]}}\n' "$id" ;;
  esac
done
"#;

/// `/mcp add` saves a server and connects it at once, even with none
/// running when Koda started: every session has its MCP manager. The next
/// start starts it again in the background, and the first turn waits for
/// it, so its tools reach the model; `/mcp` lists it, and `/mcp remove`
/// takes its tools away.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_add_list_and_remove_change_the_tools_the_model_gets() {
    let server = scripted_model(vec![]).await;
    let home = home();
    let script = home.path().join("notes-server.sh");
    std::fs::write(&script, MCP_SERVER).unwrap();
    let mut tui = Tui::start_with_model(home.path(), &server.url());
    run_command(&mut tui, &format!("/mcp add notes sh {}", script.display()));
    tui.wait_for_screen("MCP server 'notes' added and connected (1 tools)");
    quit_and_read_session_id(&mut tui);

    let mut tui = Tui::start_with_model(home.path(), &server.url());
    tui.submit("use your tools");
    let sent = wait_for_model_call(&server, 1).await;
    assert!(
        sent.contains("notes__lookup"),
        "MCP tool not offered: {sent}"
    );
    tui.wait_for_screen("All done.");
    tui.wait_for_screen_without(BUSY_PROMPT);
    run_command(&mut tui, "/mcp list");
    tui.wait_for_screen("notes (1 tools)");

    run_command(&mut tui, "/mcp remove notes");
    tui.wait_for_screen("MCP server 'notes' removed");
    tui.submit("and now");
    let sent = wait_for_model_call(&server, 2).await;
    assert!(
        !sent.contains("notes__lookup"),
        "removed tool still offered: {sent}"
    );
}

/// `/memory add` saves a note for this project, and the next request
/// carries it, with no restart. Memory used to load once, when the agent
/// started, so the note arrived only after a restart.
#[tokio::test(flavor = "multi_thread")]
async fn memory_add_reaches_the_next_request() {
    let server = scripted_model(vec![]).await;
    let home = home();
    let mut tui = Tui::start_with_model(home.path(), &server.url());
    run_command(&mut tui, "/memory add the code word is zebrafish");
    tui.wait_for_screen("Saved to Koda's notes for this project");

    tui.submit("what is the code word");
    let sent = wait_for_model_call(&server, 1).await;
    assert!(
        sent.contains("zebrafish"),
        "the note didn't reach the model: {sent}"
    );
}

/// `/compact` summarizes the earlier turns with the model, and the next
/// request carries the summary instead of the turns it replaced.
#[tokio::test(flavor = "multi_thread")]
async fn compact_replaces_earlier_turns_with_the_summary() {
    let summary = "<summary>SUMMARY-MARKER-QUOKKA covers the earlier turns.</summary>";
    let server = scripted_model(vec![
        final_text(),
        final_text(),
        final_text(),
        json_reply(summary),
    ])
    .await;
    let home = home();
    let mut tui = Tui::start_with_model(home.path(), &server.url());
    for (n, prompt) in ["turn one oldest", "turn two", "turn three"]
        .iter()
        .enumerate()
    {
        tui.submit_and_finish(prompt, "All done.");
        wait_for_model_call(&server, n + 1).await;
    }

    run_command(&mut tui, "/compact");
    tui.wait_for_screen("Compacted");
    let sent = wait_for_model_call(&server, 4).await;
    assert!(
        sent.contains("turn one oldest"),
        "the summary call lacks history: {sent}"
    );

    tui.submit("after compaction");
    let sent = wait_for_model_call(&server, 5).await;
    assert!(sent.contains("SUMMARY-MARKER-QUOKKA"), "no summary: {sent}");
    // The summary carries earlier prompts as text; the turn itself is gone.
    let body: Value = serde_json::from_str(&sent).unwrap();
    let as_message = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["content"] == "turn one oldest");
    assert!(!as_message, "the oldest turn survived as a message: {sent}");
    assert!(sent.contains("after compaction"), "{sent}");
}

/// Esc stops `/compact` as it stops a turn (the host runs it as one), and
/// the history stays as it was.
#[tokio::test(flavor = "multi_thread")]
async fn esc_stops_a_compaction_and_keeps_the_history() {
    let server = scripted_model(vec![
        final_text(),
        final_text(),
        final_text(),
        json_reply("<summary>NEVER-SEEN</summary>").set_delay(Duration::from_secs(30)),
        final_text(),
    ])
    .await;
    let home = home();
    let mut tui = Tui::start_with_model(home.path(), &server.url());
    for (n, prompt) in ["turn one oldest", "turn two", "turn three"]
        .iter()
        .enumerate()
    {
        tui.submit_and_finish(prompt, "All done.");
        wait_for_model_call(&server, n + 1).await;
    }

    run_command(&mut tui, "/compact");
    wait_for_model_call(&server, 4).await;
    tui.stop_turn(ESC);

    tui.submit("after the stop");
    let sent = wait_for_model_call(&server, 5).await;
    let body: Value = serde_json::from_str(&sent).unwrap();
    let kept = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["content"] == "turn one oldest");
    assert!(kept, "the oldest turn is gone: {sent}");
    assert!(!sent.contains("NEVER-SEEN"), "{sent}");
}

/// No more model calls come within a couple of seconds: nothing started a
/// turn on its own.
async fn assert_no_more_calls(server: &FakeLlmServer, calls: usize) {
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        model_calls(server).await.len(),
        calls,
        "a turn started that nobody asked for"
    );
}

/// Text sent while the model works reaches it at the next call of the same
/// turn, after the tool result it was waiting on. It isn't handed back or
/// run again as a turn of its own.
#[tokio::test(flavor = "multi_thread")]
async fn steering_text_reaches_the_next_call_of_the_turn() {
    let read =
        tool_calls(&[("Read", json!({"file_path": "notes.md"}))]).set_delay(Duration::from_secs(3));
    let server = scripted_model(vec![read]).await;
    let home = home();
    std::fs::write(home.path().join("notes.md"), "alpha line\n").unwrap();
    let mut tui = Tui::start_with_model(home.path(), &server.url());

    tui.submit("first prompt");
    wait_for_model_call(&server, 1).await;
    tui.submit("steering text");

    let sent = wait_for_model_call(&server, 2).await;
    let body: Value = serde_json::from_str(&sent).unwrap();
    let messages = body["messages"].as_array().unwrap();
    let result = messages
        .iter()
        .position(|m| m["role"] == "tool")
        .unwrap_or_else(|| panic!("no tool result: {sent}"));
    let steer = messages
        .iter()
        .position(|m| m["role"] == "user" && m.to_string().contains("steering text"))
        .unwrap_or_else(|| panic!("no steering text: {sent}"));
    assert!(steer > result, "steering text before the result: {sent}");

    tui.wait_for_screen("All done.");
    tui.wait_for_screen_without(BUSY_PROMPT);
    let screen = tui.screen();
    assert!(!screen.contains("Not sent to the model"), "{screen}");
    assert!(!input_box(&screen).contains("steeringtext"), "{screen}");
    assert_no_more_calls(&server, 2).await;
}

/// About 9,000 characters of text that doesn't repeat, each line starting
/// with `marker`.
fn long_text(marker: &str, seed: u64) -> String {
    let mut x = seed;
    (0..120)
        .map(|i| {
            let mut words = String::new();
            for _ in 0..3 {
                // splitmix64
                x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
                let mut z = x;
                z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
                words.push_str(&format!(" {:016x}", z ^ (z >> 31)));
            }
            format!("{marker} {i}:{words}\n")
        })
        .collect()
}

/// A turn whose context passes the compaction threshold compacts before its
/// next call and goes on: the summary call gets the oldest reply, the call
/// after it carries the summary and the user's prompt instead of it, and
/// the turn ends on its own.
#[tokio::test(flavor = "multi_thread")]
async fn a_compaction_during_a_turn_lets_the_turn_go_on() {
    let summary = "<summary>SUMMARY-MARKER-WOMBAT covers the replies.</summary>";
    let home = home();
    // Six replies of about 9,000 characters, each with a small read: past a
    // fifth of the 64,000-token window, so there is history enough to
    // compact. The bulk is the model's own text because Koda shortens older
    // reads during a turn, and one call per reply because compaction keeps a
    // call with its results.
    let mut replies: Vec<ResponseTemplate> = (0..6u64)
        .map(|n| {
            let name = format!("part{n}.txt");
            std::fs::write(home.path().join(&name), "a line\n").unwrap();
            let marker = if n == 0 {
                "OLDEST-REPLY-MARKER"
            } else {
                "later"
            };
            let mut chunks = vec![
                json!({"choices": [{"delta": {"content": long_text(marker, n)}}]}),
                tool_call_delta(&[("Read", json!({ "file_path": name }))]),
                json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
            ];
            // The provider counts far more than Koda's estimate at the last
            // reply, so the call after it starts past the threshold.
            if n == 5 {
                chunks.push(json!({"choices": [], "usage": {
                    "prompt_tokens": 56_000, "completion_tokens": 10, "total_tokens": 56_010,
                }}));
            }
            sse(&chunks)
        })
        .collect();
    replies.push(json_reply(summary));
    let server = scripted_model(replies).await;
    let mut tui = Tui::start_with_model(home.path(), &server.url());

    tui.submit("first prompt");
    // The replies scroll it off the screen; it is in the output.
    tui.wait_for_output("compacting before sending");
    let sent = wait_for_model_call(&server, 7).await;
    assert!(
        sent.contains("OLDEST-REPLY-MARKER"),
        "the summary call lacks the oldest reply: {sent}"
    );

    let sent = wait_for_model_call(&server, 8).await;
    assert!(sent.contains("SUMMARY-MARKER-WOMBAT"), "no summary: {sent}");
    assert!(sent.contains("first prompt"), "the prompt was lost: {sent}");
    assert!(
        !sent.contains("OLDEST-REPLY-MARKER"),
        "the oldest reply survived the compaction: {sent}"
    );

    tui.wait_for_screen("All done.");
    tui.wait_for_screen_without(BUSY_PROMPT);
    assert_no_more_calls(&server, 8).await;
}

/// Esc while a sub-agent works stops the turn and the sub-agent, and nothing
/// starts another turn. The sub-agent's last status reaches the TUI after
/// the turn has stopped, which the TUI reads at idle; a finished
/// background agent used to start a turn there (#1349).
#[tokio::test(flavor = "multi_thread")]
async fn esc_during_a_sub_agent_starts_no_other_turn() {
    let invoke = tool_calls(&[(
        "InvokeAgent",
        json!({"agent_name": "explore", "prompt": "look around"}),
    )]);
    // Sub-agents call the model without streaming.
    let slow = json_reply("too late").set_delay(Duration::from_secs(10));
    let server = scripted_model(vec![invoke, slow]).await;
    let home = home();
    let mut tui = Tui::start_with_model(home.path(), &server.url());

    tui.submit("first prompt");
    let sent = wait_for_model_call(&server, 2).await;
    assert!(
        sent.contains("look around"),
        "not the sub-agent's call: {sent}"
    );
    tui.stop_turn(ESC);
    tui.wait_for_screen_without(BUSY_PROMPT);

    assert_no_more_calls(&server, 2).await;
    assert_stopped(&mut tui, &server, 2).await;
}

/// A sub-agent that finishes hands its answer to the turn, and the turn ends
/// after its last reply; nothing starts another turn.
#[tokio::test(flavor = "multi_thread")]
async fn a_finished_sub_agent_starts_no_other_turn() {
    let invoke = tool_calls(&[(
        "InvokeAgent",
        json!({"agent_name": "explore", "prompt": "look around"}),
    )]);
    // Sub-agents call the model without streaming.
    let server = scripted_model(vec![invoke, json_reply("SUB-AGENT-ANSWER")]).await;
    let home = home();
    let mut tui = Tui::start_with_model(home.path(), &server.url());

    tui.submit("first prompt");
    let sent = wait_for_model_call(&server, 3).await;
    assert!(
        sent.contains("SUB-AGENT-ANSWER"),
        "no sub-agent answer: {sent}"
    );
    tui.wait_for_screen("All done.");
    tui.wait_for_screen_without(BUSY_PROMPT);
    assert_no_more_calls(&server, 3).await;
}

/// `koda "prompt"` opens the TUI and sends the prompt, as `codex "prompt"`
/// does.
#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_given_to_koda_is_sent_when_the_tui_opens() {
    let server = scripted_model(vec![]).await;
    let home = home();
    let tui = Tui::start_with_args(
        home.path(),
        &server.url(),
        &["say hello from the command line"],
        IDLE_HINT,
    );
    let sent = wait_for_model_call(&server, 1).await;
    assert!(sent.contains("say hello from the command line"), "{sent}");
    tui.wait_for_screen("All done.");
}
