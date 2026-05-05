//! E2E tests for the context-window management pipeline.
//!
//! Closes priority 4 of #1264 ("Context Window Management — High").
//! The existing `inference_context_test.rs` covers display accuracy
//! only (the `ContextUsage`/`Footer` event emission, see #874 / #946).
//! This file covers the *behavior* gaps the issue called out:
//!
//! - Pre-flight compaction triggering when the context-usage gauge
//!   exceeds [`AUTO_COMPACT_THRESHOLD`] (85%).
//! - Pre-flight compaction being skipped under the threshold.
//! - Overflow recovery: provider returns a "context too long" error →
//!   `try_overflow_recovery` runs compaction → retries the chat call →
//!   succeeds on the second attempt.
//! - Overflow recovery bubbling up when the session is too short for
//!   compaction to do anything (`CompactSkip::TooShort`).
//!
//! ## Anatomy of a compaction round-trip in test land
//!
//! Compaction makes its *own* LLM call (the summarization prompt — see
//! `compact::compact_session_with_provider`). `MockProvider::chat` and
//! `chat_stream` consume from the same queued response vector, so the
//! ordering of `MockResponse` entries matters:
//!
//! ```text
//! Pre-flight path:
//!   [0] Text("SUMMARY")     ← compact_session_with_provider via .chat()
//!   [1] Text("answer")      ← inference loop via .chat_stream()
//!
//! Overflow recovery path:
//!   [0] ContextOverflow     ← initial .chat_stream() rejected
//!   [1] Text("SUMMARY")     ← compact via .chat()
//!   [2] Text("answer")      ← retried .chat_stream()
//! ```
//!
//! ## Global-state hazard
//!
//! `compact::CONSECUTIVE_FAILURES` is a process-global `AtomicU32`
//! (the auto-compact circuit breaker — three failures in a row trips
//! it). Cargo runs tests in parallel inside one process, so any test
//! that triggers compaction must (a) call [`reset_compact_failures`]
//! at the top to neutralize state left behind by a prior failure AND
//! (b) hold [`COMPACT_SERIAL`] for the duration so a *concurrent*
//! test can't trip the breaker between the reset and the assertion.
//! The `compact.rs` test module already had to delete duplicate
//! breaker tests for this exact reason — see the comment near
//! `compact.rs:723`. We do *not* test the breaker tripping here for
//! the same reason; the existing `compact::test_circuit_breaker`
//! covers the state machine adequately.

use koda_core::compact::reset_compact_failures;
use koda_core::engine::EngineEvent;
use koda_core::persistence::Role;
use koda_test_utils::{Env, MockProvider, MockResponse};
use std::sync::LazyLock;
use tokio::sync::{Mutex, MutexGuard};

// ── Helpers ──────────────────────────────────────────────────

/// File-scoped serialization mutex. Every test that exercises a code
/// path touching `compact::CONSECUTIVE_FAILURES` (i.e. anything that
/// can call `record_compact_failure` or `is_compact_circuit_broken`)
/// must hold this lock for its full duration. See the module-level
/// "Global-state hazard" note for the why.
///
/// Uses `tokio::sync::Mutex` (not `std::sync::Mutex`) because the
/// guard is held across `.await` points — clippy's
/// `await_holding_lock` lint correctly forbids the std variant in
/// that situation. Implemented inline rather than via the
/// `serial_test` crate to avoid pulling in a dev-dependency for one
/// file.
static COMPACT_SERIAL: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Acquire [`COMPACT_SERIAL`] and reset the circuit breaker. Returns
/// the guard so the caller's test holds the lock for its whole body.
async fn lock_compact_state() -> MutexGuard<'static, ()> {
    let guard = COMPACT_SERIAL.lock().await;
    reset_compact_failures();
    guard
}

// ── Helpers ──────────────────────────────────────────────────

/// Stuff `n` user/assistant message pairs into the test session DB so
/// the heuristic token estimate inside `assemble_context` lands above
/// [`AUTO_COMPACT_THRESHOLD`] for the configured `max_context_tokens`.
///
/// The inference loop reads the message count *and* a final user
/// message to respond to, so callers should pass an even count (pairs)
/// and then add one more user message before `run_inference`.
async fn fill_history_with_pairs(env: &Env, n: usize, padding: &str) {
    for i in 0..n {
        env.insert_message(&Role::User, &format!("user msg {i}: {padding}"))
            .await;
        env.insert_message(&Role::Assistant, &format!("asst reply {i}: {padding}"))
            .await;
    }
}

