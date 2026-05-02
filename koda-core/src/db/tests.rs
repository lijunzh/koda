//! Tests for the SQLite persistence layer.
//!
//! Covers the `Persistence` trait methods on `Database`, message pruning
//! helpers (`prune_mismatched_tool_calls`, `prune_null_content_messages`,
//! `prune_whitespace_only_messages`), and interrupted turn detection.

use super::queries::prune_mismatched_tool_calls;
use super::queries::{
    dedupe_tool_results_by_call_id, detect_interruption, prune_null_content_messages,
    prune_whitespace_only_messages,
};
use crate::db::Database;
use crate::persistence::{InterruptionKind, Message, Persistence, Role};
use tempfile::TempDir;

async fn setup() -> (Database, TempDir) {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("test.db");
    let db = Database::open(&db_path).await.unwrap();
    (db, tmp)
}

/// Insert an assistant message and immediately mark it complete.
/// Most tests need "born-complete" assistant messages; only interruption
/// tests deliberately omit the `mark_message_complete` call.
async fn insert_complete_assistant(
    db: &Database,
    session: &str,
    content: Option<&str>,
    tool_calls: Option<&str>,
) -> i64 {
    let mid = db
        .insert_message(session, &Role::Assistant, content, tool_calls, None, None)
        .await
        .unwrap();
    db.mark_message_complete(mid).await.unwrap();
    mid
}

#[tokio::test]
async fn test_create_session() {
    let (db, _tmp) = setup().await;
    let id = db.create_session("default", _tmp.path()).await.unwrap();
    assert!(!id.is_empty());
}

#[tokio::test]
async fn test_insert_and_load_messages() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    db.insert_message(&session, &Role::User, Some("hello"), None, None, None)
        .await
        .unwrap();
    insert_complete_assistant(&db, &session, Some("hi there!"), None).await;

    let msgs = db.load_context(&session).await.unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].role, Role::User);
    assert_eq!(msgs[1].role, Role::Assistant);
}

#[tokio::test]
async fn test_load_context_returns_all_active_messages() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    // Insert many messages
    for i in 0..20 {
        let content = format!("Message number {i}");
        db.insert_message(&session, &Role::User, Some(&content), None, None, None)
            .await
            .unwrap();
    }

    // Load all messages — no sliding window, no truncation
    let msgs = db.load_context(&session).await.unwrap();
    assert_eq!(msgs.len(), 20, "Should load all 20 messages");

    // Messages should be in chronological order
    assert!(msgs[0].content.as_ref().unwrap().contains("number 0"));
    assert!(msgs[19].content.as_ref().unwrap().contains("number 19"));
}

#[tokio::test]
async fn test_sessions_are_isolated() {
    let (db, _tmp) = setup().await;
    let s1 = db.create_session("agent-a", _tmp.path()).await.unwrap();
    let s2 = db.create_session("agent-b", _tmp.path()).await.unwrap();

    db.insert_message(&s1, &Role::User, Some("session 1"), None, None, None)
        .await
        .unwrap();
    db.insert_message(&s2, &Role::User, Some("session 2"), None, None, None)
        .await
        .unwrap();

    let msgs1 = db.load_context(&s1).await.unwrap();
    let msgs2 = db.load_context(&s2).await.unwrap();

    assert_eq!(msgs1.len(), 1);
    assert_eq!(msgs2.len(), 1);
    assert_eq!(msgs1[0].content.as_deref().unwrap(), "session 1");
    assert_eq!(msgs2[0].content.as_deref().unwrap(), "session 2");
}

#[tokio::test]
async fn test_session_token_usage() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    db.insert_message(&session, &Role::User, Some("q1"), None, None, None)
        .await
        .unwrap();
    let usage1 = crate::providers::TokenUsage {
        prompt_tokens: 100,
        completion_tokens: 50,
        ..Default::default()
    };
    db.insert_message(
        &session,
        &Role::Assistant,
        Some("a1"),
        None,
        None,
        Some(&usage1),
    )
    .await
    .unwrap();
    db.insert_message(&session, &Role::User, Some("q2"), None, None, None)
        .await
        .unwrap();
    let usage2 = crate::providers::TokenUsage {
        prompt_tokens: 200,
        completion_tokens: 80,
        ..Default::default()
    };
    db.insert_message(
        &session,
        &Role::Assistant,
        Some("a2"),
        None,
        None,
        Some(&usage2),
    )
    .await
    .unwrap();

    let u = db.session_token_usage(&session).await.unwrap();
    assert_eq!(u.prompt_tokens, 300);
    assert_eq!(u.completion_tokens, 130);
    assert_eq!(u.api_calls, 2);
}

#[tokio::test]
async fn test_list_sessions() {
    let (db, _tmp) = setup().await;
    db.create_session("agent-a", _tmp.path()).await.unwrap();
    db.create_session("agent-b", _tmp.path()).await.unwrap();
    db.create_session("agent-c", _tmp.path()).await.unwrap();

    let sessions = db.list_sessions(10, _tmp.path()).await.unwrap();
    assert_eq!(sessions.len(), 3);
    // Most recent first
    assert_eq!(sessions[0].agent_name, "agent-c");
}

#[tokio::test]
async fn test_delete_session() {
    let (db, _tmp) = setup().await;
    let s1 = db.create_session("default", _tmp.path()).await.unwrap();
    db.insert_message(&s1, &Role::User, Some("hello"), None, None, None)
        .await
        .unwrap();

    assert!(db.delete_session(&s1).await.unwrap());

    let sessions = db.list_sessions(10, _tmp.path()).await.unwrap();
    assert!(sessions.is_empty());

    // Deleting again returns false
    assert!(!db.delete_session(&s1).await.unwrap());
}

#[tokio::test]
async fn test_compact_session() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    // Insert several messages; assistant messages are marked complete
    // (they represent finished turns that load_context must return).
    for i in 0..10 {
        let role = if i % 2 == 0 {
            &Role::User
        } else {
            &Role::Assistant
        };
        let mid = db
            .insert_message(&session, role, Some(&format!("msg {i}")), None, None, None)
            .await
            .unwrap();
        if *role == Role::Assistant {
            db.mark_message_complete(mid).await.unwrap();
        }
    }

    // Compact preserving the last 2 messages
    let deleted = db
        .compact_session(&session, "Summary of conversation", 2)
        .await
        .unwrap();
    assert_eq!(deleted, 8); // 10 total - 2 preserved = 8 deleted

    // Should have: summary(system) + continuation(assistant) + 2 preserved = 4
    let msgs = db.load_context(&session).await.unwrap();
    assert_eq!(msgs.len(), 4);

    // Check that the summary is a system message
    let system_msgs: Vec<_> = msgs.iter().filter(|m| m.role == Role::System).collect();
    assert_eq!(system_msgs.len(), 1);
    assert!(
        system_msgs[0]
            .content
            .as_ref()
            .unwrap()
            .contains("Summary of conversation")
    );

    // Check that there's a continuation hint as assistant
    let assistant_msgs: Vec<_> = msgs.iter().filter(|m| m.role == Role::Assistant).collect();
    assert!(
        assistant_msgs
            .iter()
            .any(|m| m.content.as_deref().unwrap_or("").contains("compacted")),
        "Expected a continuation hint from assistant"
    );

    // The 2 preserved messages should still be there
    let preserved: Vec<_> = msgs
        .iter()
        .filter(|m| m.content.as_deref().is_some_and(|c| c.starts_with("msg ")))
        .collect();
    assert_eq!(preserved.len(), 2);
}

#[tokio::test]
async fn test_compact_preserves_zero() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    for i in 0..6 {
        let role = if i % 2 == 0 {
            &Role::User
        } else {
            &Role::Assistant
        };
        db.insert_message(&session, role, Some(&format!("msg {i}")), None, None, None)
            .await
            .unwrap();
    }

    // Compact preserving 0 — deletes everything, inserts summary + continuation
    let deleted = db
        .compact_session(&session, "Full summary", 0)
        .await
        .unwrap();
    assert_eq!(deleted, 6);

    let msgs = db.load_context(&session).await.unwrap();
    assert_eq!(msgs.len(), 2); // summary + continuation
    assert_eq!(msgs.iter().filter(|m| m.role == Role::System).count(), 1);
    assert_eq!(msgs.iter().filter(|m| m.role == Role::Assistant).count(), 1);
}

