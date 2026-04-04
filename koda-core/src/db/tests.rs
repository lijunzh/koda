//! Tests for the SQLite persistence layer.

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
    db.insert_message(
        &session,
        &Role::Assistant,
        Some("hi there!"),
        None,
        None,
        None,
    )
    .await
    .unwrap();

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

    // Insert several messages
    for i in 0..10 {
        let role = if i % 2 == 0 {
            &Role::User
        } else {
            &Role::Assistant
        };
        db.insert_message(&session, role, Some(&format!("msg {i}")), None, None, None)
            .await
            .unwrap();
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
    db.insert_message(
        &session,
        &Role::Assistant,
        Some("Let me read that."),
        Some(r#"[{"id":"tc1","name":"Read","arguments":"{}"}]"#),
        None,
        None,
    )
    .await
    .unwrap();
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

    // Interrupted turn: assistant with tool_calls but NO tool result
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