/// Did the engine emit the pre-flight compaction "📦 Context at X% —
/// compacting before sending..." Info event? Matches by substring so
/// the percentage value doesn't have to be predicted exactly.
fn saw_preflight_compact_start(events: &[EngineEvent]) -> bool {
    events.iter().any(|e| match e {
        EngineEvent::Info { message } => message.contains("compacting before sending"),
        _ => false,
    })
}

/// Did the engine emit the post-success "✅ Compacted N messages..."
/// Info event? Matches both the pre-flight ("✅ Compacted N messages
/// (~M token summary)") and the overflow-recovery ("✅ Compacted N
/// messages. Retrying...") variants.
fn saw_compact_success(events: &[EngineEvent]) -> bool {
    events.iter().any(|e| match e {
        EngineEvent::Info { message } => {
            message.contains("Compacted") && message.contains("messages")
        }
        _ => false,
    })
}

/// Did the engine emit the overflow-recovery "⚠️ Provider rejected
/// request (context overflow)..." Warn event? This is the canary that
/// `try_overflow_recovery` was actually entered.
fn saw_overflow_recovery_warn(events: &[EngineEvent]) -> bool {
    events.iter().any(|e| match e {
        EngineEvent::Warn { message } => message.contains("context overflow"),
        _ => false,
    })
}

// ── Tests ────────────────────────────────────────────────────

/// Pre-flight path: when the heuristic context gauge sits at or above
/// [`AUTO_COMPACT_THRESHOLD`] (85%) before sending the next prompt,
/// `inference_loop` runs `compact_session_with_provider` *before* the
/// chat call. The user sees a "📦 Context at X% — compacting..." Info
/// event followed by "✅ Compacted N messages...".
///
/// Verifies:
/// - The compact-start Info event fires.
/// - The compact-success Info event fires.
/// - The chat-stream call eventually completes successfully.
#[tokio::test]
async fn preflight_compact_fires_above_85pct_threshold() {
    let _serial = lock_compact_state().await;

    // ── Sizing ───────────────────────────────────────────────
    // `compact_session_with_provider` reserves 4096 tokens for the
    // summary output (`available = max - 4096`), so `max` has to
    // sit comfortably above that *and* the heuristic estimate has to
    // hit ≥85% of `max`. Math (CHARS_PER_TOKEN = 3.5,
    // PER_MESSAGE_OVERHEAD = 10):
    //   - max               = 8000  → threshold trigger at ≥6800 tokens
    //   - history pairs     = 7     → 14 history msgs + 1 trailing user = 15
    //   - chars/msg         = 1700  → estimated = 15*(486+10) ≈ 7440 (>6800 ✓)
    //   - compact_count     = 7     → partial-compact picks oldest half
    //   - conversation_text ≈ 7*1700 = 11_900 chars → ~3500 tokens
    //     (well under the 8000-4096 = 3904 available, so HistoryTooLarge
    //     is avoided ✓)
    let env = Env::builder().max_context_tokens(8_000).build().await;
    let padding = "x".repeat(1700);
    fill_history_with_pairs(&env, 7, &padding).await;
    env.insert_user_message("now answer the real question")
        .await;

    // [0] = compaction summary (consumed by .chat())
    // [1] = real answer (consumed by .chat_stream() after compact returns)
    let provider = MockProvider::new(vec![
        MockResponse::Text("SUMMARY: prior conversation about padded messages".into()),
        MockResponse::Text("Here is the real answer.".into()),
    ]);

    let (result, events) = env.run_inference_result(&provider).await;
    assert!(
        result.is_ok(),
        "inference should succeed: {:?}",
        result.err()
    );

    assert!(
        saw_preflight_compact_start(&events),
        "expected '📦 Context at X% — compacting...' Info event; events: {events:?}"
    );
    assert!(
        saw_compact_success(&events),
        "expected '✅ Compacted N messages...' Info event; events: {events:?}"
    );
}