#[tokio::test]
async fn test_has_pending_tool_calls() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    // No messages → no pending
    assert!(!db.has_pending_tool_calls(&session).await.unwrap());

    // User message → no pending
    db.insert_message(&session, &Role::User, Some("hello"), None, None, None)
        .await
        .unwrap();
    assert!(!db.has_pending_tool_calls(&session).await.unwrap());

    // Assistant with tool_calls → pending!
    db.insert_message(
        &session,
        &Role::Assistant,
        None,
        Some(r#"[{"id":"tc1","name":"Read","arguments":"{}"}]"#),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(db.has_pending_tool_calls(&session).await.unwrap());

    // Tool response → no longer pending
    db.insert_message(
        &session,
        &Role::Tool,
        Some("file contents"),
        None,
        Some("tc1"),
        None,
    )
    .await
    .unwrap();
    assert!(!db.has_pending_tool_calls(&session).await.unwrap());
}

#[tokio::test]
async fn test_prune_mismatched_tool_calls() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    // Normal turn: user → assistant with tool_calls → tool result
    db.insert_message(&session, &Role::User, Some("hello"), None, None, None)
        .await
        .unwrap();
    let mid = db
        .insert_message(
            &session,
            &Role::Assistant,
            Some("Let me read that."),
            Some(r#"[{"id":"tc1","name":"Read","arguments":"{}"}]"#),
            None,
            None,
        )
        .await
        .unwrap();
    db.mark_message_complete(mid).await.unwrap();
    db.insert_message(
        &session,
        &Role::Tool,
        Some("file contents"),
        None,
        Some("tc1"),
        None,
    )
    .await
    .unwrap();

    // Interrupted turn: assistant with tool_calls but NO tool result and NOT marked complete
    db.insert_message(
        &session,
        &Role::Assistant,
        Some("I'll edit the file."),
        Some(r#"[{"id":"tc2","name":"Edit","arguments":"{}"}]"#),
        None,
        None,
    )
    .await
    .unwrap();
    // deliberately no mark_message_complete — simulates interrupted turn

    let msgs = db.load_context(&session).await.unwrap();

    // The first assistant's tool_calls should be preserved (has tool result)
    let first_asst = msgs
        .iter()
        .find(|m| m.content.as_deref() == Some("Let me read that."))
        .unwrap();
    assert!(
        first_asst.tool_calls.is_some(),
        "completed tool_calls should be preserved"
    );

    // The orphaned assistant (tool_calls with no result) should be dropped entirely
    let orphaned = msgs
        .iter()
        .find(|m| m.content.as_deref() == Some("I'll edit the file."));
    assert!(
        orphaned.is_none(),
        "orphaned assistant message should be dropped by prune_mismatched_tool_calls"
    );
}

#[test]
fn test_prune_mismatched_tool_calls_unit() {
    fn msg(
        role: &str,
        content: Option<&str>,
        tool_calls: Option<&str>,
        tool_call_id: Option<&str>,
    ) -> Message {
        Message {
            id: 0,
            session_id: String::new(),
            role: role.parse().unwrap_or(Role::User),
            content: content.map(Into::into),
            full_content: None,
            tool_calls: tool_calls.map(Into::into),
            tool_call_id: tool_call_id.map(Into::into),
            prompt_tokens: None,
            completion_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            thinking_tokens: None,
            thinking_content: None,
            created_at: None,
        }
    }

    // No messages — no crash
    let mut empty: Vec<Message> = vec![];
    prune_mismatched_tool_calls(&mut empty);
    assert!(empty.is_empty());

    // User message only — no change
    let mut msgs = vec![msg("user", Some("hi"), None, None)];
    prune_mismatched_tool_calls(&mut msgs);
    assert_eq!(msgs.len(), 1);

    // Orphaned assistant with tool_calls, no result — dropped
    let mut msgs = vec![
        msg("user", Some("hi"), None, None),
        msg(
            "assistant",
            Some("doing it"),
            Some(r#"[{"id":"t1"}]"#),
            None,
        ),
    ];
    prune_mismatched_tool_calls(&mut msgs);
    assert_eq!(msgs.len(), 1, "orphaned assistant should be dropped");
    assert_eq!(msgs[0].role, Role::User);

    // Complete pair — preserved
    let mut msgs = vec![
        msg("user", Some("hi"), None, None),
        msg("assistant", None, Some(r#"[{"id":"t1"}]"#), None),
        msg("tool", Some("ok"), None, Some("t1")),
    ];
    prune_mismatched_tool_calls(&mut msgs);
    assert_eq!(msgs.len(), 3, "complete pair should be preserved");
    assert!(msgs[1].tool_calls.is_some());
}

// ── #1159: dedupe_tool_results_by_call_id ─────────────────────────────────

/// Helper: build a `Message` with an explicit `id`. Mirrors the helper in
/// `test_prune_mismatched_tool_calls_unit` but sets `id` so we can
/// exercise the "latest by id" semantics.
fn msg_with_id(id: i64, role: &str, content: Option<&str>, tool_call_id: Option<&str>) -> Message {
    Message {
        id,
        session_id: String::new(),
        role: role.parse().unwrap_or(Role::User),
        content: content.map(Into::into),
        full_content: None,
        tool_calls: None,
        tool_call_id: tool_call_id.map(Into::into),
        prompt_tokens: None,
        completion_tokens: None,
        cache_read_tokens: None,
        cache_creation_tokens: None,
        thinking_tokens: None,
        thinking_content: None,
        created_at: None,
    }
}

#[test]
fn dedupe_empty_input_is_noop() {
    let mut msgs: Vec<Message> = vec![];
    dedupe_tool_results_by_call_id(&mut msgs);
    assert!(msgs.is_empty());
}

#[test]
fn dedupe_no_tool_rows_is_noop() {
    let mut msgs = vec![
        msg_with_id(1, "user", Some("hi"), None),
        msg_with_id(2, "assistant", Some("hello"), None),
    ];
    dedupe_tool_results_by_call_id(&mut msgs);
    assert_eq!(msgs.len(), 2);
}

#[test]
fn dedupe_unique_call_ids_preserved() {
    let mut msgs = vec![
        msg_with_id(1, "tool", Some("result for tc1"), Some("tc1")),
        msg_with_id(2, "tool", Some("result for tc2"), Some("tc2")),
        msg_with_id(3, "tool", Some("result for tc3"), Some("tc3")),
    ];
    dedupe_tool_results_by_call_id(&mut msgs);
    assert_eq!(msgs.len(), 3, "unique call_ids must all survive");
}

#[test]
fn dedupe_keeps_latest_for_same_call_id() {
    // The bg-agent scenario: dispatch-turn stub (id=2) then async
    // completion (id=5) sharing the parent InvokeAgent's tool_call_id.
    let mut msgs = vec![
        msg_with_id(1, "user", Some("please explore"), None),
        msg_with_id(2, "tool", Some("started (agent:1)"), Some("tc1")),
        msg_with_id(3, "assistant", Some("working on it"), None),
        msg_with_id(5, "tool", Some("[completed] Found 3 issues"), Some("tc1")),
    ];
    dedupe_tool_results_by_call_id(&mut msgs);

    assert_eq!(msgs.len(), 3, "the dispatch-turn stub must be dropped");
    let tool = msgs.iter().find(|m| m.role == Role::Tool).unwrap();
    assert_eq!(
        tool.id, 5,
        "only the latest tool_result for tc1 should remain"
    );
    assert!(
        tool.content.as_deref().unwrap().contains("Found 3 issues"),
        "latest content must be preserved"
    );
}

#[test]
fn dedupe_handles_many_call_ids_independently() {
    // Mixed: tc1 has 2 writes, tc2 has 3 writes, tc3 has 1 write.
    let mut msgs = vec![
        msg_with_id(1, "tool", Some("tc1 stub"), Some("tc1")),
        msg_with_id(2, "tool", Some("tc2 stub"), Some("tc2")),
        msg_with_id(3, "tool", Some("tc3 only"), Some("tc3")),
        msg_with_id(4, "tool", Some("tc1 final"), Some("tc1")),
        msg_with_id(5, "tool", Some("tc2 mid"), Some("tc2")),
        msg_with_id(6, "tool", Some("tc2 final"), Some("tc2")),
    ];
    dedupe_tool_results_by_call_id(&mut msgs);

    assert_eq!(
        msgs.len(),
        3,
        "one tool_result per unique call_id should remain"
    );
    let by_call_id: std::collections::HashMap<&str, &str> = msgs
        .iter()
        .filter_map(|m| {
            Some((
                m.tool_call_id.as_deref()?,
                m.content.as_deref().unwrap_or(""),
            ))
        })
        .collect();
    assert_eq!(by_call_id.get("tc1"), Some(&"tc1 final"));
    assert_eq!(by_call_id.get("tc2"), Some(&"tc2 final"));
    assert_eq!(by_call_id.get("tc3"), Some(&"tc3 only"));
}

#[test]
fn dedupe_preserves_non_tool_rows_around_dupes() {
    let mut msgs = vec![
        msg_with_id(1, "system", Some("sys prompt"), None),
        msg_with_id(2, "user", Some("do the thing"), None),
        msg_with_id(3, "tool", Some("stub"), Some("tc1")),
        msg_with_id(4, "assistant", Some("i delegated"), None),
        msg_with_id(5, "tool", Some("final"), Some("tc1")),
        msg_with_id(6, "user", Some("thanks"), None),
    ];
    dedupe_tool_results_by_call_id(&mut msgs);

    assert_eq!(msgs.len(), 5, "only the duplicate Tool stub should drop");
    let roles: Vec<Role> = msgs.iter().map(|m| m.role.clone()).collect();
    assert_eq!(
        roles,
        vec![
            Role::System,
            Role::User,
            Role::Assistant,
            Role::Tool,
            Role::User,
        ]
    );
}

#[test]
fn dedupe_tool_row_without_call_id_is_kept() {
    // Defensive: a malformed Tool row with no tool_call_id should not be
    // dropped by this pass (other passes handle malformed rows).
    let mut msgs = vec![
        msg_with_id(1, "tool", Some("orphan"), None),
        msg_with_id(2, "tool", Some("keyed"), Some("tc1")),
    ];
    dedupe_tool_results_by_call_id(&mut msgs);
    assert_eq!(msgs.len(), 2, "orphan Tool row is kept (not our concern)");
}

/// End-to-end through `load_context`: simulates the real bg-agent flow.
///
/// (#1159) Sequence: user prompt → assistant calls InvokeAgent(tc1) →
/// dispatch-turn stub tool_result(tc1) → assistant works on something else
/// → async drain writes a second tool_result(tc1) on completion. The
/// model on its next turn should see ONLY the completion, not the stub.
#[tokio::test]
async fn dedupe_via_load_context_keeps_only_latest_bg_completion() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    // Turn 1: user asks, assistant invokes a bg sub-agent.
    db.insert_message(
        &session,
        &Role::User,
        Some("please explore"),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let mid = db
        .insert_message(
            &session,
            &Role::Assistant,
            Some("I'll spawn an explorer"),
            Some(r#"[{"id":"tc1","name":"InvokeAgent","arguments":"{}"}]"#),
            None,
            None,
        )
        .await
        .unwrap();
    db.mark_message_complete(mid).await.unwrap();

    // Synchronous dispatch result: "started (agent:1)" stub keyed on tc1.
    db.insert_tool_message_with_full(
        &session,
        "Background agent 'explore' started (agent:1). Results will be injected when complete.",
        "tc1",
        "Background agent 'explore' started (agent:1). Results will be injected when complete.",
    )
    .await
    .unwrap();

    // (Imagine the assistant did other work here.)

    // Async drain: completion writes a SECOND tool_result keyed on tc1.
    db.insert_tool_message_with_full(
        &session,
        "[Background agent 'explore' completed]\nOriginal task: explore\nResult:\nFound 3 issues.",
        "tc1",
        "[Background agent 'explore' completed]\nOriginal task: explore\nResult:\nFound 3 issues.",
    )
    .await
    .unwrap();

    let msgs = db.load_context(&session).await.unwrap();

    let tool_rows: Vec<&Message> = msgs.iter().filter(|m| m.role == Role::Tool).collect();
    assert_eq!(
        tool_rows.len(),
        1,
        "only the latest tool_result for tc1 should reach the provider context, got {tool_rows:#?}"
    );
    let surviving = tool_rows[0];
    assert_eq!(surviving.tool_call_id.as_deref(), Some("tc1"));
    assert!(
        surviving.content.as_deref().unwrap().contains("completed"),
        "the completion content should win, not the stub"
    );
    assert!(
        !surviving.content.as_deref().unwrap().contains("started"),
        "the dispatch-turn stub must NOT be the surviving row"
    );

    // The full transcript (load_all_messages) preserves both rows for
    // debug-bundle / forensic use.
    let all = db.load_all_messages(&session).await.unwrap();
    let all_tool_rows: Vec<&Message> = all.iter().filter(|m| m.role == Role::Tool).collect();
    assert_eq!(
        all_tool_rows.len(),
        2,
        "load_all_messages must preserve full history (both stub and completion)"
    );
}

#[tokio::test]
async fn test_session_metadata_and_todo() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    // No metadata initially
    assert!(db.get_todo(&session).await.unwrap().is_none());
    assert!(
        db.get_metadata(&session, "anything")
            .await
            .unwrap()
            .is_none()
    );

    // Set and get todo
    db.set_todo(&session, "- [ ] Task 1\n- [x] Task 2")
        .await
        .unwrap();
    let todo = db.get_todo(&session).await.unwrap().unwrap();
    assert!(todo.contains("Task 1"));
    assert!(todo.contains("Task 2"));

    // Update (upsert) replaces the value
    db.set_todo(&session, "- [x] Task 1\n- [x] Task 2")
        .await
        .unwrap();
    let todo = db.get_todo(&session).await.unwrap().unwrap();
    assert!(todo.starts_with("- [x] Task 1"));

    // Generic metadata works too
    db.set_metadata(&session, "custom_key", "custom_value")
        .await
        .unwrap();
    assert_eq!(
        db.get_metadata(&session, "custom_key")
            .await
            .unwrap()
            .unwrap(),
        "custom_value"
    );
}

#[tokio::test]
async fn test_token_usage_empty_session() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    let u = db.session_token_usage(&session).await.unwrap();
    assert_eq!(u.prompt_tokens, 0);
    assert_eq!(u.completion_tokens, 0);
    assert_eq!(u.api_calls, 0);
}

#[tokio::test]
async fn test_last_assistant_message() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    // Empty session returns empty string
    let msg = db.last_assistant_message(&session).await.unwrap();
    assert_eq!(msg, "");

    // Insert some messages
    db.insert_message(&session, &Role::User, Some("question 1"), None, None, None)
        .await
        .unwrap();
    db.insert_message(
        &session,
        &Role::Assistant,
        Some("answer 1"),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    db.insert_message(&session, &Role::User, Some("question 2"), None, None, None)
        .await
        .unwrap();
    db.insert_message(
        &session,
        &Role::Assistant,
        Some("answer 2"),
        None,
        None,
        None,
    )
    .await
    .unwrap();

    // Should return the LAST assistant message
    let msg = db.last_assistant_message(&session).await.unwrap();
    assert_eq!(msg, "answer 2");
}

#[tokio::test]
async fn test_last_assistant_message_skips_tool_calls() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    db.insert_message(
        &session,
        &Role::User,
        Some("do something"),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    // Assistant with tool calls but no text content
    db.insert_message(
        &session,
        &Role::Assistant,
        None,
        Some("[{\"id\":\"1\"}]"),
        None,
        None,
    )
    .await
    .unwrap();
    db.insert_message(
        &session,
        &Role::Tool,
        Some("tool result"),
        None,
        Some("1"),
        None,
    )
    .await
    .unwrap();
    // Final text response
    db.insert_message(&session, &Role::Assistant, Some("Done!"), None, None, None)
        .await
        .unwrap();

    let msg = db.last_assistant_message(&session).await.unwrap();
    assert_eq!(msg, "Done!");
}

// ── prune_null_content_messages tests (#594) ──────────────────────────────

#[test]
fn test_prune_null_content_drops_ghost_assistant() {
    fn msg(role: &str, content: Option<&str>, tool_calls: Option<&str>) -> Message {
        Message {
            id: 0,
            session_id: String::new(),
            role: role.parse().unwrap_or(Role::User),
            content: content.map(Into::into),
            full_content: None,
            tool_calls: tool_calls.map(Into::into),
            tool_call_id: None,
            prompt_tokens: None,
            completion_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            thinking_tokens: None,
            thinking_content: None,
            created_at: None,
        }
    }

    // Ghost message: content=None, tool_calls=None — dropped
    let mut msgs = vec![msg("user", Some("hi"), None), msg("assistant", None, None)];
    prune_null_content_messages(&mut msgs);
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].role, Role::User);

    // No content but has tool_calls — kept (valid tool-use turn)
    let mut msgs = vec![
        msg("user", Some("hi"), None),
        msg("assistant", None, Some(r#"[{"id":"t1"}]"#)),
    ];
    prune_null_content_messages(&mut msgs);
    assert_eq!(msgs.len(), 2);

    // Normal assistant with content — kept
    let mut msgs = vec![
        msg("user", Some("hi"), None),
        msg("assistant", Some("Hello!"), None),
    ];
    prune_null_content_messages(&mut msgs);
    assert_eq!(msgs.len(), 2);

    // Non-assistant null content — kept (user/tool roles not touched)
    let mut msgs = vec![msg("tool", None, None)];
    prune_null_content_messages(&mut msgs);
    assert_eq!(msgs.len(), 1);

    // Empty vec — no crash
    let mut empty: Vec<Message> = vec![];
    prune_null_content_messages(&mut empty);
    assert!(empty.is_empty());
}

// ── prune_whitespace_only_messages tests (#594) ───────────────────────────

#[test]
fn test_prune_whitespace_only_drops_blank_assistant() {
    fn msg(role: &str, content: Option<&str>, tool_calls: Option<&str>) -> Message {
        Message {
            id: 0,
            session_id: String::new(),
            role: role.parse().unwrap_or(Role::User),
            content: content.map(Into::into),
            full_content: None,
            tool_calls: tool_calls.map(Into::into),
            tool_call_id: None,
            prompt_tokens: None,
            completion_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            thinking_tokens: None,
            thinking_content: None,
            created_at: None,
        }
    }

    // Whitespace-only assistant content — dropped
    let mut msgs = vec![
        msg("user", Some("hi"), None),
        msg("assistant", Some("   \n\n  "), None),
    ];
    prune_whitespace_only_messages(&mut msgs);
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].role, Role::User);

    // Single newline — dropped
    let mut msgs = vec![msg("assistant", Some("\n"), None)];
    prune_whitespace_only_messages(&mut msgs);
    assert!(msgs.is_empty());

    // Whitespace content but has tool_calls — kept
    let mut msgs = vec![msg("assistant", Some(" "), Some(r#"[{"id":"t1"}]"#))];
    prune_whitespace_only_messages(&mut msgs);
    assert_eq!(msgs.len(), 1);

    // Real content — kept
    let mut msgs = vec![msg("assistant", Some("Done."), None)];
    prune_whitespace_only_messages(&mut msgs);
    assert_eq!(msgs.len(), 1);

    // User with whitespace — kept (only assistant is pruned)
    let mut msgs = vec![msg("user", Some(" "), None)];
    prune_whitespace_only_messages(&mut msgs);
    assert_eq!(msgs.len(), 1);
}

// ── mark_message_complete integration test (#594) ─────────────────────────

#[tokio::test]
async fn test_mark_message_complete_sets_timestamp() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    let msg_id = db
        .insert_message(&session, &Role::Assistant, Some("hello"), None, None, None)
        .await
        .unwrap();

    // Verify completed_at is NULL before marking complete
    let row: (Option<String>,) = sqlx::query_as("SELECT completed_at FROM messages WHERE id = ?")
        .bind(msg_id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert!(row.0.is_none(), "completed_at should start NULL");

    db.mark_message_complete(msg_id).await.unwrap();

    let row: (Option<String>,) = sqlx::query_as("SELECT completed_at FROM messages WHERE id = ?")
        .bind(msg_id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert!(
        row.0.is_some(),
        "completed_at should be set after marking complete"
    );
}

// ── detect_interruption (#594) ──────────────────────────────

fn msg(role: Role, content: &str) -> Message {
    Message {
        id: 0,
        session_id: String::new(),
        role,
        content: Some(content.to_string()),
        full_content: None,
        tool_calls: None,
        tool_call_id: None,
        prompt_tokens: None,
        completion_tokens: None,
        cache_read_tokens: None,
        cache_creation_tokens: None,
        thinking_tokens: None,
        thinking_content: None,
        created_at: None,
    }
}

#[test]
fn detect_interruption_clean_end() {
    let msgs = vec![msg(Role::User, "hello"), msg(Role::Assistant, "hi there")];
    assert_eq!(detect_interruption(&msgs), None);
}

#[test]
fn detect_interruption_unanswered_prompt() {
    let msgs = vec![
        msg(Role::Assistant, "done"),
        msg(Role::User, "do something else"),
    ];
    assert_eq!(
        detect_interruption(&msgs),
        Some(InterruptionKind::Prompt("do something else".into()))
    );
}

#[test]
fn detect_interruption_orphaned_tool_result() {
    let mut tool_msg = msg(Role::Tool, "ok");
    tool_msg.tool_call_id = Some("call_123".into());
    let msgs = vec![msg(Role::Assistant, "calling tool"), tool_msg];
    assert_eq!(detect_interruption(&msgs), Some(InterruptionKind::Tool));
}

#[test]
fn detect_interruption_skips_system() {
    let msgs = vec![
        msg(Role::User, "hello"),
        msg(Role::Assistant, "hi"),
        msg(Role::System, "injected context"),
    ];
    // System message at the end should be ignored — assistant is last real msg
    assert_eq!(detect_interruption(&msgs), None);
}

#[test]
fn detect_interruption_prompt_truncated() {
    let long = "x".repeat(200);
    let msgs = vec![msg(Role::User, &long)];
    match detect_interruption(&msgs) {
        Some(InterruptionKind::Prompt(preview)) => {
            assert_eq!(preview.len(), 80, "preview should truncate to 80 chars");
        }
        other => panic!("expected Prompt, got {other:?}"),
    }
}

#[test]
fn detect_interruption_empty() {
    assert_eq!(detect_interruption(&[]), None);
}

// ── DB methods: extended coverage ────────────────────────────────────────────

#[tokio::test]
async fn test_insert_message_with_agent() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    let id = db
        .insert_message_with_agent(
            &session,
            &Role::Assistant,
            Some("hello from sub-agent"),
            None,
            None,
            None,
            Some("research-agent"),
        )
        .await
        .unwrap();
    assert!(id > 0);
    db.mark_message_complete(id).await.unwrap();

    let msgs = db.load_context(&session).await.unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].content.as_deref(), Some("hello from sub-agent"));
}

#[tokio::test]
async fn test_insert_tool_message_with_full() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    // Insert an assistant message with a tool call.
    let tc_json = r#"[{"id":"tc_1","function_name":"Read","arguments":"{}"}]"#;
    db.insert_message(&session, &Role::Assistant, None, Some(tc_json), None, None)
        .await
        .unwrap();

    // Insert tool result with full_output.
    let id = db
        .insert_tool_message_with_full(
            &session,
            "short result",
            "tc_1",
            "very long full output that was truncated",
        )
        .await
        .unwrap();
    assert!(id > 0);

    // load_all_messages returns everything (load_context may prune orphans).
    let msgs = db.load_all_messages(&session).await.unwrap();
    let tool_msg = msgs.iter().find(|m| m.role == Role::Tool).unwrap();
    assert_eq!(tool_msg.content.as_deref(), Some("short result"));
    assert_eq!(tool_msg.tool_call_id.as_deref(), Some("tc_1"));
}

#[tokio::test]
async fn test_load_all_messages_includes_compacted() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    // Insert several messages.
    for i in 0..5 {
        db.insert_message(
            &session,
            &Role::User,
            Some(&format!("msg {i}")),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    }

    // Compact (keep last 2 — means preserve_count=2 messages from the tail).
    db.compact_session(&session, "summary", 2).await.unwrap();

    // load_context should return fewer than the original 5.
    let active = db.load_context(&session).await.unwrap();
    assert!(
        active.len() < 5,
        "active should be < 5, got {}",
        active.len()
    );

    // load_all_messages should return everything (active + compacted).
    let all = db.load_all_messages(&session).await.unwrap();
    assert!(
        all.len() >= active.len(),
        "all({}) should >= active({})",
        all.len(),
        active.len()
    );
}

#[tokio::test]
async fn test_recent_user_messages() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    db.insert_message(&session, &Role::User, Some("first"), None, None, None)
        .await
        .unwrap();
    db.insert_message(&session, &Role::User, Some("second"), None, None, None)
        .await
        .unwrap();
    db.insert_message(&session, &Role::User, Some("third"), None, None, None)
        .await
        .unwrap();

    let recent = db.recent_user_messages(2).await.unwrap();
    assert_eq!(recent.len(), 2);
    // Most recent first.
    assert_eq!(recent[0], "third");
    assert_eq!(recent[1], "second");
}

#[tokio::test]
async fn test_last_user_message() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    db.insert_message(&session, &Role::User, Some("hey"), None, None, None)
        .await
        .unwrap();
    db.insert_message(&session, &Role::Assistant, Some("yo"), None, None, None)
        .await
        .unwrap();
    db.insert_message(&session, &Role::User, Some("latest"), None, None, None)
        .await
        .unwrap();

    let last = db.last_user_message(&session).await.unwrap();
    assert_eq!(last, "latest");
}

