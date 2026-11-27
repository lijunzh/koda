//! The TUI end to end, in this process ([`crate::test_tui`]): keys and slash
//! commands reach the host and what comes back is drawn. Moved here from
//! koda-cli's PTY suite, which keeps what only a real terminal shows.

mod models;
mod plan;
mod sessions;
mod stops;
mod sub_agents;

use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use koda_test_utils::network::ResponseTemplate;
use serde_json::{Value, json};

use crate::test_tui::Driver;

/// What the model is told about a tool call the stop kept from running.
const STOPPED: &str = "The user stopped the turn before this ran.";

/// Text the input box shows when it is empty and the TUI is idle.
const IDLE_HINT: &str = "Type a message";

fn esc() -> KeyEvent {
    KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

/// The arguments of a `Bash` call writing `name` that asks first: it asks to
/// leave the sandbox, the call that asks in Auto.
fn asked_write(name: &str) -> Value {
    json!({"command": format!("printf 'x\\n' > {name}"), "outside_sandbox": true, "reason": "test"})
}

/// A plain JSON reply, for the calls Koda doesn't stream (sub-agents).
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

/// A local server's model list, naming `id`.
fn models_listing(id: &str) -> Value {
    json!({"object": "list", "data": [
        {"id": id, "object": "model", "type": "llm", "state": "loaded"}
    ]})
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

/// No more model calls come within a couple of seconds: nothing started a
/// turn on its own. A turn nobody asked for has no sign to wait on, so this
/// waits out a quiet spell, as the PTY test did.
async fn assert_no_more_calls(driver: &Driver, calls: usize) {
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        driver.model_calls().await,
        calls,
        "a turn started that nobody asked for"
    );
}

/// Whether a process whose command line matches `pattern` is running.
fn running(pattern: &str) -> bool {
    std::process::Command::new("pgrep")
        .args(["-f", pattern])
        .output()
        .is_ok_and(|o| !o.stdout.is_empty())
}

/// The lines of the screen from the open picker's title down.
fn picker_lines(screen: &str, title: &str) -> String {
    screen
        .lines()
        .skip_while(|line| !line.contains(title))
        .collect::<Vec<_>>()
        .join("\n")
}
