//! Integration coverage for `PersistingSink` routing (#1129).
//!
//! ## What this file guards
//!
//! `PersistingSink` (`koda-core/src/engine/sink.rs`) is the
//! decorator that writes `Info` / `ChildTaskUpdate` / sub-agent-trace
//! events into the `session_events` table on the way to the
//! user-facing sink. The routing decision is a single branch:
//!
//! - `parent_tool_call_id == None` → top-level event, persist `Info`
//!   and `ChildTaskUpdate`.
//! - `parent_tool_call_id == Some(call_id)` → sub-agent event,
//!   persist a richer set (`Info`, `ToolCallStart`, `ApprovalRequest`,
//!   `AskUserRequest`) and stamp every row with `call_id` so the
//!   transcript renderer can fold the trace under the parent's
//!   `InvokeAgent` tool result (#1108).
//!
//! Pre-#1129 the wired-up routing had **zero** end-to-end tests —
//! only the inner-decision unit tests at the bottom of `sink.rs`.
//! A regression that swapped the branches, dropped the parent id, or
//! quietly stopped persisting one of the kinds would have surfaced
//! only when a user noticed `/export` produced flat (un-folded)
//! markdown, which is exactly the kind of slow-burn regression
//! integration tests are for.
//!
//! ## Why the real `Database`, not a mock
//!
//! `PersistingSink::persist` spawns a `tokio::task` and writes
//! through the `Persistence` trait. Mocking the trait would prove
//! the call shape but not the actual SQL — and the `parent_tool_call_id`
//! column is the bug surface (it's a nullable text column, easy to
//! mis-route by passing `Some("")` instead of `None`, or dropping the
//! parameter entirely). Using the real SQLite `Database` proves the
//! row lands with the right column value end-to-end.
//!
//! ## Why the polling helper
//!
//! `PersistingSink::persist` is fire-and-forget on a spawned task.
//! `await`ing the inserts directly would change the production code
//! path (it deliberately swallows errors so a DB hiccup can't crash
//! the inference loop). The poll loop with a 2s budget is the
//! lightweight test-only reconciliation — slow enough to survive
//! cold-cache CI, fast enough that a green run finishes in <50ms.
//!
//! ## Companion: retry behaviour (#1129 part 2)
//!
//! The second half of #1129 (`try_with_rate_limit` integration test)
//! is already covered by
//! `inference_retry_test::read_timeout_triggers_auto_retry_not_hard_failure`
//! (#1134), which exercises the same retry loop at the HTTP layer
//! via `FakeLlmServer::mount_chat_silent_then_resume`. That test
//! gives stronger coverage than the issue's mock-provider sketch
//! (it stalls a real socket past `KODA_READ_TIMEOUT_SECS`), so we
//! deliberately don't duplicate it here.

use koda_core::db::Database;
use koda_core::engine::EngineEvent;
use koda_core::engine::sink::{EngineSink, PersistingSink, TestSink};
use koda_core::persistence::{Persistence, session_event_kind as sek};
use koda_core::tools::ToolEffect;
use std::sync::Arc;
use std::time::{Duration, Instant};

// ── Helpers ───────────────────────────────────────────────

/// Spin up an isolated `Database` and a fresh session id. Returns the
/// db (as both concrete `Database` and `Arc<dyn Persistence>` so tests
/// can read back rows without an extra clone) and the session id.
///
/// Each call uses a fresh `tempfile::TempDir`; the dir is held by the
/// returned tuple so the SQLite file outlives the test body.
async fn fresh_db() -> (tempfile::TempDir, Arc<dyn Persistence>, String) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = Database::init(tmp.path()).await.expect("db init");
    let session_id = db
        .create_session("test-agent", tmp.path())
        .await
        .expect("create session");
    (tmp, Arc::new(db), session_id)
}

/// Wait up to ~2s for `load_session_events` to return at least
/// `expected` rows. Returns the rows once the count is reached, or
/// panics with a diagnostic if the budget elapses.
///
/// Polling cadence is 5ms — fast enough that a green test finishes
/// in <50ms, slow enough to not burn the CPU during the ~zero waits
/// we expect on a healthy machine.
async fn wait_for_events(
    db: &Arc<dyn Persistence>,
    session_id: &str,
    expected: usize,
) -> Vec<koda_core::persistence::SessionEvent> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let events = db.load_session_events(session_id).await.expect("load");
        if events.len() >= expected {
            return events;
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for {expected} session_events row(s); \
                 last seen: {} row(s) = {events:?}",
                events.len()
            );
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ── Top-level routing (parent_tool_call_id = None) ───────────────────

