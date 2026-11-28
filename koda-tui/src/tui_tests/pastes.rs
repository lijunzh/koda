//! Pastes: where one lands among text typed just before it.

use std::time::Instant;

use koda_test_utils::openai::{text, tool_calls};

use super::{ctrl, go, start_turn, wait_for_go};
use crate::test_tui::{BUSY_PROMPT, Driver, TestTui, input_box};

/// Put `typed` where fast typing leaves it for a moment: held as a possible
/// paste (the burst), not yet in the input box. Its clock isn't started, so
/// it stays held until something flushes it, however slow the runner.
fn hold(tui: &mut TestTui, typed: &str) {
    tui.ctx
        .paste_burst
        .begin_with_retro_grabbed(typed.to_string(), Instant::now());
}

/// A paste lands after text typed just before it, as text or as a marker.
/// The typed text was still held as a possible paste, and the paste went
/// into the box ahead of it: "why does " then a paste gave the marker first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn a_paste_lands_after_text_typed_just_before_it() {
    let mut tui = TestTui::start(vec![]).await;
    let large = "a line of the log\n".repeat(40);
    for (pasted, expected) in [
        ("this fail?", "why does this fail?"),
        (large.as_str(), "why does [Pasted text #1 +40 lines]"),
    ] {
        tui.ctx.textarea.set_text_clearing_elements("");
        hold(&mut tui, "why does ");
        tui.ctx.handle_idle_paste(pasted);
        assert_eq!(tui.ctx.textarea.text(), expected, "pasting {pasted:?}");
        assert!(
            !tui.ctx.paste_burst.is_active(),
            "a paste ends the burst before it"
        );
    }
}

/// Text typed just as Koda starts a turn by itself is in the input box
/// during that turn, ahead of what is pasted then. The turn's keys and
/// pastes go straight into the box, and the held text stayed out of it
/// until the turn ended, then landed after them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn text_held_as_a_turn_starts_stays_ahead_of_a_paste_during_it() {
    // The turn runs a command that waits for the test's `go`, so it lasts
    // until the paste has been checked.
    let mut tui = TestTui::start(vec![
        tool_calls(&[("call_1", "Bash", wait_for_go())]),
        text("All done."),
    ])
    .await;
    // Koda starts a turn; its start hasn't been read when the text is typed.
    start_turn(&tui.ctx, "first prompt").await;
    tui.driver.model_call(1).await;
    hold(&mut tui, "held text ");

    tui.run(async |driver: Driver| {
        driver.wait_for_screen(BUSY_PROMPT).await;
        driver.paste("pasted");
        driver
            .wait_for_screen_that(
                |screen| input_box(screen).ends_with("heldtextpasted"),
                "show the held text, then the paste, in the input box",
            )
            .await;
        go(&driver);
        driver.wait_for_screen("All done.").await;
        driver.wait_for_screen_without(BUSY_PROMPT).await;
        // Empty the box for the Ctrl+D that ends the run.
        driver.key(ctrl('u'));
        driver
            .wait_for_screen_that(|s| !input_box(s).contains("pasted"), "empty the box")
            .await;
        driver
    })
    .await;
}