/// Negative: when the gauge sits below the 85% threshold, the
/// pre-flight path is a no-op. No compaction events fire and the
/// provider sees the original message list.
///
/// This is the partner of the above test — both use the same fixture
/// shape; only `max_context_tokens` differs. A tight max trips the
/// threshold; a generous max stays safe.
#[tokio::test]
async fn preflight_compact_skipped_below_85pct_threshold() {
    let _serial = lock_compact_state().await;

    // Same shape as the trigger test (7 pairs + 1 trailing user, ~1700
    // chars each ≈ 7440 estimated tokens) but max=200_000 → ~4% usage,
    // far below threshold.
    let env = Env::builder().max_context_tokens(200_000).build().await;
    let padding = "x".repeat(1700);
    fill_history_with_pairs(&env, 7, &padding).await;
    env.insert_user_message("now answer the real question")
        .await;

    // Only one response: the actual answer. If preflight wrongly
    // triggered, .chat() would consume this entry as the "summary"
    // and .chat_stream() would then panic on an empty queue (or
    // return Text("") and silently break the test downstream). Either
    // way the assertion below catches it cleanly.
    let provider = MockProvider::new(vec![MockResponse::Text("the real answer".into())]);

    let (result, events) = env.run_inference_result(&provider).await;
    assert!(
        result.is_ok(),
        "inference should succeed: {:?}",
        result.err()
    );

    assert!(
        !saw_preflight_compact_start(&events),
        "preflight compact must NOT fire below 85% threshold; events: {events:?}"
    );
    assert!(
        !saw_compact_success(&events),
        "no compaction success event expected; events: {events:?}"
    );
}

/// Overflow recovery path: provider rejects the first chat with a
/// `400 prompt is too long` error → engine catches via
/// `is_context_overflow_error` → calls `try_overflow_recovery` →
/// compacts → retries chat_stream → succeeds.
///
/// Verifies:
/// - The "⚠️ Provider rejected request (context overflow)..." Warn
///   event fires (proves `try_overflow_recovery` was entered).
/// - The "✅ Compacted N messages. Retrying..." Info event fires
///   (proves compaction ran and the retry was attempted).
/// - The final result is `Ok(())` (proves the retry succeeded — the
///   user got an answer instead of a hard failure).
#[tokio::test]
async fn overflow_recovery_compacts_and_retries_successfully() {
    let _serial = lock_compact_state().await;

    // Need ≥4 messages for compaction to do anything. Pad enough that
    // the partial-compact path has work to do without forcing
    // HistoryTooLarge (which would short-circuit recovery).
    let env = Env::builder().max_context_tokens(8_000).build().await;
    let padding = "y".repeat(80);
    fill_history_with_pairs(&env, 4, &padding).await;
    env.insert_user_message("trigger overflow").await;

    // [0] ContextOverflow → initial chat_stream gets rejected
    // [1] Text("SUMMARY") → compact's .chat() call
    // [2] Text("recovered") → retried chat_stream succeeds
    let provider = MockProvider::new(vec![
        MockResponse::ContextOverflow,
        MockResponse::Text("SUMMARY: previous turns".into()),
        MockResponse::Text("recovered answer".into()),
    ]);

    let (result, events) = env.run_inference_result(&provider).await;
    assert!(
        result.is_ok(),
        "overflow should recover, not bubble up: {:?}",
        result.err()
    );
    assert!(
        saw_overflow_recovery_warn(&events),
        "expected overflow-recovery Warn event; events: {events:?}"
    );
    assert!(
        saw_compact_success(&events),
        "expected compaction success after overflow; events: {events:?}"
    );
}

/// Negative for overflow recovery: when the session is too short for
/// compaction to reduce anything (history < 4 messages →
/// `CompactSkip::TooShort`), `try_overflow_recovery` cannot save the
/// turn and the original overflow error must propagate. Without this
/// behaviour the user would silently get an empty turn.
///
/// This pins the "compaction unsuccessful → return original error"
/// branch at `inference.rs::try_overflow_recovery`'s `_ => return Err`
/// arm.
#[tokio::test]
async fn overflow_recovery_propagates_when_session_too_short_to_compact() {
    let _serial = lock_compact_state().await;

    let env = Env::builder().max_context_tokens(8_000).build().await;
    // Only the single trailing user message — total history = 1, well
    // below the compaction floor of 4.
    env.insert_user_message("hi").await;

    let provider = MockProvider::new(vec![MockResponse::ContextOverflow]);

    let (result, events) = env.run_inference_result(&provider).await;
    assert!(
        result.is_err(),
        "expected the overflow error to bubble up when compaction can't help; got Ok"
    );
    let err = format!("{:#}", result.unwrap_err()).to_lowercase();
    assert!(
        err.contains("context overflow") || err.contains("too long") || err.contains("compaction"),
        "error should mention overflow or compaction failure; got: {err}"
    );
    // Recovery was at least *attempted* — the warn event proves
    // `try_overflow_recovery` ran before deciding it couldn't help.
    assert!(
        saw_overflow_recovery_warn(&events),
        "expected overflow-recovery Warn event even when recovery ultimately failed; events: {events:?}"
    );
}
