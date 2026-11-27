//! The real TUI in this process, for tests: [`TuiContext`] on a test screen,
//! its keys sent by the test, against an in-process host and a scripted
//! model. What the PTY tests in koda-cli check, without a terminal or a
//! process per test, as Codex's TUI tests render into a test backend.
//!
//! ```rust,ignore
//! let tui = TestTui::start(vec![text("Hello.")]).await;
//! tui.run(async |driver: Driver| {
//!     driver.submit("hi").await;
//!     driver.wait_for_screen("Hello.").await;
//!     driver
//! })
//! .await;
//! ```
//!
//! The config folder is the test's own, set through
//! [`koda_config::runtime_env`], which the TUI and the host both read. That
//! is process-wide, so these tests run one at a time ([`serial_test`]).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use tokio::sync::mpsc;

use koda_test_utils::network::{FakeLlmServer, ResponseTemplate};

use crate::session_id::Resume;
use crate::tui_context::TuiContext;
use crate::tui_types::TuiIo;

/// How long a wait for the screen lasts.
const WAIT: Duration = Duration::from_secs(20);

/// The prompt while a turn runs.
pub(crate) const BUSY_PROMPT: &str = "\u{23f3}>";

/// What a stopped turn says.
pub(crate) const INTERRUPTED: &str = "Interrupted";

/// The TUI under test, and what the test drives it with.
pub(crate) struct TestTui {
    pub ctx: TuiContext,
    pub driver: Driver,
}

/// What a test holds while the TUI runs: its keys and its screen.
pub(crate) struct Driver {
    screen: Arc<Mutex<TestBackend>>,
    keys: mpsc::UnboundedSender<Event>,
    /// The scripted model.
    pub server: FakeLlmServer,
    /// The project folder.
    pub project: tempfile::TempDir,
    home: tempfile::TempDir,
}

impl TestTui {
    /// A TUI on a 100x30 screen, its model answering with `replies` in turn.
    pub async fn start(replies: Vec<ResponseTemplate>) -> Self {
        let server = koda_test_utils::openai::model(replies).await;
        Self::start_on(server).await
    }

    /// A TUI whose model is `server`.
    pub async fn start_on(server: FakeLlmServer) -> Self {
        Self::start_in(server, tempfile::tempdir().unwrap()).await
    }

    /// A TUI in `project`, which the test has prepared (an agent file, say).
    pub async fn start_in(server: FakeLlmServer, project: tempfile::TempDir) -> Self {
        Self::start_with(server, project, "").await
    }

    /// A TUI in `project` with `config` as its `config.toml`.
    pub async fn start_with(
        server: FakeLlmServer,
        project: tempfile::TempDir,
        config: &str,
    ) -> Self {
        let home = tempfile::tempdir().unwrap();
        let koda = home.path().join(".config/koda");
        std::fs::create_dir_all(&koda).unwrap();
        std::fs::write(koda.join("config.toml"), config).unwrap();
        Self::open(server, project, home, None).await
    }

    /// A new TUI in the last one's home and project, on its server, as
    /// `koda` started again there: `resume` names the session to open.
    pub async fn start_again(driver: Driver, resume: Option<Resume>) -> Self {
        let Driver {
            server,
            project,
            home,
            ..
        } = driver;
        Self::open(server, project, home, resume).await
    }

