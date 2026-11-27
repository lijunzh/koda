//! Plan mode: a proposed plan, and what the menu after it does.

use crossterm::event::KeyCode;
use koda_test_utils::network::ResponseTemplate;
use koda_test_utils::openai::text;

use crate::test_tui::{BUSY_PROMPT, Driver, TestTui, input_box};

/// A reply that proposes a plan, as Plan's instructions ask.
fn proposed_plan() -> ResponseTemplate {
    text("Here is the plan.\n\n<proposed_plan>\n# Greeting\n- Add a hello line\n</proposed_plan>\n")
}

/// `/plan <prompt>` enters Plan and sends the prompt, whose turn tells the
/// model it plans; a proposed plan is drawn framed, and "Implement this
/// plan?" switches to Safe and sends "Implement the plan.", as Codex's TUI
/// does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn plan_then_implement_it_in_safe() {
    use koda_state::session_store::SessionStore;
    let tui = TestTui::start_safe(vec![proposed_plan(), text("All done.")]).await;
    let driver = tui
        .run(async |driver: Driver| {
            driver.wait_for_screen("SAFE").await;
            driver.submit("/plan add a greeting").await;
            driver.wait_for_screen("Implement this plan?").await;
            let screen = driver.screen();
            assert!(screen.contains("PLAN"), "{screen}");
            assert!(screen.contains("Proposed plan"), "{screen}");
            assert!(!screen.contains("<proposed_plan>"), "{screen}");
            assert!(screen.contains("No, keep planning"), "{screen}");
            let planning = driver.model_call(1).await;
            assert!(planning.contains("add a greeting"), "{planning}");
            assert!(planning.contains("plan mode"), "{planning}");
            assert!(planning.contains("<proposed_plan>"), "{planning}");

            // The first choice: implement it in Safe.
            driver.press(KeyCode::Enter);
            driver.wait_for_screen("All done.").await;
            let implementing = driver.model_call(2).await;
            assert!(
                implementing.contains("Implement the plan."),
                "{implementing}"
            );
            assert!(
                !implementing.contains("Read-only session"),
                "still told it plans: {implementing}"
            );
            driver.wait_for_screen("SAFE").await;
            driver.wait_for_screen_without(BUSY_PROMPT).await;
            driver
        })
        .await;
    let session = driver.sessions().await.remove(0);
    let mode = driver.db().await.get_session_mode(&session).await.unwrap();
    assert_eq!(mode.as_deref(), Some("safe"));
}

/// Typing while "Implement this plan?" is up is a reply: the menu goes,
/// the text is sent, and the session stays in Plan.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn typing_a_reply_keeps_planning() {
    let tui = TestTui::start_safe(vec![proposed_plan(), text("All done.")]).await;
    tui.run(async |driver: Driver| {
        driver.wait_for_screen("SAFE").await;
        driver.submit("/plan add a greeting").await;
        driver.wait_for_screen("Implement this plan?").await;
        driver.type_keys("use French");
        driver
            .wait_for_screen_that(
                |screen| input_box(screen).ends_with("useFrench"),
                "show the reply in the input box",
            )
            .await;
        driver.press(KeyCode::Enter);
        driver.wait_for_screen("All done.").await;
        let reply = driver.model_call(2).await;
        assert!(reply.contains("use French"), "{reply}");
        assert!(reply.contains("plan mode"), "{reply}");
        let screen = driver.screen();
        assert!(screen.contains("PLAN"), "{screen}");
        assert!(!screen.contains("Implement this plan?"), "{screen}");
        driver.wait_for_screen_without(BUSY_PROMPT).await;
        driver
    })
    .await;
}