#[tokio::test]
async fn test_session_mode_roundtrip() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    // Initially None.
    let mode = db.get_session_mode(&session).await.unwrap();
    assert!(mode.is_none());

    // Set and get.
    db.set_session_mode(&session, "confirm").await.unwrap();
    let mode = db.get_session_mode(&session).await.unwrap();
    assert_eq!(mode.as_deref(), Some("confirm"));

    // Overwrite.
    db.set_session_mode(&session, "auto").await.unwrap();
    let mode = db.get_session_mode(&session).await.unwrap();
    assert_eq!(mode.as_deref(), Some("auto"));
}

#[tokio::test]
async fn test_set_session_title() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    db.set_session_title(&session, "My Cool Session")
        .await
        .unwrap();

    let sessions = db.list_sessions(10, _tmp.path()).await.unwrap();
    let found = sessions.iter().find(|s| s.id == session).unwrap();
    assert_eq!(found.title.as_deref(), Some("My Cool Session"));
}

#[tokio::test]
async fn test_get_session_idle_secs() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    // Fresh session — insert a message to set a timestamp.
    db.insert_message(&session, &Role::User, Some("hi"), None, None, None)
        .await
        .unwrap();

    let idle = db.get_session_idle_secs(&session).await.unwrap();
    // Could be None or Some depending on DB impl, but if Some it should be small.
    if let Some(secs) = idle {
        assert!(secs < 5, "just created, idle: {secs}");
    }
    // Test passes either way — we're just verifying no crash.
}

