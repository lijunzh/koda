//! Choosing the provider, the model and a key.

use crossterm::event::KeyCode;
use koda_test_utils::openai::{chat_requests, model, text};

use super::{IDLE_HINT, esc, models_listing, picker_lines};
use crate::test_tui::{BUSY_PROMPT, Driver, TestTui, model_call_on};

/// A key typed into a wizard is taken with one Enter. The wizard's trail
/// box took the first Enter and dropped the key, so it needed two.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn a_wizard_takes_its_answer_with_one_enter() {
    let tui = TestTui::start(vec![]).await;
    let driver = tui
        .run(async |driver: Driver| {
            // `/key` asks for a key without switching provider, so nothing
            // calls an API.
            driver.submit("/key").await;
            driver.wait_for_screen("API key").await;
            driver.press(KeyCode::Enter);
            driver.wait_for_screen("API key for").await;
            driver.wait_for_screen("Paste or type it").await;
            driver.paste("sk-test-not-a-real-key");
            driver.press(KeyCode::Enter);
            driver.wait_for_screen("set to sk-t").await;
            driver
        })
        .await;
    // The host made the key this process's: take it back out, so no later
    // test in the process has it.
    let screen = driver.screen();
    let line = screen.lines().find(|l| l.contains(" set to sk-t")).unwrap();
    let env_key = line.split(" set to").next().unwrap();
    let env_key = env_key.split_whitespace().last().unwrap();
    koda_config::runtime_env::remove(env_key);
}

/// The provider picker lists, and switches to, the address Koda is running
/// with. It used the provider's default port, not the one given.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn the_provider_picker_uses_the_running_address() {
    let server = model(vec![text("All done.")]).await;
    server
        .mount_ok("GET", r".*/models$", models_listing("stub-picked"))
        .await;
    let tui = TestTui::start_on(server).await;
    tui.run(async |driver: Driver| {
        driver.submit("/provider").await;
        driver.wait_for_screen("Select a provider").await;
        // LM Studio, the provider in use, comes first.
        driver.press(KeyCode::Enter);
        driver.wait_for_screen("stub-picked").await;
        driver.press(KeyCode::Enter);
        driver.wait_for_screen("Model: stub-picked").await;

        driver.submit_and_finish("hi", "All done.").await;
        let sent = driver.model_call(1).await;
        assert!(sent.contains(r#""model":"stub-picked""#), "{sent}");
        driver
    })
    .await;
}

/// `s` in `/model` changes only this session, as Codex's does: its next
/// request asks for the model, and `config.toml` doesn't name it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn s_in_the_model_picker_is_this_session_only() {
    let server = model(vec![text("All done.")]).await;
    server
        .mount_ok("GET", r".*/models$", models_listing("stub-local"))
        .await;
    let tui = TestTui::start_on(server).await;
    let driver = tui
        .run(async |driver: Driver| {
            driver.submit("/model").await;
            driver.wait_for_screen("Select a model").await;
            driver.highlight("stub-local").await;
            driver.press(KeyCode::Char('s'));
            driver
                .wait_for_screen("Model: stub-local \u{00b7} this session")
                .await;
            // The input box is still empty: the `s` was the picker's.
            driver.wait_for_screen(IDLE_HINT).await;

            driver.submit_and_finish("which model", "All done.").await;
            let sent = driver.model_call(1).await;
            assert!(sent.contains(r#""model":"stub-local""#), "{sent}");
            driver
        })
        .await;
    let config = driver.config_dir().join("config.toml");
    let saved = std::fs::read_to_string(&config).unwrap_or_default();
    assert!(
        !saved.contains("stub-local"),
        "a session choice was saved: {saved}"
    );
}

/// `/model` lists the session's provider's models, as Codex's `/model`
/// lists its thread's: the local server's, and no other provider's alias.
/// Esc changes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn model_picker_lists_the_sessions_provider_and_esc_changes_nothing() {
    let server = model(vec![text("All done.")]).await;
    server
        .mount_ok("GET", r".*/models$", models_listing("stub-local"))
        .await;
    let tui = TestTui::start_on(server).await;
    tui.run(async |driver: Driver| {
        driver.submit("/model").await;
        driver.wait_for_screen("Select a model").await;
        driver.highlight("stub-local").await;
        let listed = picker_lines(&driver.screen(), "Select a model");
        assert!(!listed.contains("claude-haiku"), "{listed}");
        driver.key(esc());
        driver.wait_for_screen_without("Select a model").await;

        driver.submit_and_finish("hi", "All done.").await;
        let sent = driver.model_call(1).await;
        assert!(sent.contains(r#""model":"stub-model""#), "{sent}");
        driver
    })
    .await;
}

/// `/provider <url>` moves the session to the server at that address: the
/// next request goes there, not to the one Koda started with.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn provider_with_an_address_moves_the_next_turn_there() {
    let first = model(vec![]).await;
    let second = model(vec![text("From the second server.")]).await;
    second
        .mount_ok("GET", r".*/models$", models_listing("stub-model"))
        .await;
    let tui = TestTui::start_on(first).await;
    tui.run(async |driver: Driver| {
        driver.submit(&format!("/provider {}", second.url())).await;
        // A session's provider and server don't change: this is a new one.
        driver.wait_for_screen("New session on").await;
        driver.submit("where are you").await;
        let sent = model_call_on(&second, 1).await;
        assert!(sent.contains("where are you"), "{sent}");
        driver.wait_for_screen("From the second server.").await;
        assert!(
            chat_requests(&driver.server).await.is_empty(),
            "the first server still got the turn"
        );
        driver.wait_for_screen_without(BUSY_PROMPT).await;
        driver
    })
    .await;
}
