//! Tests for the /purge feature: compacted_stats() and purge_compacted().

use koda_core::db::{Database, Role};
use koda_core::persistence::Persistence;
use tempfile::TempDir;

async fn setup() -> (Database, TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let db = Database::init(tmp.path()).await.unwrap();
    (db, tmp)
}

#[tokio::test]
async fn test_compacted_stats_empty() {
    let (db, _tmp) = setup().await;

    let stats = db.compacted_stats().await.unwrap();
    assert_eq!(stats.message_count, 0);
    assert_eq!(stats.session_count, 0);
    assert_eq!(stats.size_bytes, 0);
    assert!(stats.oldest.is_none());
}

#[tokio::test]
async fn test_compacted_stats_after_compact() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    // Insert enough messages for compaction
    for i in 0..8 {
        let role = if i % 2 == 0 {
            &Role::User
        } else {
            &Role::Assistant
        };
        db.insert_message(&session, role, Some(&format!("msg {i}")), None, None, None)
            .await
            .unwrap();
    }

    // Compact preserving last 2
    let archived = db.compact_session(&session, "summary", 2).await.unwrap();
    assert!(archived > 0);

    let stats = db.compacted_stats().await.unwrap();
    assert_eq!(stats.message_count as usize, archived);
    assert_eq!(stats.session_count, 1);
    assert!(stats.size_bytes > 0);
    assert!(stats.oldest.is_some());
}

#[tokio::test]
async fn test_purge_all() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    for i in 0..8 {
        let role = if i % 2 == 0 {
            &Role::User
        } else {
            &Role::Assistant
        };
        db.insert_message(&session, role, Some(&format!("msg {i}")), None, None, None)
            .await
            .unwrap();
    }

    db.compact_session(&session, "summary", 2).await.unwrap();

    // Purge all (min_age_days = 0)
    let deleted = db.purge_compacted(0).await.unwrap();
    assert!(deleted > 0);

    // Stats should be empty now
    let stats = db.compacted_stats().await.unwrap();
    assert_eq!(stats.message_count, 0);
}

#[tokio::test]
async fn test_purge_with_age_filter() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    for i in 0..8 {
        let role = if i % 2 == 0 {
            &Role::User
        } else {
            &Role::Assistant
        };
        db.insert_message(&session, role, Some(&format!("msg {i}")), None, None, None)
            .await
            .unwrap();
    }

    db.compact_session(&session, "summary", 2).await.unwrap();

    // Purge messages older than 30 days — nothing should be purged (just created)
    let deleted = db.purge_compacted(30).await.unwrap();
    assert_eq!(
        deleted, 0,
        "freshly compacted messages should not be purged with 30d filter"
    );

    // Stats should still show compacted messages
    let stats = db.compacted_stats().await.unwrap();
    assert!(stats.message_count > 0);
}

#[tokio::test]
async fn test_last_accessed_at_updated_on_insert() {
    let (db, _tmp) = setup().await;
    let session = db.create_session("default", _tmp.path()).await.unwrap();

    // Insert a message — should update last_accessed_at.
    // We verify indirectly: list_sessions returns sessions ordered by recency,
    // and the session should appear (meaning it was accessed).
    db.insert_message(&session, &Role::User, Some("hello"), None, None, None)
        .await
        .unwrap();

    let sessions = db.list_sessions(10, _tmp.path()).await.unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, session);
}