#[tokio::test]
async fn test_clear_message_content() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    let id1 = db
        .insert_message(&session, &Role::User, Some("secret"), None, None, None)
        .await
        .unwrap();
    let id2 = db
        .insert_message(
            &session,
            &Role::Assistant,
            Some("response"),
            None,
            None,
            None,
        )
        .await
        .unwrap();

    db.clear_message_content(&[id1, id2], "[redacted]")
        .await
        .unwrap();

    let msgs = db.load_all_messages(&session).await.unwrap();
    for msg in &msgs {
        assert_eq!(
            msg.content.as_deref(),
            Some("[redacted]"),
            "msg {:?} should be redacted",
            msg.role
        );
    }
}

#[tokio::test]
async fn test_compacted_stats() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    // Initially zero.
    let stats = db.compacted_stats().await.unwrap();
    assert_eq!(stats.message_count, 0);

    // Create some messages and compact.
    for i in 0..10 {
        db.insert_message(
            &session,
            &Role::User,
            Some(&format!("msg {i}")),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    }
    db.compact_session(&session, "summary", 2).await.unwrap();

    let stats = db.compacted_stats().await.unwrap();
    assert!(stats.message_count > 0, "should have compacted messages");
}

#[tokio::test]
async fn test_session_usage_by_agent() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    let usage = crate::providers::TokenUsage {
        prompt_tokens: 100,
        completion_tokens: 50,
        ..Default::default()
    };

    // Insert messages from different agents.
    db.insert_message_with_agent(
        &session,
        &Role::Assistant,
        Some("main response"),
        None,
        None,
        Some(&usage),
        None, // default agent
    )
    .await
    .unwrap();

    db.insert_message_with_agent(
        &session,
        &Role::Assistant,
        Some("sub response"),
        None,
        None,
        Some(&usage),
        Some("research"),
    )
    .await
    .unwrap();

    let by_agent = db.session_usage_by_agent(&session).await.unwrap();
    assert!(
        !by_agent.is_empty(),
        "should track at least 1 agent: {by_agent:?}"
    );
}

