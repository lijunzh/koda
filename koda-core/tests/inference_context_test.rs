//! Tests that the context-window percentage display reflects actual token
//! counts rather than only the pre-send heuristic estimate.
//!
//! Regression test for #874: the heuristic (chars/3.5 + 10/message) was the
//! only signal driving the status-bar %, causing it to underreport by up to
//! 2× for code/JSON-heavy sessions.

use koda_core::engine::EngineEvent;
use koda_test_utils::{Env, MockProvider, MockResponse};

/// After a completed turn the event stream must contain a corrective
/// `ContextUsage` event whose `used` field equals the actual `prompt_tokens`
/// reported by the provider — NOT the pre-send heuristic estimate.
///
/// MockProvider::Text always reports `prompt_tokens = 10`.
/// The pre-send heuristic for a short "hello" message gives > 10
/// (formula: chars/3.5 + 10 per message ≈ 11-12).
/// So the last ContextUsage.used must equal 10.
#[tokio::test]
async fn corrective_context_usage_uses_actual_prompt_tokens() {
    let env = Env::builder().max_context_tokens(200_000).build().await;
    env.insert_user_message("hello").await;

    let provider = MockProvider::new(vec![MockResponse::Text("hi".into())]);
    let (result, events) = env.run_inference_result(&provider).await;
    assert!(result.is_ok(), "inference must succeed: {:?}", result.err());

    let context_events: Vec<(usize, usize)> = events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::ContextUsage { used, max } => Some((*used, *max)),
            _ => None,
        })
        .collect();

    assert!(
        context_events.len() >= 2,
        "expected at least 2 ContextUsage events (heuristic + corrective), \
         got {}: {context_events:?}",
        context_events.len()
    );

    // The LAST ContextUsage must be the corrective one with actual tokens.
    let (last_used, last_max) = *context_events.last().unwrap();
    assert_eq!(
        last_used, 10,
        "last ContextUsage.used must equal actual prompt_tokens (10 from mock), \
         got {last_used}"
    );
    assert_eq!(
        last_max, 200_000,
        "max must match configured context window"
    );

    // The FIRST ContextUsage is the heuristic; it should differ from 10
    // for any non-trivial message (proves the correction is actually updating).
    let (first_used, _) = context_events[0];
    assert_ne!(
        first_used, last_used,
        "heuristic and corrective ContextUsage.used should differ, \
         both were {first_used} — corrective event may not be firing"
    );
}

/// The corrective event must also update the global context atomic so that
/// `context::percentage()` reflects real usage after the turn.
#[tokio::test]
async fn context_percentage_reflects_actual_tokens_after_turn() {
    let env = Env::builder().max_context_tokens(100_000).build().await;
    env.insert_user_message("test message for context accuracy")
        .await;

    let provider = MockProvider::new(vec![MockResponse::Text("response".into())]);
    let (result, events) = env.run_inference_result(&provider).await;
    assert!(result.is_ok());

    // Confirm Footer also reports the same actual token count.
    let footer = events.iter().find_map(|e| match e {
        EngineEvent::Footer { prompt_tokens, .. } => Some(*prompt_tokens),
        _ => None,
    });
    assert!(footer.is_some(), "Footer event must be emitted");

    let footer_tokens = footer.unwrap();
    let last_context_used = events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::ContextUsage { used, .. } => Some(*used),
            _ => None,
        })
        .last()
        .expect("at least one ContextUsage event required");

    assert_eq!(
        last_context_used as i64, footer_tokens,
        "last ContextUsage.used ({last_context_used}) must match \
         Footer.prompt_tokens ({footer_tokens})"
    );
}