    async fn open(
        server: FakeLlmServer,
        project: tempfile::TempDir,
        home: tempfile::TempDir,
        resume: Option<Resume>,
    ) -> Self {
        koda_config::runtime_env::set(
            "XDG_CONFIG_HOME",
            home.path().join(".config").to_string_lossy(),
        );
        let screen = Arc::new(Mutex::new(TestBackend::new(100, 30)));
        let (keys, rx) = mpsc::unbounded_channel::<Event>();
        let events = Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (Ok(event), rx))
        }));
        let named = crate::tui_context::settings::Named {
            provider: Some("lmstudio".to_string()),
            base_url: Some(server.url()),
            model: Some("stub-model".to_string()),
        };
        // Canonical, as `koda` hands it over (`/var` is `/private/var` on
        // macOS): sessions are listed by the root they were stored under.
        let root = std::fs::canonicalize(project.path()).unwrap();
        let ctx = TuiContext::new(
            root,
            named,
            resume,
            false,
            tokio::spawn(async { None }),
            false,
            crate::backend::Backend::InProcess,
            TuiIo::Test {
                screen: screen.clone(),
                events,
            },
        )
        .await
        .unwrap();
        Self {
            ctx,
            driver: Driver {
                screen,
                keys,
                server,
                project,
                home,
            },
        }
    }

    /// Run the TUI's event loop with `script` beside it, then `/exit`. The
    /// driver comes back, for what a test checks once the TUI has gone.
    pub async fn run<F, Fut>(mut self, script: F) -> Driver
    where
        F: FnOnce(Driver) -> Fut,
        Fut: std::future::Future<Output = Driver>,
    {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<crate::inference::TurnCommand>(32);
        let driver = self.driver;
        let keys = driver.keys.clone();
        let script = async move {
            let driver = script(driver).await;
            driver.submit("/exit").await;
            driver
        };
        let (looped, driver) = tokio::join!(self.ctx.run_event_loop(&cmd_tx, &mut cmd_rx), script);
        drop(keys);
        looped.unwrap();
        driver
    }
}

