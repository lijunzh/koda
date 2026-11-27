//! Stopping a turn, and what comes back after it.

use std::time::{Duration, Instant};

use crossterm::event::KeyCode;
use koda_test_utils::openai::{text, tool_calls};
use serde_json::json;

use super::{STOPPED, assert_stopped, ctrl, esc, running, write_file};
use crate::test_tui::{BUSY_PROMPT, Driver, TestTui, input_box};

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
        tool_calls(&[("call_1", "Write", write_file("a.txt"))]),
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
    let slow = text("All done.").set_delay(Duration::from_secs(2));
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

/// A stop sends nothing queued: the queued text comes back to the input box.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn a_stop_hands_queued_text_back() {
    let reply =
        tool_calls(&[("call_0", "Write", write_file("a.txt"))]).set_delay(Duration::from_secs(3));
    let tui = TestTui::start_safe(vec![reply]).await;
    tui.run(async |driver: Driver| {
        driver.submit("first prompt").await;
        driver.model_call(1).await;
        driver.type_keys("queued text");
        driver.press(KeyCode::Tab);
        driver.wait_for_screen("approve").await;
        driver.stop_turn(ctrl('c')).await;

        assert_eq!(driver.model_calls().await, 1);
        driver.wait_for_screen("Not sent to the model").await;
        let screen = driver.screen();
        assert!(input_box(&screen).contains("queuedtext"), "{screen}");
        // Empty the box for the `/exit` that ends the run.
        driver.key(ctrl('u'));
        driver
            .wait_for_screen_that(|s| !input_box(s).contains("queued"), "empty the box")
            .await;
        driver
    })
    .await;
}

/// `/stop` runs during a turn, as Codex's does, and the turn goes on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn stop_works_during_a_turn() {
    // 36 s: no other test sleeps that long, so pgrep finds only this one.
    let started = tool_calls(&[(
        "call_0",
        "Bash",
        json!({"command": "sleep 36", "background": true}),
    )]);
    let slow = text("All done.").set_delay(Duration::from_secs(5));
    let tui = TestTui::start_safe(vec![started, slow]).await;
    tui.run(async |driver: Driver| {
        driver.submit("start it").await;
        driver.approve_until(|| running("sleep 36")).await;
        driver.model_call(2).await;
        driver.submit("/stop").await;
        driver.wait_for_screen("Stopped 1 background task.").await;
        let screen = driver.screen();
        assert!(screen.contains(BUSY_PROMPT), "{screen}");
        let deadline = Instant::now() + Duration::from_secs(3);
        while running("sleep 36") {
            assert!(Instant::now() < deadline, "/stop left it running");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        driver.wait_for_screen("All done.").await;
        driver.wait_for_screen_without(BUSY_PROMPT).await;
        driver
    })
    .await;
}