#[tokio::test]
async fn test_purge_compacted() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    for i in 0..10 {
        db.insert_message(
            &session,
            &Role::User,
            Some(&format!("msg {i}")),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    }
    db.compact_session(&session, "summary", 2).await.unwrap();
    // Purge with 0 min_age_days to catch everything.
    let purged = db.purge_compacted(0).await.unwrap();
    assert!(purged > 0, "should purge some messages");

    // After purge, compacted stats should be lower.
    let stats = db.compacted_stats().await.unwrap();
    assert_eq!(stats.message_count, 0, "all compacted should be purged");
}

// ── config_dir ─────────────────────────────────────────────────────────

/// Serialize tests that mutate XDG_CONFIG_HOME (in the runtime-env map).
static XDG_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn test_config_dir_with_xdg() {
    let _guard = XDG_MUTEX.lock().unwrap();
    // **#1109 F1**: was `unsafe { std::env::set_var(...) }`. Now uses
    // the thread-safe runtime-env map; production [`config_dir`] reads
    // via [`crate::runtime_env::get`] so the override is observed.
    crate::runtime_env::set("XDG_CONFIG_HOME", "/tmp/test_xdg_config");
    let dir = super::config_dir().unwrap();
    crate::runtime_env::remove("XDG_CONFIG_HOME");
    // Always ends with "koda"
    assert!(dir.ends_with("koda"), "got: {dir:?}");
    assert!(
        dir.to_string_lossy().contains("test_xdg_config"),
        "should use XDG_CONFIG_HOME: {dir:?}"
    );
}

#[test]
fn test_config_dir_with_home() {
    let _guard = XDG_MUTEX.lock().unwrap();
    // **#1109 F1**: mask hides any developer-exported XDG_CONFIG_HOME
    // from production code so we exercise the HOME fallback branch.
    // Uses the runtime-env mask facility — no `unsafe`, no std::env mutation.
    crate::runtime_env::remove("XDG_CONFIG_HOME");
    crate::runtime_env::mask("XDG_CONFIG_HOME");
    let dir = super::config_dir().unwrap();
    crate::runtime_env::unmask("XDG_CONFIG_HOME");
    // Should end with koda regardless of base path
    assert!(dir.ends_with("koda"), "got: {dir:?}");
}

// ── kv store ───────────────────────────────────────────────────────────

