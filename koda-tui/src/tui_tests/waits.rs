//! Waits for one of the host's messages keep the others, in order: a turn
//! Koda starts meanwhile is followed, with its approvals, and a settings
//! report nothing waited for is mirrored when it comes.

use std::time::Duration;

use koda_client::protocol::goal::{GoalStatus, SessionGoalSetParams};
use koda_client::protocol::methods::SessionGoalSet;
use koda_test_utils::openai::{model, text, tool_calls};
use serde_json::json;
use tokio::sync::mpsc;

use super::{
    asked_write, go, is_turn_started, models_listing, read_until, start_turn, wait_for_go,
};
use crate::test_tui::{BUSY_PROMPT, Driver, TestTui};
use crate::tui_context::TurnRequest;
use crate::tui_context::settings::{ModelScope, fetch_settings};

/// What the TUI says when it gave up waiting for a settings report.
const NO_REPORT: &str = "didn't report the session's settings";

/// The status bar: the screen's last line.
fn status_bar(screen: &str) -> &str {
    screen.lines().last().unwrap_or_default()
}

/// Shift+Tab just as Koda starts a turn on the session's goal: the host
/// applies the mode once that turn ends, and the TUI follows the turn, with
/// its approval. It waited out the settings report and dropped what came
/// meanwhile, the turn's start and its approval with it, so the turn hung.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn shift_tab_as_a_goal_turn_starts_follows_that_turn() {
    let mut tui = TestTui::start(vec![
        tool_calls(&[("call_1", "Bash", asked_write("a.txt"))]),
        tool_calls(&[("call_2", "UpdateGoal", json!({"status": "complete"}))]),
        text("Goal reached."),
    ])
    .await;
    // Koda starts a turn on the goal while the TUI is idle...
    tui.ctx
        .client
        .request::<SessionGoalSet>(SessionGoalSetParams {
            session_id: tui.ctx.session.id.clone(),
            objective: Some("Write a.txt".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    tui.driver.model_call(1).await;
    // ...as Shift+Tab sends the mode.
    tui.ctx.mode = crate::mode::next(tui.ctx.mode);
    tui.ctx.send_mode().await;

    tui.run(async |driver: Driver| {
        let written = driver.project.path().join("a.txt");
        driver.approve_until(|| written.exists()).await;
        driver.wait_for_screen("Goal reached.").await;
        driver.wait_for_screen_without(BUSY_PROMPT).await;
        let screen = driver.screen();
        assert!(!screen.contains(NO_REPORT), "{screen}");
        driver
    })
    .await;
}

/// `/model` just as a turn starts that the TUI didn't start: the host
/// changes the model once the turn ends, and the status bar shows it then,
/// from the report nothing waits for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn a_model_change_as_a_turn_starts_shows_once_it_ends() {
    let server = model(vec![
        tool_calls(&[("call_1", "Bash", wait_for_go())]),
        text("Main done."),
    ])
    .await;
    server
        .mount_ok("GET", r".*/models$", models_listing("stub-picked"))
        .await;
    let mut tui = TestTui::start_on(server).await;
    start_turn(&tui.ctx, "first prompt").await;
    tui.driver.model_call(1).await;
    tui.ctx
        .change_model("stub-picked", ModelScope::Session)
        .await;

    tui.run(async |driver: Driver| {
        driver.wait_for_screen(BUSY_PROMPT).await;
        let screen = driver.screen();
        assert!(screen.contains("once Koda's running turn ends"), "{screen}");
        assert!(!status_bar(&screen).contains("stub-picked"), "{screen}");
        go(&driver);
        driver.wait_for_screen("Main done.").await;
        driver
            .wait_for_screen_that(
                |screen| status_bar(screen).contains("stub-picked"),
                "show the new model in the status bar",
            )
            .await;
        driver
    })
    .await;
}

/// `/compact` just as Koda starts a turn on the session: the TUI says so
/// and follows that turn, with its approval. The turn's start was dropped
/// before the compaction went out, and its approval while the TUI waited
/// for the compaction, which the host runs after the turn: both hung.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn compact_as_a_turn_starts_follows_that_turn() {
    let mut tui = TestTui::start(vec![
        tool_calls(&[("call_1", "Bash", asked_write("a.txt"))]),
        text("Main done."),
    ])
    .await;
    start_turn(&tui.ctx, "first prompt").await;
    // Its start is there for the TUI, not read yet, when `/compact` goes.
    let unread = read_until(&mut tui.ctx, is_turn_started).await;
    tui.ctx.held.extend(unread);
    let (tx, mut rx) = mpsc::channel(8);
    tokio::time::timeout(
        Duration::from_secs(20),
        tui.ctx
            .run_inference_turn(TurnRequest::Compact, &tx, &mut rx),
    )
    .await
    .expect("/compact waited on the turn")
    .unwrap();

    tui.run(async |driver: Driver| {
        driver.wait_for_screen("/compact once it ends").await;
        let written = driver.project.path().join("a.txt");
        driver.approve_until(|| written.exists()).await;
        driver.wait_for_screen("Main done.").await;
        driver.wait_for_screen_without(BUSY_PROMPT).await;
        driver
    })
    .await;
}

/// What the host sends while the TUI waits for a session's settings, as it
/// does at start-up, is handled afterwards: here the goal's report.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn what_comes_while_settings_are_fetched_is_kept() {
    let mut tui = TestTui::start(vec![]).await;
    let session = tui.ctx.session.id.clone();
    tui.ctx
        .client
        .request::<SessionGoalSet>(SessionGoalSetParams {
            session_id: session.clone(),
            objective: Some("Ship it".to_string()),
            status: Some(GoalStatus::Paused),
            ..Default::default()
        })
        .await
        .unwrap();
    let (settings, _) = fetch_settings(
        &mut tui.ctx.client,
        &mut tui.ctx.held,
        &session,
        tui.ctx.mode,
    )
    .await
    .unwrap();
    assert_eq!(settings.model, "stub-model");

    tui.run(async |driver: Driver| {
        driver.wait_for_screen("Goal (paused): Ship it").await;
        driver
    })
    .await;
}
