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
        .next_back()
        .expect("at least one ContextUsage event required");

    assert_eq!(
        last_context_used as i64, footer_tokens,
        "last ContextUsage.used ({last_context_used}) must match \
         Footer.prompt_tokens ({footer_tokens})"
    );
}

/// Regression test for #946: the context-usage meter must NOT exceed 100%
/// on multi-iteration turns. Each iteration's `prompt_tokens` reports the
/// full prompt size (not a delta), so summing across iterations double-counts
/// the shared history. The corrective `ContextUsage` must reflect the *last*
/// iteration's `prompt_tokens` (current context-window occupancy), while the
/// `Footer` keeps the cumulative sum (billing/telemetry).
///
/// Three-iteration turn (tool_call → tool_call → text), MockProvider reports
/// `prompt_tokens: 10` per iteration:
///   - last_prompt_tokens = 10 — single prompt's footprint  → ContextUsage
///   - total_prompt_tokens = 30 — cumulative billing for the turn → Footer
///
/// Pre-fix bug: ContextUsage.used was 30, exceeding `max_context_tokens = 25`
/// and rendering as 120% in the status bar. Post-fix: 10/25 = 40%.
#[tokio::test]
async fn context_meter_uses_last_iteration_not_sum() {
    // Tight max so the bug would manifest as a clear >100% overflow.
    let env = Env::builder().max_context_tokens(25).build().await;
    env.insert_user_message("do two things then summarise")
        .await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call("Bash", serde_json::json!({"command": "echo one"})),
        MockResponse::tool_call("Bash", serde_json::json!({"command": "echo two"})),
        MockResponse::Text("Both done.".into()),
    ]);
    let (result, events) = env.run_inference_result(&provider).await;
    assert!(result.is_ok(), "inference must succeed: {:?}", result.err());

    let last_context_used = events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::ContextUsage { used, .. } => Some(*used),
            _ => None,
        })
        .next_back()
        .expect("at least one ContextUsage event required");

    let footer_tokens = events
        .iter()
        .find_map(|e| match e {
            EngineEvent::Footer { prompt_tokens, .. } => Some(*prompt_tokens),
            _ => None,
        })
        .expect("Footer event must be emitted");

    // The corrective meter must reflect the LAST iteration's prompt size.
    assert_eq!(
        last_context_used, 10,
        "ContextUsage.used must be the last iteration's prompt_tokens (10), \
         not the cumulative sum — got {last_context_used} (#946)"
    );

    // The Footer must still report the cumulative sum (3 iterations × 10 = 30).
    assert_eq!(
        footer_tokens, 30,
        "Footer.prompt_tokens must remain the cumulative sum across iterations \
         (3 iterations × 10 = 30), got {footer_tokens}"
    );

    // The two values MUST diverge for multi-iteration turns — that's the whole
    // point of the fix. If they're equal, the meter is still summing.
    assert_ne!(
        last_context_used as i64, footer_tokens,
        "ContextUsage.used and Footer.prompt_tokens must diverge on \
         multi-iteration turns (they're measuring different things)"
    );

    // The headline regression: meter must never exceed 100% under normal use.
    // Pre-fix this would be 30/25 = 120%.
    assert!(
        last_context_used <= 25,
        "context meter must never exceed max_context_tokens — \
         got {last_context_used}/25 (#946 regression)"
    );
}