#[tokio::test]
async fn test_kv_set_get_delete() {
    let (db, _tmp) = setup().await;
    // Initially absent
    assert!(db.kv_get("my_key").await.unwrap().is_none());
    // Set
    db.kv_set("my_key", "hello").await.unwrap();
    assert_eq!(db.kv_get("my_key").await.unwrap().as_deref(), Some("hello"));
    // Overwrite
    db.kv_set("my_key", "updated").await.unwrap();
    assert_eq!(
        db.kv_get("my_key").await.unwrap().as_deref(),
        Some("updated")
    );
    // Delete
    db.kv_delete("my_key").await.unwrap();
    assert!(db.kv_get("my_key").await.unwrap().is_none());
}

#[tokio::test]
async fn test_kv_list_prefix() {
    let (db, _tmp) = setup().await;
    db.kv_set("cfg:foo", "1").await.unwrap();
    db.kv_set("cfg:bar", "2").await.unwrap();
    db.kv_set("other:baz", "3").await.unwrap();
    let items = db.kv_list_prefix("cfg:").await.unwrap();
    assert_eq!(items.len(), 2);
    let keys: Vec<&str> = items.iter().map(|(k, _)| k.as_str()).collect();
    assert!(keys.contains(&"cfg:foo"));
    assert!(keys.contains(&"cfg:bar"));
    assert!(!keys.contains(&"other:baz"));
}

// ── thinking_content (#819) ────────────────────────────────────────────

#[tokio::test]
async fn thinking_content_round_trip() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    let id = db
        .insert_message(&session, &Role::Assistant, Some("answer"), None, None, None)
        .await
        .unwrap();
    db.mark_message_complete(id).await.unwrap();

    db.update_message_thinking_content(id, "I should think carefully about this…")
        .await
        .unwrap();

    let msgs = db.load_context(&session).await.unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(
        msgs[0].thinking_content.as_deref(),
        Some("I should think carefully about this…"),
        "thinking_content should survive a round-trip through the DB"
    );
}

#[tokio::test]
async fn thinking_content_null_by_default() {
    // Messages for non-Claude models should have NULL thinking_content.
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    db.insert_message(&session, &Role::User, Some("hello"), None, None, None)
        .await
        .unwrap();
    db.insert_message(&session, &Role::Assistant, Some("hi"), None, None, None)
        .await
        .unwrap();

    let msgs = db.load_context(&session).await.unwrap();
    for msg in &msgs {
        assert!(
            msg.thinking_content.is_none(),
            "thinking_content should be None when never written (role: {:?})",
            msg.role
        );
    }
}

#[tokio::test]
async fn thinking_content_persists_and_is_loaded_in_context() {
    // Simulates session resume: write thinking content, then re-load context.
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    db.insert_message(
        &session,
        &Role::User,
        Some("what is 2+2?"),
        None,
        None,
        None,
    )
    .await
    .unwrap();

    let assistant_id = db
        .insert_message(
            &session,
            &Role::Assistant,
            Some("It is 4."),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    db.mark_message_complete(assistant_id).await.unwrap();

    db.update_message_thinking_content(assistant_id, "2+2=4 trivially")
        .await
        .unwrap();

    // Reload context (as session resume would).
    let msgs = db.load_context(&session).await.unwrap();
    let assistant = msgs.iter().find(|m| m.role == Role::Assistant).unwrap();
    assert_eq!(
        assistant.thinking_content.as_deref(),
        Some("2+2=4 trivially"),
        "thinking_content must survive context reload (session resume path)"
    );
}

// ── Incomplete assistant messages excluded from load_context (#875, #877) ──

/// Assistant messages without `completed_at` (interrupted/network-error turns)
/// must be excluded from `load_context()` so the model gets a clean slate —
/// as if the interrupted turn never happened. They should still be present
/// in `load_all_messages()` for history/recall purposes.
#[tokio::test]
async fn load_context_excludes_incomplete_assistant_messages() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    // User sends a message
    db.insert_message(
        &session,
        &Role::User,
        Some("research inference"),
        None,
        None,
        None,
    )
    .await
    .unwrap();

    // Assistant starts responding but is interrupted (no mark_message_complete)
    let _incomplete_id = db
        .insert_message(
            &session,
            &Role::Assistant,
            Some("I'll start by looking at"),
            None,
            None,
            None,
        )
        .await
        .unwrap();

    // load_context should exclude the incomplete assistant message
    let context = db.load_context(&session).await.unwrap();
    assert_eq!(
        context.len(),
        1,
        "only the user message should be in context"
    );
    assert_eq!(context[0].role, Role::User);
    assert_eq!(context[0].content.as_deref(), Some("research inference"));

    // load_all_messages should still include it (for history/recall)
    let all = db.load_all_messages(&session).await.unwrap();
    assert_eq!(all.len(), 2, "both messages should be in full history");
}

