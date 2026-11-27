//! The TUI end to end, in this process ([`crate::test_tui`]): keys and slash
//! commands reach the host and what comes back is drawn. Moved here from
//! koda-cli's PTY suite, which keeps what only a real terminal shows.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use koda_test_utils::openai::{text, tool_calls};
use serde_json::json;

use crate::test_tui::{Driver, TestTui};

/// What the model is told about a tool call the stop kept from running.
const STOPPED: &str = "The user stopped the turn before this ran.";

fn esc() -> KeyEvent {
    KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
}

/// After a stop the turn calls the model no more, and the next prompt is a
/// turn of its own; what that turn sent.
async fn assert_stopped(driver: &Driver, calls_so_far: usize) -> String {
    assert_eq!(
        driver.model_calls().await,
        calls_so_far,
        "the turn called the model after the stop"
    );
    driver.submit("second prompt").await;
    let sent = driver.model_call(calls_so_far + 1).await;
    assert!(sent.contains("second prompt"), "{sent}");
    sent
}

/// Esc at a question the model asked stops the turn; the half-typed answer
/// isn't sent, and the next turn hears the call was stopped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn esc_stops_the_turn_at_a_question() {
    let tui = TestTui::start(vec![
        tool_calls(&[("call_1", "AskUser", json!({"question": "Which one?"}))]),
        text("All done."),
    ])
    .await;
    tui.run(async |driver: Driver| {
        driver.submit("first prompt").await;
        driver.wait_for_screen("Which one?").await;
        driver.type_keys("half an answer");
        driver.stop_turn(esc()).await;
        let sent = assert_stopped(&driver, 1).await;
        assert!(sent.contains(STOPPED), "no word of the stop: {sent}");
        assert!(!sent.contains("half an answer"), "{sent}");
        driver
    })
    .await;
}

/// Esc while typing the feedback of a rejection stops the turn; the
/// half-typed reason isn't sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn esc_stops_the_turn_while_typing_feedback() {
    let tui = TestTui::start_safe(vec![
        tool_calls(&[(
            "call_1",
            "Write",
            json!({"file_path": "a.txt", "content": "x\n"}),
        )]),
        text("All done."),
    ])
    .await;
    tui.run(async |driver: Driver| {
        driver.submit("first prompt").await;
        driver.wait_for_screen("approve").await;
        driver.press(KeyCode::Char('f'));
        driver.type_keys("half a reason");
        driver.stop_turn(esc()).await;
        let sent = assert_stopped(&driver, 1).await;
        assert!(sent.contains(STOPPED), "no word of the stop: {sent}");
        assert!(!sent.contains("half a reason"), "{sent}");
        assert!(!driver.project.path().join("a.txt").exists());
        driver
    })
    .await;
}

/// Esc at the loop cap's "Continue?" stops the turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn esc_stops_the_turn_at_the_loop_cap() {
    let project = tempfile::tempdir().unwrap();
    let agents = project.path().join(".koda/agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join("default.json"),
        json!({"name": "default", "system_prompt": "Test.", "max_iterations": 1}).to_string(),
    )
    .unwrap();
    let server = koda_test_utils::openai::model(vec![
        tool_calls(&[("call_1", "List", json!({}))]),
        text("All done."),
    ])
    .await;
    let tui = TestTui::start_in(server, project).await;
    tui.run(async |driver: Driver| {
        driver.submit("first prompt").await;
        driver.wait_for_screen("Hard cap reached. Continue?").await;
        driver.stop_turn(esc()).await;
        assert_stopped(&driver, 1).await;
        driver
    })
    .await;
}

/// Tab while a turn runs queues the text for after it: the running turn
/// never sees it, the next one starts with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn tab_queues_text_for_after_the_turn() {
    let slow = text("All done.").set_delay(std::time::Duration::from_secs(2));
    let tui = TestTui::start(vec![slow, text("All done.")]).await;
    tui.run(async |driver: Driver| {
        driver.submit("first prompt").await;
        driver.model_call(1).await;
        driver.type_keys("queued text");
        driver.press(KeyCode::Tab);
        driver.wait_for_screen("Queued:").await;
        let sent = driver.model_call(2).await;
        assert!(sent.contains("queued text"), "{sent}");
        let first = driver.model_call(1).await;
        assert!(
            !first.contains("queued text"),
            "the queued text reached the running turn: {first}"
        );
        driver
    })
    .await;
}

/// `/new` starts a session with none of the last one's history.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn new_starts_a_fresh_session() {
    let tui = TestTui::start(vec![text("All done."), text("All done.")]).await;
    tui.run(async |driver: Driver| {
        driver
            .submit_and_finish("echo topic please", "All done.")
            .await;
        driver.submit("/new").await;
        driver.wait_for_screen("new session").await;
        driver.submit_and_finish("a fresh start", "All done.").await;
        let sent = driver.model_call(2).await;
        assert!(sent.contains("a fresh start"), "{sent}");
        assert!(
            !sent.contains("echo topic please"),
            "history carried over: {sent}"
        );
        driver
    })
    .await;
}

/// `/rename` names the session and `/status` shows the name, the model and
/// the mode.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn rename_and_status() {
    let tui = TestTui::start_safe(vec![]).await;
    tui.run(async |driver: Driver| {
        driver.submit("/rename Parser work").await;
        driver.wait_for_screen("Renamed to \"Parser work\"").await;
        driver.submit("/status").await;
        driver.wait_for_screen("Instructions").await;
        let screen = driver.screen();
        assert!(screen.contains("Parser work"), "{screen}");
        assert!(screen.contains("stub-model"), "{screen}");
        assert!(screen.contains("safe"), "{screen}");
        driver
    })
    .await;
}
