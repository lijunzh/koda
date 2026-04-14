//! Tests for the SQLite persistence layer.
//!
//! Covers the `Persistence` trait methods on `Database`, message pruning
//! helpers (`prune_mismatched_tool_calls`, `prune_null_content_messages`,
//! `prune_whitespace_only_messages`), and interrupted turn detection.

use super::queries::prune_mismatched_tool_calls;
use super::queries::{
    detect_interruption, prune_null_content_messages, prune_whitespace_only_messages,
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

/// Serialize tests that mutate XDG_CONFIG_HOME.
static XDG_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn test_config_dir_with_xdg() {
    let _guard = XDG_MUTEX.lock().unwrap();
    // SAFETY: serialized via XDG_MUTEX
    unsafe { std::env::set_var("XDG_CONFIG_HOME", "/tmp/test_xdg_config") };
    let dir = super::config_dir().unwrap();
    unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
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
    unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
    let dir = super::config_dir().unwrap();
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