/// Completed assistant messages (with `completed_at` set) must still appear
/// in `load_context()` — only incomplete ones are excluded.
#[tokio::test]
async fn load_context_includes_completed_assistant_messages() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    db.insert_message(&session, &Role::User, Some("hello"), None, None, None)
        .await
        .unwrap();

    let msg_id = db
        .insert_message(
            &session,
            &Role::Assistant,
            Some("hi there!"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    db.mark_message_complete(msg_id).await.unwrap();

    let context = db.load_context(&session).await.unwrap();
    assert_eq!(
        context.len(),
        2,
        "completed assistant message must be in context"
    );
    assert_eq!(context[1].role, Role::Assistant);
    assert_eq!(context[1].content.as_deref(), Some("hi there!"));
}

/// After an interrupted turn, detect_interruption should see the user message
/// as the last relevant message (the incomplete assistant was filtered out).
#[tokio::test]
async fn interrupted_turn_detected_after_incomplete_filtered() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    // Complete first exchange
    db.insert_message(&session, &Role::User, Some("hello"), None, None, None)
        .await
        .unwrap();
    let mid = db
        .insert_message(&session, &Role::Assistant, Some("hi!"), None, None, None)
        .await
        .unwrap();
    db.mark_message_complete(mid).await.unwrap();

    // Second turn: user sends, assistant interrupted
    db.insert_message(
        &session,
        &Role::User,
        Some("research inference"),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    db.insert_message(
        &session,
        &Role::Assistant,
        Some("partial response"),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    // No mark_message_complete — simulates Ctrl+C

    let context = db.load_context(&session).await.unwrap();
    // Should be: [user "hello", assistant "hi!", user "research inference"]
    assert_eq!(context.len(), 3, "incomplete assistant should be filtered");

    let interruption = detect_interruption(&context);
    assert!(
        matches!(interruption, Some(InterruptionKind::Prompt(_))),
        "should detect unanswered user prompt; got {interruption:?}"
    );
}

// ── #1022 B20: copy_messages_into_session ────────────────────────────────
//
// Tests the atomic batch-copy method introduced for fork sub-agent
// dispatch. Covers:
//
// - **Empty input is a no-op** (early return; doesn't touch the WAL).
// - **All roles are copied verbatim** (User/Assistant/Tool).
// - **Assistant rows are born complete** (`completed_at` set inline,
//   so `load_context` includes them without a follow-up
//   `mark_message_complete` round-trip).
// - **Order is preserved** (load_context returns them in the same
//   sequence).
// - **No usage stats leak across sessions** (parent token usage is
//   not double-counted on the child; matches the pre-fix loop's
//   `usage = None` policy).
// - **Tool-call wiring survives the round-trip** (tool_calls JSON
//   and tool_call_id columns are preserved so the model's
//   tool_use/tool_result pairs aren't broken in the fork).

#[tokio::test]
async fn test_copy_messages_empty_is_noop() {
    let (db, _tmp) = setup().await;
    let dst = db.create_session("default", _tmp.path()).await.unwrap();

    db.copy_messages_into_session(&dst, &[]).await.unwrap();

    let loaded = db.load_context(&dst).await.unwrap();
    assert!(loaded.is_empty(), "empty copy must leave session empty");
}

#[tokio::test]
async fn test_copy_messages_preserves_roles_and_order() {
    let (db, _tmp) = setup().await;
    let src = db.create_session("default", _tmp.path()).await.unwrap();
    let dst = db.create_session("default", _tmp.path()).await.unwrap();

    db.insert_message(&src, &Role::User, Some("first"), None, None, None)
        .await
        .unwrap();
    insert_complete_assistant(&db, &src, Some("second"), None).await;
    db.insert_message(&src, &Role::User, Some("third"), None, None, None)
        .await
        .unwrap();
    insert_complete_assistant(&db, &src, Some("fourth"), None).await;

    let history = db.load_context(&src).await.unwrap();
    assert_eq!(history.len(), 4, "src should have 4 messages");

    db.copy_messages_into_session(&dst, &history).await.unwrap();

    let copied = db.load_context(&dst).await.unwrap();
    assert_eq!(copied.len(), 4, "dst must contain all 4 messages");

    let texts: Vec<&str> = copied.iter().filter_map(|m| m.content.as_deref()).collect();
    assert_eq!(
        texts,
        vec!["first", "second", "third", "fourth"],
        "order must be preserved across the batch copy"
    );

    let roles: Vec<&str> = copied.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(roles, vec!["user", "assistant", "user", "assistant"]);
}

#[tokio::test]
async fn test_copy_messages_assistant_rows_are_born_complete() {
    // The bug this prevents: pre-fix needed a follow-up
    // `mark_message_complete` per assistant row, otherwise
    // `load_context` would filter the copied assistant rows out
    // (`role != 'assistant' OR completed_at IS NOT NULL`). If the
    // CASE-based inline `completed_at` ever regresses to NULL, this
    // test catches it because `load_context` would return only the
    // user rows.
    let (db, _tmp) = setup().await;
    let src = db.create_session("default", _tmp.path()).await.unwrap();
    let dst = db.create_session("default", _tmp.path()).await.unwrap();

    db.insert_message(&src, &Role::User, Some("q1"), None, None, None)
        .await
        .unwrap();
    insert_complete_assistant(&db, &src, Some("a1"), None).await;
    db.insert_message(&src, &Role::User, Some("q2"), None, None, None)
        .await
        .unwrap();
    insert_complete_assistant(&db, &src, Some("a2"), None).await;

    let history = db.load_context(&src).await.unwrap();
    db.copy_messages_into_session(&dst, &history).await.unwrap();

    let loaded = db.load_context(&dst).await.unwrap();
    let assistant_count = loaded.iter().filter(|m| m.role == Role::Assistant).count();
    assert_eq!(
        assistant_count, 2,
        "both assistant rows must survive load_context's \
         `completed_at IS NOT NULL` filter \u{2014} if this is 0, the inline \
         `completed_at` write regressed and assistant rows are being \
         persisted as incomplete"
    );
}

#[tokio::test]
async fn test_copy_messages_preserves_tool_call_wiring() {
    // tool_calls JSON and tool_call_id columns must survive the
    // copy or the model's tool_use/tool_result pairing in the fork
    // session would be broken (load_context would prune them via
    // `prune_mismatched_tool_calls`).
    let (db, _tmp) = setup().await;
    let src = db.create_session("default", _tmp.path()).await.unwrap();
    let dst = db.create_session("default", _tmp.path()).await.unwrap();

    let tool_calls_json = r#"[{"id":"tc_1","function":{"name":"Read","arguments":"{}"}}]"#;

    db.insert_message(&src, &Role::User, Some("read foo.txt"), None, None, None)
        .await
        .unwrap();
    insert_complete_assistant(&db, &src, None, Some(tool_calls_json)).await;
    db.insert_message(
        &src,
        &Role::Tool,
        Some("file contents"),
        None,
        Some("tc_1"),
        None,
    )
    .await
    .unwrap();

    let history = db.load_context(&src).await.unwrap();
    db.copy_messages_into_session(&dst, &history).await.unwrap();

    let loaded = db.load_context(&dst).await.unwrap();
    assert_eq!(loaded.len(), 3, "user + assistant + tool must all survive");

    let assistant = loaded.iter().find(|m| m.role == Role::Assistant).unwrap();
    assert_eq!(
        assistant.tool_calls.as_deref(),
        Some(tool_calls_json),
        "tool_calls JSON must round-trip unchanged"
    );

    let tool = loaded.iter().find(|m| m.role == Role::Tool).unwrap();
    assert_eq!(
        tool.tool_call_id.as_deref(),
        Some("tc_1"),
        "tool_call_id must round-trip unchanged"
    );
}

#[tokio::test]
async fn test_copy_messages_does_not_touch_source_session() {
    // Copying out of `src` must not mutate `src` \u{2014} sanity check
    // for the SQL (no accidental DELETE, no role swap).
    let (db, _tmp) = setup().await;
    let src = db.create_session("default", _tmp.path()).await.unwrap();
    let dst = db.create_session("default", _tmp.path()).await.unwrap();

    db.insert_message(&src, &Role::User, Some("hello"), None, None, None)
        .await
        .unwrap();
    insert_complete_assistant(&db, &src, Some("world"), None).await;

    let before = db.load_context(&src).await.unwrap();
    let history = before.clone();
    db.copy_messages_into_session(&dst, &history).await.unwrap();
    let after = db.load_context(&src).await.unwrap();

    assert_eq!(
        before.len(),
        after.len(),
        "source session message count must not change"
    );
    let before_texts: Vec<_> = before.iter().filter_map(|m| m.content.as_deref()).collect();
    let after_texts: Vec<_> = after.iter().filter_map(|m| m.content.as_deref()).collect();
    assert_eq!(
        before_texts, after_texts,
        "source content must be unchanged"
    );
}

// ── Session events (#1108 P1b/P2a) ──────────────────────────────────────────
//
// Round-trip the new `session_events` table. The schema lives in
// `db/mod.rs`; the trait methods (`insert_session_event` /
// `load_session_events`) live in `db/queries.rs`. These tests pin
// the contract end-to-end: insert order is preserved, parent-link
// is nullable, and unrelated sessions don't leak into each other.

use crate::persistence::session_event_kind;

#[tokio::test]
async fn session_events_round_trip_preserves_order_and_parent() {
    let (db, _tmp) = setup().await;
    let session = db
        .create_session("test-agent", std::path::Path::new("/tmp"))
        .await
        .unwrap();

    // Three events, two top-level + one parented. Insertion order
    // should equal load order — `load_session_events` orders by id
    // (auto-increment), which is monotonic per-insert.
    db.insert_session_event(&session, session_event_kind::INFO, "first", None)
        .await
        .unwrap();
    db.insert_session_event(
        &session,
        session_event_kind::SUB_AGENT_EVENT,
        "  🔧 Read",
        Some("call_abc"),
    )
    .await
    .unwrap();
    db.insert_session_event(
        &session,
        session_event_kind::BG_TASK_UPDATE,
        r#"{"task_id":1,"status":"Pending"}"#,
        None,
    )
    .await
    .unwrap();

    let events = db.load_session_events(&session).await.unwrap();
    assert_eq!(events.len(), 3, "all three events must round-trip");
    assert_eq!(events[0].payload, "first");
    assert_eq!(events[0].parent_tool_call_id, None);
    assert_eq!(events[1].kind, session_event_kind::SUB_AGENT_EVENT);
    assert_eq!(events[1].parent_tool_call_id.as_deref(), Some("call_abc"));
    assert_eq!(events[2].kind, session_event_kind::BG_TASK_UPDATE);
}

#[tokio::test]
async fn session_events_isolated_per_session() {
    let (db, _tmp) = setup().await;
    let s1 = db
        .create_session("agent-1", std::path::Path::new("/tmp"))
        .await
        .unwrap();
    let s2 = db
        .create_session("agent-2", std::path::Path::new("/tmp"))
        .await
        .unwrap();

    db.insert_session_event(&s1, session_event_kind::INFO, "in s1", None)
        .await
        .unwrap();
    db.insert_session_event(&s2, session_event_kind::INFO, "in s2", None)
        .await
        .unwrap();

    let s1_events = db.load_session_events(&s1).await.unwrap();
    let s2_events = db.load_session_events(&s2).await.unwrap();
    assert_eq!(s1_events.len(), 1);
    assert_eq!(s2_events.len(), 1);
    assert_eq!(s1_events[0].payload, "in s1");
    assert_eq!(s2_events[0].payload, "in s2");
}

#[tokio::test]
async fn session_events_empty_when_none_inserted() {
    let (db, _tmp) = setup().await;
    let session = db
        .create_session("test-agent", std::path::Path::new("/tmp"))
        .await
        .unwrap();
    let events = db.load_session_events(&session).await.unwrap();
    assert!(events.is_empty(), "fresh session must have no events");
}

// ─────────────────────────────────────────────────────────────────────
// (#1166) Per-session context cache correctness.
//
// The cache lives in `db/context_cache.rs`. These tests verify that
// every mutation path which can affect what `load_context` returns is
// correctly observed by subsequent `load_context` calls. Each test
// covers one cache invariant from the module-level docs.
// ─────────────────────────────────────────────────────────────────────

/// Cache HIT with no new inserts must return byte-identical data to the
/// uncached path. Sanity floor: the cache must not change semantics.
#[tokio::test]
async fn ctx_cache_hit_returns_same_data_as_full_load() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();
    db.insert_message(&session, &Role::User, Some("hi"), None, None, None)
        .await
        .unwrap();
    insert_complete_assistant(&db, &session, Some("hello"), None).await;

    let first = db.load_context(&session).await.unwrap();
    let second = db.load_context(&session).await.unwrap(); // cache hit
    assert_eq!(first.len(), second.len());
    for (a, b) in first.iter().zip(second.iter()) {
        assert_eq!(a.id, b.id);
        assert_eq!(a.content, b.content);
        assert_eq!(a.role, b.role);
    }
}

/// Cache HIT + new tool-result row → delta-fetch must surface the new
/// row in the next `load_context` call.
#[tokio::test]
async fn ctx_cache_delta_picks_up_new_tool_results() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();
    db.insert_message(&session, &Role::User, Some("read foo"), None, None, None)
        .await
        .unwrap();
    let tc_json = r#"[{"id":"tc_1","function_name":"Read","arguments":"{}"}]"#;
    insert_complete_assistant(&db, &session, None, Some(tc_json)).await;

    // Prime the cache.
    let before = db.load_context(&session).await.unwrap();
    // The assistant has a tool_call with no matching tool result, so
    // `prune_mismatched_tool_calls` strips it. Pre-tool: just the user.
    assert_eq!(before.len(), 1);

    // Append a tool result. This is the inference loop's hot path.
    db.insert_message(
        &session,
        &Role::Tool,
        Some("file contents"),
        None,
        Some("tc_1"),
        None,
    )
    .await
    .unwrap();

    // Now the assistant's tool_call has a match — both the assistant
    // AND the tool row are valid. Delta-fetch picks them up; sanitization
    // re-runs on the merged vec and stops pruning the assistant.
    let after = db.load_context(&session).await.unwrap();
    assert_eq!(after.len(), 3);
    assert_eq!(after[0].role, Role::User);
    assert_eq!(after[1].role, Role::Assistant);
    assert_eq!(after[2].role, Role::Tool);
    assert_eq!(after[2].content.as_deref(), Some("file contents"));
}

