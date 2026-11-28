//! Tests for the SQLite persistence layer.
//!
//! Covers the `SessionStore` trait methods on `Database`, message pruning
//! helpers (`prune_mismatched_tool_calls`, `prune_null_content_messages`,
//! `prune_whitespace_only_messages`), and interrupted turn detection.

use super::queries::{HistoryEdit, prune_mismatched_tool_calls, sanitize_history};
use super::queries::{
    dedupe_tool_results_by_call_id, prune_null_content_messages, prune_whitespace_only_messages,
};
use crate::db::Database;
use crate::session_store::{Message, Role, SessionStore};
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
    let usage1 = crate::session_store::MessageUsage {
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
    let usage2 = crate::session_store::MessageUsage {
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

    // Should have: summary(system) + 2 preserved = 3
    let msgs = db.load_context(&session).await.unwrap();
    assert_eq!(msgs.len(), 3);

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

    // Compaction puts no words in the model's mouth (#1444).
    assert!(
        !msgs.iter().any(|m| m.role == Role::Assistant
            && m.content.as_deref().unwrap_or("").contains("compacted")),
        "no assistant row may carry a compaction hint"
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
    assert_eq!(msgs.len(), 1); // summary only
    assert_eq!(msgs[0].role, Role::System);
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
            thinking_model: None,
            thinking_blocks: None,
            images: None,
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

    // Partial batch: dropping the assistant must take its finished sibling too
    let mut msgs = vec![
        msg("user", Some("hi"), None, None),
        msg(
            "assistant",
            None,
            Some(r#"[{"id":"t1"},{"id":"t2"}]"#),
            None,
        ),
        msg("tool", Some("ok"), None, Some("t1")),
    ];
    prune_mismatched_tool_calls(&mut msgs);
    assert_eq!(msgs.len(), 1, "partial batch should leave no orphan");
    assert_eq!(msgs[0].role, Role::User);
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
        thinking_model: None,
        thinking_blocks: None,
        images: None,
        created_at: None,
    }
}

#[test]
fn sanitize_reports_each_row_a_pass_removes() {
    let mut messages = vec![
        msg_with_id(1, "user", Some("task"), None),
        msg_with_id(2, "tool", Some("old result"), Some("orphan")),
        msg_with_id(3, "assistant", Some("   "), None),
        msg_with_id(4, "user", Some("next"), None),
    ];

    let edits = sanitize_history(&mut messages);

    let ids: Vec<i64> = messages.iter().map(|m| m.id).collect();
    assert_eq!(ids, vec![1, 4]);
    assert_eq!(
        edits,
        vec![
            HistoryEdit {
                pass: "prune_mismatched_tool_calls",
                ids: vec![2],
                tool_rows: 1,
                chars: 10,
            },
            HistoryEdit {
                pass: "prune_null_content_messages",
                ids: vec![3],
                tool_rows: 0,
                chars: 0,
            },
        ]
    );
}

#[test]
fn sanitize_reports_nothing_for_a_clean_history() {
    let mut messages = vec![msg_with_id(1, "user", Some("task"), None)];
    assert!(sanitize_history(&mut messages).is_empty());
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
    // feedback reports and forensic use.
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

/// The model's text next to a tool call is not its answer (#1444).
#[tokio::test]
async fn test_last_assistant_message_ignores_text_beside_tool_calls() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();
    db.insert_message(&session, &Role::User, Some("go"), None, None, None)
        .await
        .unwrap();
    insert_complete_assistant(
        &db,
        &session,
        Some("Let me look."),
        Some("[{\"id\":\"1\"}]"),
    )
    .await;

    assert_eq!(db.last_assistant_message(&session).await.unwrap(), "");
}

/// A model that ends on nothing leaves `response` empty, and an earlier
/// answer in the session does not stand in for it (#1444).
#[tokio::test]
async fn test_last_assistant_message_empty_final_is_empty_not_stale() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();
    db.insert_message(&session, &Role::User, Some("first"), None, None, None)
        .await
        .unwrap();
    insert_complete_assistant(&db, &session, Some("first answer"), None).await;
    db.insert_message(&session, &Role::User, Some("second"), None, None, None)
        .await
        .unwrap();
    insert_complete_assistant(&db, &session, None, Some("[{\"id\":\"1\"}]")).await;
    db.insert_message(&session, &Role::Tool, Some("ok"), None, Some("1"), None)
        .await
        .unwrap();
    insert_complete_assistant(&db, &session, None, None).await;
    assert_eq!(db.last_assistant_message(&session).await.unwrap(), "");

    insert_complete_assistant(&db, &session, Some(" \n\t"), None).await;
    assert_eq!(db.last_assistant_message(&session).await.unwrap(), "");
}

/// A session compacted before #1444 still loads, without the old hint row.
#[tokio::test]
async fn test_legacy_compaction_hint_row_is_ignored() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();
    db.insert_message(&session, &Role::User, Some("go"), None, None, None)
        .await
        .unwrap();
    insert_complete_assistant(&db, &session, Some("answer"), None).await;
    sqlx::query("INSERT INTO messages (session_id, role, content) VALUES (?, 'system', 'summary')")
        .bind(&session)
        .execute(&db.pool)
        .await
        .unwrap();
    let hint = "Your context was compacted. The previous message contains a summary of our \
                earlier conversation. Do not mention the summary or that compaction occurred. \
                Continue the conversation naturally based on the summarized context.";
    insert_complete_assistant(&db, &session, Some(hint), None).await;

    let msgs = db.load_context(&session).await.unwrap();
    let roles: Vec<_> = msgs.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(roles, ["user", "assistant", "system"]);

    // A later turn that ends on nothing cannot report it either.
    db.insert_message(&session, &Role::User, Some("again"), None, None, None)
        .await
        .unwrap();
    assert_eq!(db.last_assistant_message(&session).await.unwrap(), "");
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
            thinking_model: None,
            thinking_blocks: None,
            images: None,
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
            thinking_model: None,
            thinking_blocks: None,
            images: None,
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
async fn test_session_usage_by_agent() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    let usage = crate::session_store::MessageUsage {
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

/// A sub-agent's session counts toward the session that started it, under the
/// agent's name, and an unrelated session doesn't.
#[tokio::test]
async fn usage_includes_linked_sub_agent_sessions() {
    use crate::session_store::PARENT_SESSION_KEY;

    let (db, _tmp) = setup().await;
    let parent = db.create_session("default", _tmp.path()).await.unwrap();
    let child = db.create_session("explore", _tmp.path()).await.unwrap();
    let other = db.create_session("default", _tmp.path()).await.unwrap();
    db.set_metadata(&child, PARENT_SESSION_KEY, &parent)
        .await
        .unwrap();

    let usage = |prompt_tokens| crate::session_store::MessageUsage {
        prompt_tokens,
        completion_tokens: 1,
        ..Default::default()
    };
    for (session, tokens) in [(&parent, 100), (&child, 30), (&child, 30), (&other, 7)] {
        db.insert_message(
            session,
            &Role::Assistant,
            Some("reply"),
            None,
            None,
            Some(&usage(tokens)),
        )
        .await
        .unwrap();
    }

    let total = db.session_token_usage(&parent).await.unwrap();
    assert_eq!(total.prompt_tokens, 160);
    assert_eq!(total.api_calls, 3);

    let by_agent: std::collections::HashMap<_, _> = db
        .session_usage_by_agent(&parent)
        .await
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(by_agent.len(), 2, "{by_agent:?}");
    assert_eq!(by_agent["main"].prompt_tokens, 100);
    assert_eq!(by_agent["explore"].prompt_tokens, 60);
    assert_eq!(by_agent["explore"].api_calls, 2);

    // A child's own totals stay its own.
    let own = db.session_token_usage(&child).await.unwrap();
    assert_eq!(own.prompt_tokens, 60);
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

    db.update_message_thinking_content(id, "m", "I should think carefully about this…")
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

    db.update_message_thinking_content(assistant_id, "m", "2+2=4 trivially")
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

use crate::session_store::session_event_kind;

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
    assert_eq!(after.len(), 4);
    // Tail of original messages preserved (oldest of these is "msg 7").
    assert_eq!(after[0].role, Role::User);
    assert_eq!(after[0].content.as_deref(), Some("msg 7"));
    // Synthetic summary appended.
    assert_eq!(after[3].role, Role::System);
    assert_eq!(after[3].content.as_deref(), Some("summary"));
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

/// Another process may rewind or compact a session between two times this
/// one holds it, so taking its writer lock drops what this process cached
/// of its context.
#[tokio::test]
async fn taking_the_writer_lock_drops_the_cached_context() {
    let (db, tmp) = setup().await;
    let session = db.create_session("default", tmp.path()).await.unwrap();
    for text in ["kept", "rewound away"] {
        db.insert_message(&session, &Role::User, Some(text), None, None, None)
            .await
            .unwrap();
    }
    assert_eq!(db.load_context(&session).await.unwrap().len(), 2);

    // Another process's rewind: this process's cache can't see it.
    let other = Database::open(&tmp.path().join("test.db")).await.unwrap();
    sqlx::query("DELETE FROM messages WHERE session_id = ? AND content = 'rewound away'")
        .bind(&session)
        .execute(other.pool())
        .await
        .unwrap();

    let _writer = db.acquire_writer(&session).await.unwrap();
    let context = db.load_context(&session).await.unwrap();
    assert_eq!(context.len(), 1, "{context:?}");
}

/// A session with event rows (every one that used a sub-agent or logged a
/// notice) deletes, its events with it: they hold a foreign key to it.
#[tokio::test]
async fn delete_session_removes_its_events() {
    let (db, tmp) = setup().await;
    let doomed = db.create_session("default", tmp.path()).await.unwrap();
    let kept = db.create_session("default", tmp.path()).await.unwrap();
    for session in [&doomed, &kept] {
        db.insert_session_event(session, "info", "{}", None)
            .await
            .unwrap();
    }

    assert!(db.delete_session(&doomed).await.unwrap());
    assert!(db.load_session_events(&doomed).await.unwrap().is_empty());
    assert_eq!(db.load_session_events(&kept).await.unwrap().len(), 1);
}

/// The file tracker's table goes when a database from before its removal
/// is opened.
#[tokio::test]
async fn test_open_drops_the_owned_files_table() {
    let (db, tmp) = setup().await;
    sqlx::query(
        "CREATE TABLE owned_files (session_id TEXT NOT NULL, path TEXT NOT NULL, \
         PRIMARY KEY(session_id, path))",
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query("INSERT INTO owned_files VALUES ('s', '/p/a.txt')")
        .execute(db.pool())
        .await
        .unwrap();
    // A file from before versions were kept.
    sqlx::query("PRAGMA user_version = 0")
        .execute(db.pool())
        .await
        .unwrap();
    drop(db);

    let db = Database::open(&tmp.path().join("test.db")).await.unwrap();
    let tables: Vec<(String,)> = sqlx::query_as(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'owned_files'",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert!(tables.is_empty(), "owned_files should be dropped");
}

/// The file as Koda 0.7.12 left it: version 0, sessions without the columns
/// versions 2 and 3 add.
async fn as_koda_0_7_12_left_it(db: &Database) {
    for sql in [
        "DROP INDEX idx_sessions_root_recent",
        "DROP INDEX idx_sessions_recent",
        "ALTER TABLE sessions DROP COLUMN archived_at",
        "ALTER TABLE sessions DROP COLUMN source",
        "ALTER TABLE sessions DROP COLUMN originator",
        "ALTER TABLE sessions DROP COLUMN koda_version",
        "ALTER TABLE sessions DROP COLUMN forked_from_id",
        "ALTER TABLE sessions DROP COLUMN parent_session_id",
        "ALTER TABLE sessions DROP COLUMN ephemeral",
        "ALTER TABLE sessions DROP COLUMN preview",
        "PRAGMA user_version = 0",
    ] {
        sqlx::query(sql).execute(db.pool()).await.unwrap();
    }
}

async fn schema_version(db: &Database) -> i64 {
    let (version,): (i64,) = sqlx::query_as("PRAGMA user_version")
        .fetch_one(db.pool())
        .await
        .unwrap();
    version
}

/// An older Koda's file gains the later versions' columns once: its
/// sessions get their previews and last-used times from their messages, a
/// sub-agent's session its parent from its metadata, and none is archived.
#[tokio::test]
async fn an_older_file_is_brought_to_the_current_version() {
    let (db, tmp) = setup().await;
    as_koda_0_7_12_left_it(&db).await;
    let long = "x".repeat(crate::session_store::PREVIEW_CHARS + 50);
    for sql in [
        "INSERT INTO sessions (id, agent_name, project_root, created_at) \
         VALUES ('parent', 'default', '/p', '2026-01-02 03:04:05')",
        "INSERT INTO sessions (id, agent_name, project_root) VALUES ('child', 'explore', '/p')",
        "INSERT INTO sessions (id, agent_name, project_root, created_at) \
         VALUES ('empty', 'default', '/p', '2026-01-01 00:00:00')",
        "INSERT INTO session_metadata (session_id, key, value) \
         VALUES ('child', 'parent_session', 'parent')",
        "INSERT INTO messages (session_id, role, content, created_at) \
         VALUES ('parent', 'user', 'fix the test', '2026-01-02 03:05:00')",
        "INSERT INTO messages (session_id, role, content, created_at) \
         VALUES ('parent', 'assistant', 'done', '2026-01-02 03:06:00')",
    ] {
        sqlx::query(sql).execute(db.pool()).await.unwrap();
    }
    sqlx::query("INSERT INTO messages (session_id, role, content) VALUES ('child', 'user', ?)")
        .bind(&long)
        .execute(db.pool())
        .await
        .unwrap();
    drop(db);

    let db = Database::open(&tmp.path().join("test.db")).await.unwrap();
    assert_eq!(schema_version(&db).await, 3);
    let parent = db.get_session("parent").await.unwrap().unwrap();
    assert_eq!(parent.preview.as_deref(), Some("fix the test"));
    assert_eq!(parent.source, None, "who started it was never recorded");
    assert_eq!(parent.parent_session_id, None);
    assert_eq!(parent.updated_unix - parent.created_unix, 115);
    let child = db.get_session("child").await.unwrap().unwrap();
    assert_eq!(child.source.as_deref(), Some("subAgent"));
    assert_eq!(child.parent_session_id.as_deref(), Some("parent"));
    assert_eq!(
        child.preview.map(|p| p.chars().count()),
        Some(crate::session_store::PREVIEW_CHARS)
    );
    let empty = db.get_session("empty").await.unwrap().unwrap();
    assert_eq!(empty.preview, None);
    assert_eq!(empty.updated_unix, empty.created_unix);
    assert_eq!(empty.archived_unix, None);

    // Opening it again changes nothing.
    drop(db);
    let db = Database::open(&tmp.path().join("test.db")).await.unwrap();
    assert_eq!(schema_version(&db).await, 3);
}

/// Two Kodas opening an older file at once take turns: both open, and the
/// columns are added once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_processes_can_open_an_older_file_at_once() {
    let (db, tmp) = setup().await;
    as_koda_0_7_12_left_it(&db).await;
    drop(db);
    let path = tmp.path().join("test.db");
    let (a, b) = tokio::join!(Database::open(&path), Database::open(&path));
    let (a, _b) = (a.unwrap(), b.unwrap());
    assert_eq!(schema_version(&a).await, 3);
}

/// A newer Koda's file opens as it is: this version adds nothing to it and
/// leaves its version alone, as Codex's migrator ignores migrations it
/// doesn't know.
#[tokio::test]
async fn a_newer_file_opens_unchanged() {
    let (db, tmp) = setup().await;
    sqlx::query("PRAGMA user_version = 99")
        .execute(db.pool())
        .await
        .unwrap();
    drop(db);
    let db = Database::open(&tmp.path().join("test.db")).await.unwrap();
    assert_eq!(schema_version(&db).await, 99);
    db.create_session("default", tmp.path()).await.unwrap();
}

/// A file from before sessions could be archived (version 2) gains the
/// column: its sessions are listed as before, none archived, and can be.
#[tokio::test]
async fn a_version_2_file_gains_the_archive_column() {
    let (db, tmp) = setup().await;
    let session = db.create_session("default", tmp.path()).await.unwrap();
    for sql in [
        "ALTER TABLE sessions DROP COLUMN archived_at",
        "PRAGMA user_version = 2",
    ] {
        sqlx::query(sql).execute(db.pool()).await.unwrap();
    }
    drop(db);

    let db = Database::open(&tmp.path().join("test.db")).await.unwrap();
    assert_eq!(schema_version(&db).await, 3);
    let listed = db.list_sessions(10, tmp.path()).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].archived_unix, None);
    assert_eq!(db.archive_session(&session).await.unwrap(), [session]);
}

/// An archived session is left out of every list unless archived ones are
/// asked for, as Codex's `thread/list` takes `archived`, and stays readable
/// by its id; unarchiving puts it back. Archiving twice, or unarchiving
/// what isn't archived, changes nothing and says so with an empty list.
#[tokio::test]
async fn an_archived_session_is_listed_only_when_asked_for() {
    use crate::session_store::SessionListQuery;
    let (db, tmp) = setup().await;
    let kept = db.create_session("default", tmp.path()).await.unwrap();
    let put_away = db.create_session("default", tmp.path()).await.unwrap();
    let ids = |sessions: Vec<crate::session_store::SessionInfo>| {
        sessions.into_iter().map(|s| s.id).collect::<Vec<_>>()
    };
    let page = |archived: bool| SessionListQuery {
        archived,
        limit: 50,
        ..Default::default()
    };

    assert_eq!(
        db.archive_session(&put_away).await.unwrap(),
        std::slice::from_ref(&put_away)
    );
    assert_eq!(
        ids(db.list_sessions_page(&page(false)).await.unwrap()),
        std::slice::from_ref(&kept)
    );
    assert_eq!(
        ids(db.list_sessions_page(&page(true)).await.unwrap()),
        std::slice::from_ref(&put_away)
    );
    assert_eq!(
        ids(db.list_sessions(10, tmp.path()).await.unwrap()),
        std::slice::from_ref(&kept),
        "the folder's own list leaves it out too"
    );
    let stored = db.get_session(&put_away).await.unwrap().unwrap();
    assert!(stored.archived_unix.is_some(), "still read by its id");
    assert!(db.archive_session(&put_away).await.unwrap().is_empty());

    assert_eq!(
        db.unarchive_session(&put_away).await.unwrap(),
        std::slice::from_ref(&put_away)
    );
    assert_eq!(
        ids(db.list_sessions_page(&page(false)).await.unwrap()),
        [put_away.clone(), kept.clone()]
    );
    assert!(db.list_sessions_page(&page(true)).await.unwrap().is_empty());
    let stored = db.get_session(&put_away).await.unwrap().unwrap();
    assert_eq!(stored.archived_unix, None);
    assert!(db.unarchive_session(&put_away).await.unwrap().is_empty());
    assert!(db.unarchive_session(&kept).await.unwrap().is_empty());
    assert!(
        db.archive_session("no-such-session")
            .await
            .unwrap()
            .is_empty()
    );
}

/// Archiving a session takes the sessions of the sub-agents it ran with
/// it, and theirs, the deepest first, as Codex archives a thread's spawned
/// descendants; a fork of it is another conversation and stays. One
/// already archived isn't archived again, and unarchiving brings the whole
/// subtree back.
#[tokio::test]
async fn archiving_takes_the_sub_agents_sessions_along() {
    use crate::session_store::{SessionOrigin, source};
    let (db, tmp) = setup().await;
    let parent = db.create_session("default", tmp.path()).await.unwrap();
    let under = |parent: &str| SessionOrigin {
        source: Some(source::SUB_AGENT.into()),
        parent_session_id: Some(parent.to_string()),
        ..Default::default()
    };
    let child = db
        .create_session_from("explore", tmp.path(), &under(&parent))
        .await
        .unwrap();
    let grandchild = db
        .create_session_from("explore", tmp.path(), &under(&child))
        .await
        .unwrap();
    let other_child = db
        .create_session_from("explore", tmp.path(), &under(&parent))
        .await
        .unwrap();
    let fork = db
        .fork_session(&parent, "a fork", tmp.path(), &SessionOrigin::default())
        .await
        .unwrap();
    let archived = async |id: &str| {
        let stored = db.get_session(id).await.unwrap().unwrap();
        stored.archived_unix.is_some()
    };

    // One sub-agent's session was archived on its own before.
    assert_eq!(
        db.archive_session(&other_child).await.unwrap(),
        std::slice::from_ref(&other_child)
    );
    assert_eq!(
        db.archive_session(&parent).await.unwrap(),
        [parent.clone(), grandchild.clone(), child.clone()]
    );
    for id in [&parent, &child, &grandchild, &other_child] {
        assert!(archived(id).await, "{id}");
    }
    assert!(!archived(&fork).await, "a fork is not a descendant");

    // A fork of an archived session isn't archived either.
    let later_fork = db
        .fork_session(&parent, "later", tmp.path(), &SessionOrigin::default())
        .await
        .unwrap();
    assert!(!archived(&later_fork).await);

    assert_eq!(
        db.unarchive_session(&parent).await.unwrap(),
        [
            parent.clone(),
            grandchild.clone(),
            child.clone(),
            other_child.clone()
        ]
    );
    for id in [&parent, &child, &grandchild, &other_child] {
        assert!(!archived(id).await, "{id}");
    }
}

/// A session records where it came from, its first prompt as its preview,
/// and when it was last written.
#[tokio::test]
async fn a_session_records_its_origin_and_its_preview() {
    use crate::session_store::{SessionOrigin, source};
    let (db, tmp) = setup().await;
    let parent = db
        .create_session_from(
            "default",
            tmp.path(),
            &SessionOrigin {
                source: Some(source::CLI.into()),
                originator: Some("koda-tui".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    db.insert_message(&parent, &Role::User, Some("first prompt"), None, None, None)
        .await
        .unwrap();
    db.insert_message(
        &parent,
        &Role::User,
        Some("second prompt"),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let info = db.get_session(&parent).await.unwrap().unwrap();
    assert_eq!(info.source.as_deref(), Some("cli"));
    assert_eq!(info.originator.as_deref(), Some("koda-tui"));
    assert_eq!(info.preview.as_deref(), Some("first prompt"));
    assert!(!info.ephemeral);

    let fork = db
        .fork_session(
            &parent,
            "a fork",
            tmp.path(),
            &SessionOrigin {
                source: Some(source::APP_SERVER.into()),
                originator: Some("koda-desktop".into()),
                ephemeral: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let info = db.get_session(&fork).await.unwrap().unwrap();
    assert_eq!(info.forked_from_id.as_deref(), Some(parent.as_str()));
    assert_eq!(info.source.as_deref(), Some("appServer"));
    assert_eq!(info.preview.as_deref(), Some("first prompt"));
    assert!(info.ephemeral);
}

/// A page of sessions is filtered and ordered in the query, and the next
/// page goes on from the last session, so one made meanwhile doesn't shift
/// it. Ephemeral sessions are never listed.
#[tokio::test]
async fn sessions_are_listed_by_folder_source_and_page() {
    use crate::session_store::{SessionListQuery, SessionOrigin, source};
    let (db, _tmp) = setup().await;
    let from = |source: &str, ephemeral: bool| SessionOrigin {
        source: Some(source.to_string()),
        ephemeral,
        ..Default::default()
    };
    // Created a second apart, so their order is their creation.
    let mut made = Vec::new();
    for (i, (root, origin)) in [
        ("/a", from(source::CLI, false)),
        ("/a", from(source::EXEC, false)),
        ("/b", from(source::APP_SERVER, false)),
        ("/a", from(source::SUB_AGENT, false)),
        ("/a", from(source::CLI, true)),
        ("/a", SessionOrigin::default()),
    ]
    .into_iter()
    .enumerate()
    {
        let id = db
            .create_session_from("default", std::path::Path::new(root), &origin)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE sessions SET created_at = datetime('2026-01-01', ? || ' seconds') WHERE id = ?",
        )
        .bind(i as i64)
        .bind(&id)
        .execute(db.pool())
        .await
        .unwrap();
        made.push(id);
    }
    let ids = |sessions: Vec<crate::session_store::SessionInfo>| {
        sessions.into_iter().map(|s| s.id).collect::<Vec<_>>()
    };
    let everything = SessionListQuery {
        limit: 50,
        ..Default::default()
    };

    // Every folder, every source, newest first; not the ephemeral one.
    let all = ids(db.list_sessions_page(&everything).await.unwrap());
    assert_eq!(
        all,
        [&made[5], &made[3], &made[2], &made[1], &made[0]].map(String::clone)
    );

    // One folder, the interactive sources and those never recorded.
    let interactive = SessionListQuery {
        project_roots: Some(vec!["/a".into()]),
        sources: Some(vec![source::CLI.into(), source::APP_SERVER.into()]),
        unknown_source: true,
        ..everything.clone()
    };
    let listed = ids(db.list_sessions_page(&interactive).await.unwrap());
    assert_eq!(listed, [&made[5], &made[0]].map(String::clone));

    // Two per page; a session made between pages doesn't repeat or skip one.
    let page = |after| SessionListQuery {
        limit: 2,
        after,
        ..everything.clone()
    };
    let first = db.list_sessions_page(&page(None)).await.unwrap();
    assert_eq!(first.len(), 2);
    db.create_session("default", std::path::Path::new("/a"))
        .await
        .unwrap();
    let last = first.last().unwrap();
    let second = ids(db
        .list_sessions_page(&page(Some((last.created_unix, last.sequence))))
        .await
        .unwrap());
    assert_eq!(second, [&made[2], &made[1]].map(String::clone));

    // Oldest first.
    let oldest = SessionListQuery {
        ascending: true,
        limit: 1,
        ..everything
    };
    assert_eq!(
        ids(db.list_sessions_page(&oldest).await.unwrap()),
        [made[0].clone()]
    );
}

/// Deleting a session deletes the sessions of the sub-agents it ran, and
/// theirs; another session's are left.
#[tokio::test]
async fn deleting_a_session_deletes_its_sub_agents_sessions() {
    use crate::session_store::{SessionOrigin, source};
    let (db, tmp) = setup().await;
    let child_of = |parent: &str| SessionOrigin {
        source: Some(source::SUB_AGENT.into()),
        parent_session_id: Some(parent.to_string()),
        ..Default::default()
    };
    let parent = db.create_session("default", tmp.path()).await.unwrap();
    let child = db
        .create_session_from("explore", tmp.path(), &child_of(&parent))
        .await
        .unwrap();
    let grandchild = db
        .create_session_from("explore", tmp.path(), &child_of(&child))
        .await
        .unwrap();
    let other = db.create_session("default", tmp.path()).await.unwrap();
    let others_child = db
        .create_session_from("explore", tmp.path(), &child_of(&other))
        .await
        .unwrap();
    db.insert_session_event(&grandchild, "info", "{}", None)
        .await
        .unwrap();

    assert!(db.delete_session(&parent).await.unwrap());
    for gone in [&parent, &child, &grandchild] {
        assert!(db.get_session(gone).await.unwrap().is_none(), "{gone}");
    }
    for kept in [&other, &others_child] {
        assert!(db.get_session(kept).await.unwrap().is_some(), "{kept}");
    }
    assert!(!db.delete_session(&parent).await.unwrap(), "already gone");
}

/// A prompt's images are stored on the latest user message and come back
/// with the context, even when that context was cached before they were
/// stored (a user row is complete when inserted).
#[tokio::test]
async fn images_go_on_the_latest_user_message_and_reach_a_cached_context() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();
    db.insert_message(&session, &Role::User, Some("first"), None, None, None)
        .await
        .unwrap();
    insert_complete_assistant(&db, &session, Some("ok"), None).await;
    db.insert_message(&session, &Role::User, Some("look"), None, None, None)
        .await
        .unwrap();
    // Cached now, before the images are stored.
    assert!(
        db.load_context(&session)
            .await
            .unwrap()
            .iter()
            .all(|m| m.images.is_none())
    );

    let images = r#"[{"media_type":"image/png","base64":"AAAA"}]"#;
    db.set_last_user_message_images(&session, images)
        .await
        .unwrap();
    let context = db.load_context(&session).await.unwrap();
    let with: Vec<_> = context
        .iter()
        .filter_map(|m| m.images.as_deref().map(|i| (m.content.clone(), i)))
        .collect();
    assert_eq!(with, [(Some("look".to_string()), images)]);
}

/// A tool result's images are stored on its message and reach a context
/// cached before they were stored.
#[tokio::test]
async fn a_tool_results_images_are_stored_on_its_message() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();
    db.insert_message(&session, &Role::User, Some("look"), None, None, None)
        .await
        .unwrap();
    let call = r#"[{"id":"c1","function_name":"Read","arguments":"{}"}]"#;
    insert_complete_assistant(&db, &session, None, Some(call)).await;
    let result = db
        .insert_message(
            &session,
            &Role::Tool,
            Some("a.png: an image"),
            None,
            Some("c1"),
            None,
        )
        .await
        .unwrap();
    assert!(
        db.load_context(&session)
            .await
            .unwrap()
            .iter()
            .all(|m| m.images.is_none())
    );

    let images = r#"[{"media_type":"image/png","base64":"AAAA"}]"#;
    db.set_message_images(&session, result, images)
        .await
        .unwrap();
    let context = db.load_context(&session).await.unwrap();
    let tool = context.iter().find(|m| m.id == result).unwrap();
    assert_eq!(tool.images.as_deref(), Some(images));
}

// ── session/fork: fork_session ──────────────────────────────────────────

/// A fork is a faithful copy, unlike a sub-agent's: reasoning blocks,
/// images, turn records and metadata come with the messages, the session's
/// model and mode too; a parent link doesn't, and the source is untouched.
#[tokio::test]
async fn fork_session_copies_everything_a_session_holds() {
    use crate::session_store::{PARENT_SESSION_KEY, TurnRecord};
    let (db, tmp) = setup().await;
    let src = db.create_session("default", tmp.path()).await.unwrap();
    db.set_session_model(&src, "anthropic", "https://api", "claude-opus")
        .await
        .unwrap();
    db.set_session_mode(&src, "plan").await.unwrap();
    db.insert_message(&src, &Role::User, Some("look"), None, None, None)
        .await
        .unwrap();
    let reply = insert_complete_assistant(&db, &src, Some("seen"), None).await;
    db.update_message_thinking_blocks(reply, "[{\"type\":\"thinking\"}]")
        .await
        .unwrap();
    db.set_message_images(&src, reply, "[\"png\"]")
        .await
        .unwrap();
    db.append_turn_records(
        &src,
        &[TurnRecord {
            turn_id: "t1".into(),
            kind: "turn".into(),
            payload: "{}".into(),
        }],
    )
    .await
    .unwrap();
    db.set_metadata(&src, "todo_seen", "yes").await.unwrap();
    db.set_metadata(&src, PARENT_SESSION_KEY, "someone")
        .await
        .unwrap();

    let fork = db
        .fork_session(
            &src,
            "Fork of look",
            std::path::Path::new("/elsewhere/checkout"),
            &Default::default(),
        )
        .await
        .unwrap();
    assert_ne!(fork, src);
    let info = db.get_session(&fork).await.unwrap().unwrap();
    assert_eq!(info.title.as_deref(), Some("Fork of look"));
    // The copy belongs to where it opens; the source keeps its project.
    assert_eq!(info.project_root.as_deref(), Some("/elsewhere/checkout"));
    assert_ne!(
        db.get_session(&src)
            .await
            .unwrap()
            .unwrap()
            .project_root
            .as_deref(),
        Some("/elsewhere/checkout")
    );
    assert_eq!(info.mode.as_deref(), Some("plan"));
    assert_eq!(info.model.as_deref(), Some("claude-opus"));
    let copied = db.load_all_messages(&fork).await.unwrap();
    let contents: Vec<_> = copied.iter().map(|m| m.content.clone()).collect();
    assert_eq!(contents, [Some("look".into()), Some("seen".into())]);
    assert_eq!(
        copied[1].thinking_blocks.as_deref(),
        Some("[{\"type\":\"thinking\"}]")
    );
    assert_eq!(copied[1].images.as_deref(), Some("[\"png\"]"));
    assert_eq!(db.load_turn_records(&fork).await.unwrap().len(), 1);
    assert_eq!(
        db.get_metadata(&fork, "todo_seen")
            .await
            .unwrap()
            .as_deref(),
        Some("yes")
    );
    assert_eq!(
        db.get_metadata(&fork, PARENT_SESSION_KEY).await.unwrap(),
        None
    );
    assert_eq!(db.load_all_messages(&src).await.unwrap().len(), 2);

    assert!(
        db.fork_session(
            "missing",
            "x",
            std::path::Path::new("/p"),
            &Default::default()
        )
        .await
        .is_err()
    );
}

/// A rewind takes a turn and everything after it out of the session, and
/// keeps what came before.
#[tokio::test]
async fn a_rewind_drops_a_turn_and_what_followed() {
    use crate::session_store::{MessageBoundary, REWIND_KIND, TurnRecord};
    let (db, tmp) = setup().await;
    let session = db.create_session("default", tmp.path()).await.unwrap();
    let record = |turn: &str, kind: &str| TurnRecord {
        turn_id: turn.into(),
        kind: kind.into(),
        payload: "{}".into(),
    };

    let before_one = db.message_boundary(&session).await.unwrap();
    assert_eq!(before_one, MessageBoundary::default());
    db.append_turn_records(&session, &[record("t1", REWIND_KIND), record("t1", "turn")])
        .await
        .unwrap();
    db.insert_message(&session, &Role::User, Some("first"), None, None, None)
        .await
        .unwrap();
    insert_complete_assistant(&db, &session, Some("one"), None).await;

    let before_two = db.message_boundary(&session).await.unwrap();
    assert_eq!(before_two.last_message_id, 2);
    db.append_turn_records(&session, &[record("t2", REWIND_KIND), record("t2", "turn")])
        .await
        .unwrap();
    db.insert_message(&session, &Role::User, Some("second"), None, None, None)
        .await
        .unwrap();
    insert_complete_assistant(&db, &session, Some("two"), None).await;
    db.append_turn_records(&session, &[record("t3", "turn")])
        .await
        .unwrap();

    db.rewind(&session, "t2", before_two).await.unwrap();
    let context = db.load_context(&session).await.unwrap();
    let texts: Vec<_> = context.iter().filter_map(|m| m.content.clone()).collect();
    assert_eq!(texts, ["first", "one"]);
    let turns: Vec<String> = db
        .load_turn_records(&session)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.turn_id)
        .collect();
    assert_eq!(turns, ["t1", "t1"]);
    assert!(db.rewind(&session, "t9", before_two).await.is_err());
}

/// A compaction after a turn's start refuses a rewind to before it.
#[tokio::test]
async fn a_rewind_across_a_compaction_is_refused() {
    use crate::session_store::{REWIND_KIND, TurnRecord};
    let (db, tmp) = setup().await;
    let session = db.create_session("default", tmp.path()).await.unwrap();
    db.insert_message(&session, &Role::User, Some("first"), None, None, None)
        .await
        .unwrap();
    insert_complete_assistant(&db, &session, Some("one"), None).await;
    let boundary = db.message_boundary(&session).await.unwrap();
    db.append_turn_records(
        &session,
        &[TurnRecord {
            turn_id: "t2".into(),
            kind: REWIND_KIND.into(),
            payload: "{}".into(),
        }],
    )
    .await
    .unwrap();
    db.insert_message(&session, &Role::User, Some("second"), None, None, None)
        .await
        .unwrap();
    db.compact_session(&session, "summary", 1).await.unwrap();
    let error = db.rewind(&session, "t2", boundary).await.unwrap_err();
    assert!(error.to_string().contains("compacted"), "{error}");
}

// ── Searching sessions ──────────────────────────────────────────────────

/// A record of a completed item that says `text`, as the host stores one.
fn said(turn: &str, text: &str) -> crate::session_store::TurnRecord {
    crate::session_store::TurnRecord {
        turn_id: turn.into(),
        kind: crate::session_store::ITEM_KIND.into(),
        payload: serde_json::json!({"type": "agentMessage", "id": "m1", "text": text}).to_string(),
    }
}

/// The ids of the search candidates for `term`, among every session.
async fn candidates(db: &Database, term: &str) -> Vec<String> {
    let everything = crate::session_store::SessionListQuery {
        ascending: true,
        limit: 50,
        ..Default::default()
    };
    db.search_candidates_page(&everything, term)
        .await
        .unwrap()
        .into_iter()
        .map(|session| session.id)
        .collect()
}

/// A search's candidates are the sessions whose stored text has the term,
/// in either case: an item's record for a session with turn records, its
/// messages for one without. The system prompt and a turn's context are no
/// part of the conversation.
#[tokio::test]
async fn search_candidates_come_from_records_or_from_messages() {
    use crate::session_store::{TURN_CONTEXT_KIND, TurnRecord};
    let (db, tmp) = setup().await;
    let mut made = Vec::new();
    for _ in 0..6 {
        made.push(db.create_session("default", tmp.path()).await.unwrap());
    }
    let [
        recorded,
        older,
        read_from_records,
        sub_agent,
        prompt_only,
        unrelated,
    ] = &made[..]
    else {
        unreachable!()
    };
    db.append_turn_records(recorded, &[said("t1", "The Flux capacitor is charged.")])
        .await
        .unwrap();
    db.insert_message(
        older,
        &Role::User,
        Some("where is the FLUX pump"),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    // Its records are what a reader gets, and they don't say it.
    db.insert_message(
        read_from_records,
        &Role::User,
        Some("flux"),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    db.append_turn_records(read_from_records, &[said("t1", "Nothing of the kind.")])
        .await
        .unwrap();
    // A sub-agent's session has a context record and no turns.
    db.append_turn_records(
        sub_agent,
        &[TurnRecord {
            turn_id: "t1".into(),
            kind: TURN_CONTEXT_KIND.into(),
            payload: r#"{"model": "flux-1"}"#.into(),
        }],
    )
    .await
    .unwrap();
    insert_complete_assistant(&db, sub_agent, Some("Found the flux pump."), None).await;
    db.insert_message(
        prompt_only,
        &Role::System,
        Some("You are Flux."),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    db.insert_message(unrelated, &Role::User, Some("hello"), None, None, None)
        .await
        .unwrap();

    assert_eq!(
        candidates(&db, "flux").await,
        [recorded, older, sub_agent].map(String::clone)
    );
    assert_eq!(
        candidates(&db, "FLUX CAPACITOR").await,
        std::slice::from_ref(recorded)
    );
    assert!(candidates(&db, "warp").await.is_empty());
}

/// A term is looked for as written: `%` and `_` are themselves, and a
/// quote or a backslash is found where JSON escaped it in a record and
/// where a message has it plain.
#[tokio::test]
async fn a_search_term_is_literal_in_records_and_in_messages() {
    let (db, tmp) = setup().await;
    let has = r#"at 100% of a_b, "quoted", in C:\dir"#;
    let lacks = "at 1000 of axb, quoted, in C:dir";
    let mut made = Vec::new();
    for text in [has, lacks] {
        let recorded = db.create_session("default", tmp.path()).await.unwrap();
        db.append_turn_records(&recorded, &[said("t1", text)])
            .await
            .unwrap();
        let older = db.create_session("default", tmp.path()).await.unwrap();
        db.insert_message(&older, &Role::User, Some(text), None, None, None)
            .await
            .unwrap();
        made.push([recorded, older]);
    }
    for term in ["100%", "a_b", r#""quoted""#, r"C:\dir", r#"B, "Q"#] {
        assert_eq!(candidates(&db, term).await, made[0], "{term}");
    }
}

/// `LIKE` ignores case for ASCII letters only, and no stored text holds
/// the line break a reader puts between two: for a term with a letter
/// outside ASCII that has another case, or a control character, nothing is
/// narrowed, so no session is missed. Text without case narrows as ASCII
/// does.
#[tokio::test]
async fn a_term_like_cant_be_trusted_with_narrows_nothing() {
    let (db, tmp) = setup().await;
    let upper = db.create_session("default", tmp.path()).await.unwrap();
    db.append_turn_records(&upper, &[said("t1", "L'ÉCOLE est fermée. 中文")])
        .await
        .unwrap();
    let other = db.create_session("default", tmp.path()).await.unwrap();
    db.append_turn_records(&other, &[said("t1", "nothing")])
        .await
        .unwrap();
    let both = [upper.clone(), other];
    for term in ["école", "É", "a\nb", "a\tb", "ǅ", "\u{212a}"] {
        assert_eq!(candidates(&db, term).await, both, "{term:?}");
    }
    for term in ["中文", "l'", "EST FERM"] {
        assert_eq!(
            candidates(&db, term).await,
            std::slice::from_ref(&upper),
            "{term:?}"
        );
    }
}

/// The rule behind it: `LIKE` is trusted with a character outside ASCII
/// only if it folds to itself and no other character folds to it; and
/// nothing outside ASCII folds to an ASCII character, or `LIKE` would miss
/// it.
#[test]
fn like_is_trusted_only_where_it_agrees_with_the_fold() {
    use super::queries::stored_text_patterns;
    use crate::session_store::search_fold;
    for c in (0..=u32::from(char::MAX)).filter_map(char::from_u32) {
        let folded = search_fold(c);
        assert_eq!(search_fold(folded), folded, "{c:?} folds twice");
        assert_eq!(folded.is_ascii(), c.is_ascii(), "{c:?} folds across ASCII");
        if folded != c && !c.is_ascii() {
            for other_case in [c, folded] {
                assert_eq!(
                    stored_text_patterns(&other_case.to_string()),
                    None,
                    "{c:?} folds to {folded:?}"
                );
            }
        }
    }
}

/// A session from before turn records keeps tool calls as the provider
/// sent them. A term every JSON writer leaves as it is is looked for
/// there, `\/` read as `/`; for any other, every such session with a tool
/// call is a candidate, since the arguments may spell it another way.
#[tokio::test]
async fn an_older_sessions_tool_calls_are_searched_as_the_provider_wrote_them() {
    let (db, tmp) = setup().await;
    let called = |arguments: &str| {
        serde_json::json!([{"id": "c1", "function_name": "Bash", "arguments": arguments}])
            .to_string()
    };
    let mut made = Vec::new();
    for arguments in [
        // `cat src/main.rs`, `a<b` and 1000, each written another way.
        r#"{"command": "cat src\/main.rs && test a\u003cb", "timeout": 1e3}"#,
        r#"{"command": "ls"}"#,
    ] {
        let session = db.create_session("default", tmp.path()).await.unwrap();
        insert_complete_assistant(&db, &session, None, Some(&called(arguments))).await;
        made.push(session);
    }
    let no_calls = db.create_session("default", tmp.path()).await.unwrap();
    db.insert_message(&no_calls, &Role::User, Some("hello"), None, None, None)
        .await
        .unwrap();

    for term in ["src/main.rs", "BASH", "command"] {
        let expected: &[String] = if term == "src/main.rs" {
            &made[..1]
        } else {
            &made
        };
        assert_eq!(candidates(&db, term).await, expected, "{term}");
    }
    for term in ["a<b", "1000", "it's"] {
        assert_eq!(candidates(&db, term).await, made, "{term}");
    }
}

/// `session/list`'s `searchTerm`: sessions whose title or preview has the
/// text, ASCII letters in either case, `%` and `_` as themselves.
#[tokio::test]
async fn sessions_are_listed_by_a_term_in_their_title_or_preview() {
    use crate::session_store::SessionListQuery;
    let (db, tmp) = setup().await;
    let titled = db.create_session("default", tmp.path()).await.unwrap();
    db.set_session_title(&titled, "Fix the Parser")
        .await
        .unwrap();
    let previewed = db.create_session("default", tmp.path()).await.unwrap();
    db.insert_message(
        &previewed,
        &Role::User,
        Some("why is the parser 100% slower"),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let answered = db.create_session("default", tmp.path()).await.unwrap();
    db.insert_message(&answered, &Role::User, Some("hello"), None, None, None)
        .await
        .unwrap();
    insert_complete_assistant(&db, &answered, Some("The parser is fine."), None).await;

    let listed = |term: &'static str| {
        let db = db.clone();
        async move {
            let query = SessionListQuery {
                search_term: Some(term.to_string()),
                ascending: true,
                limit: 50,
                ..Default::default()
            };
            let page = db.list_sessions_page(&query).await.unwrap();
            page.into_iter().map(|s| s.id).collect::<Vec<_>>()
        }
    };
    assert_eq!(listed("PARSER").await, [titled.clone(), previewed.clone()]);
    assert_eq!(listed("fix the").await, [titled]);
    assert_eq!(listed("100%").await, [previewed]);
    assert!(listed("1_0").await.is_empty());
    assert!(listed("fine").await.is_empty(), "a reply is not searched");
}