/// Top-level `Info` events land in `session_events` with `kind = info`,
/// `parent_tool_call_id = NULL`, and the message text as payload.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn top_level_info_event_persists_with_null_parent() {
    let (_tmp, db, session_id) = fresh_db().await;
    let inner = TestSink::new();
    let sink = PersistingSink::new(&inner, db.clone(), session_id.clone(), None);

    sink.emit(EngineEvent::Info {
        message: "compaction completed".into(),
    });

    let events = wait_for_events(&db, &session_id, 1).await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, sek::INFO);
    assert_eq!(events[0].payload, "compaction completed");
    assert_eq!(
        events[0].parent_tool_call_id, None,
        "top-level events must have parent_tool_call_id = NULL; \
         a non-null value here would mis-fold the row under a phantom parent in /export"
    );

    // Forwarding contract: every event still reaches the inner sink.
    let forwarded = inner.events();
    assert_eq!(forwarded.len(), 1);
    assert!(
        matches!(&forwarded[0], EngineEvent::Info { message } if message == "compaction completed")
    );
}

/// Top-level `ChildTaskUpdate` events persist with `kind = bg_task_update`
/// and a JSON-encoded payload (so the renderer can deserialize the
/// `AgentStatus` back into the original variant).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn top_level_bg_task_update_persists_as_json_with_null_parent() {
    use koda_core::child_agent::AgentStatus;

    let (_tmp, db, session_id) = fresh_db().await;
    let inner = TestSink::new();
    let sink = PersistingSink::new(&inner, db.clone(), session_id.clone(), None);

    let event = EngineEvent::ChildTaskUpdate {
        task_id: 42,
        spawner: None,
        is_background: true,
        status: AgentStatus::Running { iter: 7 },
    };
    sink.emit(event);

    let events = wait_for_events(&db, &session_id, 1).await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, sek::BG_TASK_UPDATE);
    assert_eq!(events[0].parent_tool_call_id, None);

    // Payload must be valid JSON with the task fields preserved.
    // Asserting on the structure (not the exact bytes) so a future
    // serde rename of an unrelated field doesn't break this test.
    // Both `EngineEvent` (`tag = "type"`) and `AgentStatus`
    // (`tag = "kind"`) use internally-tagged enum representations,
    // so `Running { iter: 7 }` flattens to `{"kind": "running", "iter": 7}`
    // alongside `task_id` at the same level.
    let parsed: serde_json::Value =
        serde_json::from_str(&events[0].payload).expect("payload must be JSON");
    assert_eq!(parsed["task_id"], 42, "full payload: {parsed}");
    assert_eq!(
        parsed["status"]["kind"], "running",
        "full payload: {parsed}"
    );
    assert_eq!(parsed["status"]["iter"], 7, "full payload: {parsed}");
}

/// Forwarding contract: events that aren't on the `Info` /
/// `ChildTaskUpdate` allowlist must still reach the inner sink
/// untouched. The persistence-side guarantee ("these events do not
/// write rows") is covered as a pure-function test in
/// [`koda_core::engine::sink::tests::classify_top_level_skips_non_allowlisted_events`]
/// — see #1265 item 8a, PR-2 for why the negative DB assertion
/// previously here was deleted in favour of testing the *decision*
/// instead of waiting for the *side effect*.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn top_level_forwards_non_persistable_events_unconditionally() {
    let (_tmp, db, session_id) = fresh_db().await;
    let inner = TestSink::new();
    let sink = PersistingSink::new(&inner, db.clone(), session_id.clone(), None);

    sink.emit(EngineEvent::ResponseStart);
    sink.emit(EngineEvent::TextDelta {
        text: "hello".into(),
    });
    sink.emit(EngineEvent::TextDone);
    sink.emit(EngineEvent::ToolCallStart {
        id: "call-1".into(),
        name: "Read".into(),
        args: serde_json::json!({"path": "f.txt"}),
        is_sub_agent: false,
    });

    // Forwarding still works for every event, persisted or not.
    assert_eq!(inner.len(), 4);
}

// ── Sub-agent routing (parent_tool_call_id = Some(call_id)) ────────────