/// An assistant row inserted as incomplete is filtered out of
/// `load_context` until `mark_message_complete` runs. The next
/// `load_context` call must surface the now-complete row via the delta
/// query (id > max_id_returned), since incomplete rows do NOT
/// contribute to `max_id_returned`.
#[tokio::test]
async fn ctx_cache_delta_picks_up_newly_completed_assistant() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();
    db.insert_message(&session, &Role::User, Some("hi"), None, None, None)
        .await
        .unwrap();
    // Insert assistant WITHOUT marking complete (mid-stream state).
    let asst_id = db
        .insert_message(
            &session,
            &Role::Assistant,
            Some("partial"),
            None,
            None,
            None,
        )
        .await
        .unwrap();

    // First load: filters out the incomplete assistant. Cache snapshot
    // captures only the user row.
    let first = db.load_context(&session).await.unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].role, Role::User);

    // Stream completes.
    db.mark_message_complete(asst_id).await.unwrap();

    // Second load: delta query (id > user_row.id) re-evaluates the
    // assistant — it now passes the filter and is appended.
    let second = db.load_context(&session).await.unwrap();
    assert_eq!(second.len(), 2);
    assert_eq!(second[1].role, Role::Assistant);
    assert_eq!(second[1].content.as_deref(), Some("partial"));
}

/// `compact_session` sets `compacted_at` on existing rows — a
/// retroactive filter change that the cached message vector cannot
/// observe via delta-fetch. The compaction-gen bump must force a full
/// reload on the next `load_context`.
#[tokio::test]
async fn ctx_cache_compaction_forces_full_reload() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();
    // Pad enough rows to give compact_session something to archive.
    for i in 0..10 {
        db.insert_message(
            &session,
            &Role::User,
            Some(&format!("msg {i}")),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    }
    let before = db.load_context(&session).await.unwrap();
    assert_eq!(before.len(), 10);

    // Archive the first 7, keeping the tail of 3. `compact_session`
    // inserts TWO synthetic rows (summary + continuation hint) AFTER
    // the kept tail — since rows are ordered by id ASC and the new
    // rows get the highest ids. Post-compaction load returns 5 rows:
    // [3 tail user rows, system summary, assistant continuation].
    let archived = db.compact_session(&session, "summary", 3).await.unwrap();
    assert_eq!(archived, 7);

    let after = db.load_context(&session).await.unwrap();
    assert_eq!(after.len(), 5);
    // Tail of original messages preserved (oldest of these is "msg 7").
    assert_eq!(after[0].role, Role::User);
    assert_eq!(after[0].content.as_deref(), Some("msg 7"));
    // Synthetic summary + continuation appended.
    assert_eq!(after[3].role, Role::System);
    assert_eq!(after[3].content.as_deref(), Some("summary"));
    assert_eq!(after[4].role, Role::Assistant);
}

/// Microcompact rewrites tool-result content in place via
/// `clear_message_content`. The cached snapshot holds the pre-clear
/// content, so the gen bump in `clear_message_content` must invalidate
/// it.
#[tokio::test]
async fn ctx_cache_clear_message_content_forces_full_reload() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();
    let tc_json = r#"[{"id":"tc_1","function_name":"Read","arguments":"{}"}]"#;
    insert_complete_assistant(&db, &session, None, Some(tc_json)).await;
    let tool_id = db
        .insert_message(
            &session,
            &Role::Tool,
            Some("LARGE original content"),
            None,
            Some("tc_1"),
            None,
        )
        .await
        .unwrap();

    // Prime cache with original content.
    let before = db.load_context(&session).await.unwrap();
    assert_eq!(
        before
            .iter()
            .find(|m| m.id == tool_id)
            .unwrap()
            .content
            .as_deref(),
        Some("LARGE original content")
    );

    // Microcompact stub-rewrite.
    db.clear_message_content(&[tool_id], "[cleared]")
        .await
        .unwrap();

    // Without invalidation, this would return stale content.
    let after = db.load_context(&session).await.unwrap();
    assert_eq!(
        after
            .iter()
            .find(|m| m.id == tool_id)
            .unwrap()
            .content
            .as_deref(),
        Some("[cleared]")
    );
}

/// `delete_session` invalidates only the deleted session's cache entry;
/// other sessions' caches are untouched.
#[tokio::test]
async fn ctx_cache_delete_session_invalidates_only_target() {
    let (db, _tmp) = setup().await;
    let s1 = db.create_session("default", _tmp.path()).await.unwrap();
    let s2 = db.create_session("default", _tmp.path()).await.unwrap();
    db.insert_message(&s1, &Role::User, Some("from s1"), None, None, None)
        .await
        .unwrap();
    db.insert_message(&s2, &Role::User, Some("from s2"), None, None, None)
        .await
        .unwrap();

    // Prime both caches.
    assert_eq!(db.load_context(&s1).await.unwrap().len(), 1);
    assert_eq!(db.load_context(&s2).await.unwrap().len(), 1);

    db.delete_session(&s1).await.unwrap();

    // s1's session is gone; load returns empty (no cache poisoning).
    assert_eq!(db.load_context(&s1).await.unwrap().len(), 0);
    // s2 still works.
    assert_eq!(db.load_context(&s2).await.unwrap().len(), 1);
}