impl Driver {
    /// The screen, one line per row, with trailing spaces trimmed.
    ///
    /// A wide character covers the cell after it, as a terminal draws it.
    /// The test backend keeps whatever that cell held before (ratatui
    /// doesn't redraw a covered cell), which after a scroll can be another
    /// row's character: `⏳🔒>` where a terminal shows `⏳>`.
    pub fn screen(&self) -> String {
        use unicode_width::UnicodeWidthStr;
        let screen = self.screen.lock().unwrap();
        let buffer = screen.buffer();
        let area = buffer.area;
        (0..area.height)
            .map(|y| {
                let mut row = String::new();
                let mut x = 0;
                while x < area.width {
                    let symbol = buffer[(x, y)].symbol();
                    row.push_str(symbol);
                    x += (symbol.width() as u16).max(1);
                }
                row.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Wait until the screen shows `text`.
    pub async fn wait_for_screen(&self, text: &str) {
        self.wait_for_screen_that(|screen| screen.contains(text), &format!("show {text:?}"))
            .await;
    }

    /// Wait until `ready` holds for the screen.
    pub async fn wait_for_screen_that(&self, ready: impl Fn(&str) -> bool, what: &str) {
        let deadline = Instant::now() + WAIT;
        loop {
            let screen = self.screen();
            if ready(&screen) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting to {what}:\n{screen}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Wait until the screen no longer shows `text`.
    pub async fn wait_for_screen_without(&self, text: &str) {
        self.wait_for_screen_that(
            |screen| !screen.contains(text),
            &format!("stop showing {text:?}"),
        )
        .await;
    }

    /// Type `text` a key at a time.
    pub fn type_keys(&self, text: &str) {
        for c in text.chars() {
            self.press(KeyCode::Char(c));
        }
    }

    /// Send `prompt` and wait until the turn has finished with `reply` shown
    /// once more than before.
    pub async fn submit_and_finish(&self, prompt: &str, reply: &str) {
        let before = self.screen().matches(reply).count();
        self.submit(prompt).await;
        self.wait_for_screen_that(
            |screen| screen.matches(reply).count() > before && !screen.contains(BUSY_PROMPT),
            &format!("finish the turn for {prompt:?}"),
        )
        .await;
    }

    /// The `count`th chat request the model got (1 is the first), as text;
    /// waits for it.
    pub async fn model_call(&self, count: usize) -> String {
        model_call_on(&self.server, count).await
    }

    /// How many chat requests the model has had.
    pub async fn model_calls(&self) -> usize {
        koda_test_utils::openai::chat_requests(&self.server)
            .await
            .len()
    }

    /// Stop the running turn with `key` and wait until it has stopped.
    pub async fn stop_turn(&self, key: KeyEvent) {
        self.key(key);
        self.wait_for_screen(INTERRUPTED).await;
        self.wait_for_screen_without(BUSY_PROMPT).await;
    }

    /// Send a key.
    pub fn press(&self, code: KeyCode) {
        self.key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    /// Send a key with modifiers.
    pub fn key(&self, key: KeyEvent) {
        let _ = self.keys.send(Event::Key(key));
    }

    /// Paste `text`, as a terminal's bracketed paste delivers it.
    pub fn paste(&self, text: &str) {
        let _ = self.keys.send(Event::Paste(text.to_string()));
    }

    /// Wait until `done` holds, saying `what` if it never does.
    pub async fn wait_until(&self, done: impl Fn() -> bool, what: &str) {
        let deadline = Instant::now() + WAIT;
        while !done() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting until {what}:\n{}",
                self.screen()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Answer every approval with `y` until `done` holds.
    pub async fn approve_until(&self, done: impl Fn() -> bool) {
        let deadline = Instant::now() + WAIT;
        while !done() {
            assert!(Instant::now() < deadline, "timed out:\n{}", self.screen());
            if self.screen().contains("[y] approve") {
                self.press(KeyCode::Char('y'));
                self.wait_for_screen_without("[y] approve").await;
            } else {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }

    /// In an open picker, press Down until the highlighted line shows
    /// `text`. Pickers move with the arrow keys only, and Down wraps.
    pub async fn highlight(&self, text: &str) {
        let highlighted = |screen: &str| {
            screen
                .lines()
                .any(|line| line.trim_start().starts_with('\u{203a}') && line.contains(text))
        };
        for _ in 0..40 {
            if highlighted(&self.screen()) {
                return;
            }
            let before = self.screen();
            self.press(KeyCode::Down);
            self.wait_for_screen_that(|screen| screen != before, "move the highlight")
                .await;
        }
        panic!("never highlighted {text:?}:\n{}", self.screen());
    }

    /// Koda's config folder, where its database and `config.toml` are.
    pub fn config_dir(&self) -> std::path::PathBuf {
        self.home.path().join(".config/koda")
    }

    /// Koda's database.
    pub async fn db(&self) -> koda_state::db::Database {
        koda_state::db::Database::init(&self.config_dir())
            .await
            .unwrap()
    }

    /// The project's session ids, most recent first.
    pub async fn sessions(&self) -> Vec<String> {
        use koda_state::session_store::SessionStore;
        let project = std::fs::canonicalize(self.project.path()).unwrap();
        self.db()
            .await
            .list_sessions(10, &project)
            .await
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect()
    }

    /// The messages session `id` has stored, as one string.
    pub async fn stored_messages(&self, id: &str) -> String {
        use koda_state::session_store::SessionStore;
        self.db()
            .await
            .load_all_messages(id)
            .await
            .unwrap()
            .iter()
            .filter_map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Put `text` in the input box and press Enter.
    pub async fn submit(&self, text: &str) {
        self.paste(text);
        let want: String = text.split_whitespace().collect();
        self.wait_for_screen_that(
            |screen| crate::test_tui::input_box(screen).ends_with(&want),
            &format!("show {text:?} in the input box"),
        )
        .await;
        self.press(KeyCode::Enter);
    }
}

/// The `count`th chat request `server` got (1 is the first), as text;
/// waits for it.
pub(crate) async fn model_call_on(server: &FakeLlmServer, count: usize) -> String {
    let deadline = Instant::now() + WAIT;
    loop {
        let calls = koda_test_utils::openai::chat_requests(server).await;
        if let Some(call) = calls.get(count - 1) {
            return call.to_string();
        }
        assert!(Instant::now() < deadline, "no model call {count}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The input box's text: the lines between the last two rules, with the
/// spaces removed (the prompt sign stays: `>say hello`).
pub(crate) fn input_box(screen: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use koda_test_utils::openai::text;

    /// The harness itself: a prompt typed into the TUI reaches the scripted
    /// model, and its reply shows on the screen.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial]
    async fn a_prompt_gets_the_models_reply_on_screen() {
        let tui = TestTui::start(vec![text("Hello from the stub.")]).await;
        tui.run(async |driver: Driver| {
            driver.submit("say hello").await;
            driver.wait_for_screen("Hello from the stub.").await;
            let sent = koda_test_utils::openai::chat_requests(&driver.server).await;
            assert!(sent.last().unwrap().to_string().contains("say hello"));
            driver
        })
        .await;
    }
}