/// Sub-agent `Info` events land with `kind = sub_agent_event` (NOT the
/// top-level `info` kind) and the parent call id stamped on the row.
/// This is the exact contract that drives `/export` transcript folding.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sub_agent_info_event_persists_with_parent_call_id_and_sub_agent_kind() {
    let (_tmp, db, session_id) = fresh_db().await;
    let inner = TestSink::new();
    let parent_id = "invoke-agent-call-abc-123";
    let sink = PersistingSink::new(
        &inner,
        db.clone(),
        session_id.clone(),
        Some(parent_id.to_string()),
    );

    sink.emit(EngineEvent::Info {
        message: "  🔍 explore: scanning workspace".into(),
    });

    let events = wait_for_events(&db, &session_id, 1).await;
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].kind,
        sek::SUB_AGENT_EVENT,
        "sub-agent Info must be tagged sub_agent_event, not info — \
         the renderer dispatches on this column to choose the folded layout"
    );
    assert_eq!(events[0].payload, "  🔍 explore: scanning workspace");
    assert_eq!(
        events[0].parent_tool_call_id.as_deref(),
        Some(parent_id),
        "parent_tool_call_id must round-trip exactly so /export can fold the row"
    );
}

/// Sub-agent `ToolCallStart` events persist as `sub_agent_event` with
/// the tool name in the payload (rendered as `🔧 <name>` per the
/// BufferingSink convention). Top-level `ToolCallStart` is suppressed
/// (already in `messages.tool_calls`); the asymmetry is the routing
/// contract this test pins.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sub_agent_tool_call_start_persists_with_tool_name_payload() {
    let (_tmp, db, session_id) = fresh_db().await;
    let inner = TestSink::new();
    let sink = PersistingSink::new(
        &inner,
        db.clone(),
        session_id.clone(),
        Some("parent-1".into()),
    );

    sink.emit(EngineEvent::ToolCallStart {
        id: "tc-1".into(),
        name: "Glob".into(),
        args: serde_json::json!({"pattern": "**/*.rs"}),
        is_sub_agent: true,
    });

    let events = wait_for_events(&db, &session_id, 1).await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, sek::SUB_AGENT_EVENT);
    assert!(
        events[0].payload.contains("Glob"),
        "tool name must be in payload (rendered for the trace); got: {}",
        events[0].payload
    );
    assert_eq!(events[0].parent_tool_call_id.as_deref(), Some("parent-1"));
}

/// Sub-agents have no user channel; approval requests are auto-rejected
/// and the rejection must be visible in the persisted trace. Same row
/// shape as the other sub-agent events (folded under parent on /export).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sub_agent_approval_request_persists_as_auto_reject_marker() {
    let (_tmp, db, session_id) = fresh_db().await;
    let inner = TestSink::new();
    let sink = PersistingSink::new(
        &inner,
        db.clone(),
        session_id.clone(),
        Some("parent-2".into()),
    );

    sink.emit(EngineEvent::ApprovalRequest {
        id: "ap-1".into(),
        tool_name: "Delete".into(),
        detail: "rm important.txt".into(),
        preview: None,
        effect: ToolEffect::Destructive,
    });

    let events = wait_for_events(&db, &session_id, 1).await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, sek::SUB_AGENT_EVENT);
    assert!(
        events[0].payload.contains("auto-rejected"),
        "approval-without-channel must be marked as auto-rejected so the \
         user can see why a sub-agent skipped a destructive op; got: {}",
        events[0].payload
    );
    assert!(events[0].payload.contains("Delete"));
    assert_eq!(events[0].parent_tool_call_id.as_deref(), Some("parent-2"));
}

/// Sub-agent `AskUserRequest` events also persist as auto-skip markers
/// (sub-agents have no user to ask). Question text is truncated to 80
/// chars so a runaway 10K-char prompt doesn't bloat the events table.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sub_agent_ask_user_request_persists_truncated() {
    let (_tmp, db, session_id) = fresh_db().await;
    let inner = TestSink::new();
    let sink = PersistingSink::new(
        &inner,
        db.clone(),
        session_id.clone(),
        Some("parent-3".into()),
    );

    let long_question = "Q".repeat(500);
    sink.emit(EngineEvent::AskUserRequest {
        id: "aq-1".into(),
        question: long_question.clone(),
        options: vec![],
    });

    let events = wait_for_events(&db, &session_id, 1).await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, sek::SUB_AGENT_EVENT);
    assert!(events[0].payload.contains("auto-skipped"));
    // Truncation: payload contains <= 80 Q's (the prefix) plus the
    // boilerplate. A regression that dropped the truncation would
    // leak the full 500-char question into the row.
    let q_count = events[0].payload.matches('Q').count();
    assert!(
        q_count <= 80,
        "question must be truncated to <=80 chars; got {q_count} Qs in payload: {}",
        events[0].payload
    );
}

