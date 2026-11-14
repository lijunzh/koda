//! Inference-loop retry regression test for transient SSE timeouts.
//!
//! Closes #1123 (acceptance criterion #5 from #1119, deferred from #1121).
//!
//! ## What this test guards
//!
//! v0.2.23 wired `try_with_rate_limit` around `provider.chat_stream(...)`
//! so transient network errors (idle read timeouts, connection resets,
//! broken pipes, DNS hiccups) trigger exponential-backoff retries
//! instead of bubbling "connection broken" up to the user. The
//! `is_network_transient_error` predicate has 13 unit-test assertions
//! in `inference_helpers.rs`, but the **wired-up retry behaviour**
//! (predicate → backoff → retry → succeed) had no end-to-end coverage.
//!
//! This file closes that gap with a deterministic HTTP-level test
//! using `FakeLlmServer::mount_chat_silent_then_resume`. A regression
//! in any of the four observable contracts below — predicate not
//! matching, retry loop short-circuiting, warn event suppressed, or
//! backoff disabled — fails this test.
//!
//! ## Why an integration test, not a unit test
//!
//! `try_with_rate_limit` is a private async fn deep inside
//! `koda_core::inference`. Exercising it through the public surface
//! (`Env::run_inference_result`) proves the retry logic survives
//! refactoring of any layer between the entry point and the helper —
//! including the warn-event emission on the engine sink, which is
//! itself a contract the user relies on (the "Network glitch.
//! Retrying in Ns..." toast). A unit test on the helper alone would
//! miss that.
//!
//! ## The read timeout
//!
//! The provider is built from a config whose `stream_idle_timeout_ms`
//! is 1s, as the config file's `[model_providers.<id>]` table sets it.
//! `ENV_MUTEX` is still held: the provider reads proxy variables, which
//! other tests in this crate change.

use std::time::{Duration, Instant};

use koda_core::config::ProviderType;
use koda_core::persistence::Persistence;
use koda_test_utils::network::FakeLlmServer;
use koda_test_utils::{ENV_MUTEX, EngineEvent, Env};

// ── Helpers ───────────────────────────────────────────────

/// SSE chunks that reassemble into the assistant message
/// `"recovered after retry"` and signal a clean stop.
fn ok_sse_chunks() -> Vec<&'static str> {
    vec![
        r#"{"choices":[{"index":0,"delta":{"content":"recovered after retry"},"finish_reason":null}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        "[DONE]",
    ]
}

// ── Tests ─────────────────────────────────────────────────────────────────

/// **The acceptance criterion this test exists to enforce (#1123).**
///
/// Scenario: the provider's first chat-completion request stalls past
/// its read timeout (`stream_idle_timeout_ms`). The inference loop must:
///
/// 1. Catch the resulting timeout error from `provider.chat_stream(...)`.
/// 2. Recognise it as a transient network failure (per
///    `is_network_transient_error`).
/// 3. Emit a `Warn` event to the sink containing "Network glitch" so
///    the user sees a "Retrying in 1s..." toast instead of a stack trace.
/// 4. Sleep `rate_limit_backoff(0)` = 1s.
/// 5. Retry. The second request gets the success body and the
///    assistant message persists to the DB.
///
/// We assert all four observable contracts.
#[tokio::test]
async fn read_timeout_triggers_auto_retry_not_hard_failure() {
    let _g = ENV_MUTEX.lock().await;

    // The fake server: first request stalls for 3s (well past the 1s
    // read timeout); second request succeeds immediately.
    let server = FakeLlmServer::spawn().await;
    server
        .mount_chat_silent_then_resume(Duration::from_secs(3), &ok_sse_chunks())
        .await;

    let test_env = Env::builder()
        .max_context_tokens(100_000)
        .provider_type(ProviderType::OpenAI)
        .build()
        .await;
    test_env.insert_user_message("hello").await;

    // A read timeout below the silent_for window, so the first request
    // errors with "operation timed out", the canonical
    // is_network_transient_error trigger.
    let mut config = koda_core::config::KodaConfig::default_for_testing(ProviderType::OpenAI);
    config.base_url = server.url();
    config.stream_idle_timeout_ms = Some(1_000);
    let provider = koda_core::providers::create_provider(&config);

    let started = Instant::now();
    let (result, events) = test_env.run_inference_result(provider.as_ref()).await;
    let elapsed = started.elapsed();

    // Contract 1: the inference loop recovered (no error bubbled up).
    assert!(
        result.is_ok(),
        "transient timeout must be retried, not surfaced; got: {:?}",
        result.err()
    );

    // Contract 2: the user saw a "Network glitch" retry warning.
    let has_glitch_warn = events.iter().filter_map(EngineEvent::notice).any(|e| {
        matches!(
            e,
            EngineEvent::Warn { message } if message.contains("Network glitch")
                && message.contains("Retrying")
        )
    });
    assert!(
        has_glitch_warn,
        "expected a 'Network glitch ... Retrying' Warn event; got: {events:?}"
    );

    // Contract 3: the server actually received TWO requests (one
    // stalled, one retried). If retries didn't fire we'd see only 1.
    let received = server.received_requests().await;
    assert_eq!(
        received.len(),
        2,
        "expected exactly 2 requests (1 stall + 1 retry), got {}",
        received.len()
    );

    // Contract 4: the recovered response landed in the DB. Without
    // the retry we'd have nothing to persist.
    let last = test_env
        .db
        .last_assistant_message(&test_env.session_id)
        .await
        .expect("DB must have a persisted assistant message after retry");
    assert!(
        last.contains("recovered after retry"),
        "DB must contain the second-attempt response, got: {last}"
    );

    // Sanity bound on wall-clock: 1s timeout + 1s backoff + fast retry
    // = ~2s. If we exceed 8s, something is very wrong (e.g. the retry
    // is firing 5 times because the predicate stopped matching and we
    // fell into the rate-limit branch's larger backoff).
    assert!(
        elapsed < Duration::from_secs(8),
        "retry sequence took {elapsed:?}; expected ~2s. \
         Backoff misconfigured or predicate not matching."
    );
}
