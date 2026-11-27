//! Sub-agents: their requests in the main session, and stopping one.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crossterm::event::KeyCode;
use koda_test_utils::network::{FakeLlmServer, ResponseTemplate};
use koda_test_utils::openai::{text, tool_calls};
use serde_json::{Value, json};

use super::{asked_write, assert_no_more_calls, assert_stopped, esc, json_reply};
use crate::test_tui::{BUSY_PROMPT, Driver, TestTui};

/// A sub-agent's reply, which doesn't stream, that calls one tool.
fn sub_agent_call(name: &str, args: Value) -> ResponseTemplate {
    let call = json!({
        "id": "sub_call",
        "type": "function",
        "function": {"name": name, "arguments": args.to_string()},
    });
    ResponseTemplate::new(200).set_body_json(json!({
        "choices": [{
            "message": {"role": "assistant", "tool_calls": [call]},
            "finish_reason": "stop",
        }],
    }))
}

/// The main agent's streamed replies in order, then "All done."; a
/// sub-agent's replies (which don't stream) in order, then "Sub-agent
/// done.".
async fn model_with_a_sub_agent(
    main: Vec<ResponseTemplate>,
    sub: Vec<ResponseTemplate>,
) -> FakeLlmServer {
    let server = FakeLlmServer::spawn().await;
    let (main_next, sub_next) = (AtomicUsize::new(0), AtomicUsize::new(0));
    server
        .mount_fn("POST", r".*/chat/completions$", move |request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap_or_default();
            if body["stream"] == json!(true) {
                return main
                    .get(main_next.fetch_add(1, Ordering::SeqCst))
                    .cloned()
                    .unwrap_or_else(|| text("All done."));
            }
            sub.get(sub_next.fetch_add(1, Ordering::SeqCst))
                .cloned()
                .unwrap_or_else(|| {
                    ResponseTemplate::new(200).set_body_json(json!({
                        "choices": [{
                            "message": {"role": "assistant", "content": "Sub-agent done."},
                            "finish_reason": "stop",
                        }],
                    }))
                })
        })
        .await;
    server
}

/// The main agent hands `args` to a sub-agent.
fn delegate(args: Value) -> ResponseTemplate {
    tool_calls(&[("call_0", "InvokeAgent", args)])
}

/// A model whose main agent has the `task` sub-agent write `a.txt`, in the
/// background if `background`. A background agent asks a second later,
/// once the turn that started it has ended.
async fn a_sub_agent_writes_a_file(background: bool) -> FakeLlmServer {
    let mut args = json!({"agent_name": "task", "prompt": "write a.txt"});
    let mut write = sub_agent_call("Bash", asked_write("a.txt"));
    if background {
        args["run_in_background"] = json!(true);
        write = write.set_delay(Duration::from_secs(1));
    }
    model_with_a_sub_agent(vec![delegate(args)], vec![write]).await
}

/// A sub-agent asks in the main session, as in Claude Code and Codex: the
/// request names it, and `y` lets its write run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn a_sub_agents_request_names_it_and_y_approves_it() {
    let tui = TestTui::start_on(a_sub_agent_writes_a_file(false).await).await;
    tui.run(async |driver: Driver| {
        driver.submit("first prompt").await;
        driver.wait_for_screen("task asks").await;
        let screen = driver.screen();
        assert!(screen.contains("[esc] reject"), "{screen}");
        driver.press(KeyCode::Char('y'));
        driver.wait_for_screen("All done.").await;
        assert!(
            driver.project.path().join("a.txt").exists(),
            "the approved write didn't run"
        );
        driver.wait_for_screen_without(BUSY_PROMPT).await;
        driver
    })
    .await;
}

/// Esc on a sub-agent's request refuses that one call; the sub-agent and the
/// turn go on (Claude Code).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn esc_refuses_a_sub_agents_call_and_the_turn_goes_on() {
    let tui = TestTui::start_on(a_sub_agent_writes_a_file(false).await).await;
    tui.run(async |driver: Driver| {
        driver.submit("first prompt").await;
        driver.wait_for_screen("task asks").await;
        driver.key(esc());
        driver.wait_for_screen("All done.").await;
        assert!(
            !driver.project.path().join("a.txt").exists(),
            "the refused write ran"
        );
        let sub_agent_heard = driver.model_call(3).await;
        assert!(
            sub_agent_heard.contains("User rejected this action."),
            "{sub_agent_heard}"
        );
        driver.wait_for_screen_without(BUSY_PROMPT).await;
        driver
    })
    .await;
}

/// A background agent may ask after the turn that started it has ended: the
/// idle TUI shows its request, and `y` answers it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn a_background_agent_asks_while_the_tui_is_idle() {
    let tui = TestTui::start_on(a_sub_agent_writes_a_file(true).await).await;
    tui.run(async |driver: Driver| {
        driver.submit("first prompt").await;
        driver.wait_for_screen("asks").await;
        let screen = driver.screen();
        assert!(
            screen.contains("All done.") && !screen.contains(BUSY_PROMPT),
            "asked before the turn ended: {screen}"
        );
        assert!(
            screen.contains("task a"),
            "names the agent and its id: {screen}"
        );
        assert!(!screen.contains("[f] feedback"), "{screen}");
        driver.press(KeyCode::Char('y'));
        let written = driver.project.path().join("a.txt");
        driver
            .wait_until(|| written.exists(), "the approved write runs")
            .await;
        driver
    })
    .await;
}

/// Esc while a sub-agent works stops the turn and the sub-agent, and nothing
/// starts another turn. The sub-agent's last status reaches the TUI after
/// the turn has stopped, which the TUI reads at idle; a finished
/// background agent used to start a turn there (#1349).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn esc_during_a_sub_agent_starts_no_other_turn() {
    let invoke = tool_calls(&[(
        "call_0",
        "InvokeAgent",
        json!({"agent_name": "explore", "prompt": "look around"}),
    )]);
    // Sub-agents call the model without streaming.
    let slow = json_reply("too late").set_delay(Duration::from_secs(10));
    let tui = TestTui::start(vec![invoke, slow]).await;
    tui.run(async |driver: Driver| {
        driver.submit("first prompt").await;
        let sent = driver.model_call(2).await;
        assert!(
            sent.contains("look around"),
            "not the sub-agent's call: {sent}"
        );
        driver.stop_turn(esc()).await;

        assert_no_more_calls(&driver, 2).await;
        assert_stopped(&driver, 2).await;
        driver
    })
    .await;
}