/// Forwarding contract on the sub-agent path: `ChildTaskUpdate`
/// must reach the inner sink even though it's not on the sub-agent
/// persist allowlist. The persistence-side guarantee is covered as
/// a pure-function test in
/// [`koda_core::engine::sink::tests::classify_sub_agent_skips_child_task_update`]
/// — see #1265 item 8a, PR-2.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sub_agent_forwards_child_task_update_unconditionally() {
    use koda_core::child_agent::AgentStatus;

    let (_tmp, db, session_id) = fresh_db().await;
    let inner = TestSink::new();
    let sink = PersistingSink::new(
        &inner,
        db.clone(),
        session_id.clone(),
        Some("parent-4".into()),
    );

    sink.emit(EngineEvent::ChildTaskUpdate {
        task_id: 1,
        spawner: Some(99),
        is_background: true,
        status: AgentStatus::Pending,
    });

    // Inner sink still saw the event — forwarding is unconditional.
    assert_eq!(inner.len(), 1);
}

// ── Mixed multi-event scenarios ─────────────────────────────────

/// End-to-end shape: multiple events on a sub-agent sink land in
/// emission order and all carry the same parent id. This is the
/// scenario `/export` consumes — a contiguous block of rows the
/// renderer folds under the parent's tool result.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sub_agent_multi_event_sequence_preserves_order_and_parent_id() {
    let (_tmp, db, session_id) = fresh_db().await;
    let inner = TestSink::new();
    let parent = "invoke-call-multi";
    let sink = PersistingSink::new(&inner, db.clone(), session_id.clone(), Some(parent.into()));

    sink.emit(EngineEvent::Info {
        message: "  🔍 starting".into(),
    });
    sink.emit(EngineEvent::ToolCallStart {
        id: "t1".into(),
        name: "Glob".into(),
        args: serde_json::json!({"pattern": "*.rs"}),
        is_sub_agent: true,
    });
    sink.emit(EngineEvent::ToolCallStart {
        id: "t2".into(),
        name: "Read".into(),
        args: serde_json::json!({"path": "src/main.rs"}),
        is_sub_agent: true,
    });
    sink.emit(EngineEvent::Info {
        message: "  ✅ done".into(),
    });

    // ⚠ Don't hard-assert exactly 4 rows immediately — `tokio::spawn`
    // doesn't guarantee FIFO across spawned tasks. Wait for AT LEAST
    // 4, then compare set-membership rather than per-index ordering
    // for the *kinds*. SQLite's auto-increment id IS monotonic by
    // insert order, but the inserts themselves are spawned tasks and
    // could reorder under load. We document this tolerance instead
    // of asserting strict order, because the production renderer
    // already sorts by `id` and so cares only that all four landed
    // with the right parent.
    let events = wait_for_events(&db, &session_id, 4).await;
    assert_eq!(events.len(), 4, "got: {events:?}");
    for ev in &events {
        assert_eq!(
            ev.kind,
            sek::SUB_AGENT_EVENT,
            "every row in a sub-agent sink must be tagged sub_agent_event"
        );
        assert_eq!(
            ev.parent_tool_call_id.as_deref(),
            Some(parent),
            "every row must carry the parent id for /export folding"
        );
    }

    // Payloads — order-insensitive set check (see above re: spawn order).
    let payloads: Vec<&str> = events.iter().map(|e| e.payload.as_str()).collect();
    assert!(payloads.iter().any(|p| p.contains("starting")));
    assert!(payloads.iter().any(|p| p.contains("Glob")));
    assert!(payloads.iter().any(|p| p.contains("Read")));
    assert!(payloads.iter().any(|p| p.contains("done")));
}

/// Forwarding stays intact regardless of routing: events that NEITHER
/// path persists (e.g. `ResponseStart`, `TextDone`) still pass through
/// to the inner sink. Locks in the "decorator transparency" contract
/// — a buggy implementation that early-returned after the routing
/// branches would silently drop these events.
///
/// The persistence-side guarantee for these events ("do not write
/// rows") is covered as a pure-function test in
/// [`koda_core::engine::sink::tests::classify_sub_agent_skips_other_non_allowlisted_events`]
/// — see #1265 item 8a, PR-2 for why the negative DB assertion
/// previously here was deleted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forwarding_is_unconditional_for_non_persisted_events() {
    let (_tmp, db, session_id) = fresh_db().await;
    let inner = TestSink::new();
    let sink = PersistingSink::new(&inner, db.clone(), session_id.clone(), Some("p".into()));

    sink.emit(EngineEvent::ResponseStart);
    sink.emit(EngineEvent::TextDone);

    // Inner sink saw both — forwarding is unconditional.
    assert_eq!(inner.len(), 2);
    assert!(matches!(&inner.events()[0], EngineEvent::ResponseStart));
    assert!(matches!(&inner.events()[1], EngineEvent::TextDone));
}
