//! E2E tests: skills, compaction, and file ownership.

use koda_core::{compact, db::Role, engine::EngineEvent, persistence::Persistence};
use koda_test_utils::{Env, LlmProvider, MockProvider, MockResponse};
use std::sync::Arc;
use tokio::sync::RwLock;

// ── Skills ────────────────────────────────────────────────────

#[tokio::test]
async fn test_list_skills_returns_builtins() {
    let env = Env::new().await;
    env.insert_user_message("list skills").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call("ListSkills", serde_json::json!({})),
        MockResponse::Text("Here are the available skills.".into()),
    ]);
    let events = env.run_inference(&provider).await;

    let tool_result = events.iter().find_map(|e| {
        if let EngineEvent::ToolCallResult { output, name, .. } = e
            && name == "ListSkills"
        {
            return Some(output.clone());
        }
        None
    });
    assert!(tool_result.is_some(), "expected ListSkills result");
    let output = tool_result.unwrap();
    assert!(output.contains("code-review"));
    assert!(output.contains("security-audit"));
}

#[tokio::test]
async fn test_list_skills_with_search_query() {
    let env = Env::new().await;
    env.insert_user_message("find security skills").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call("ListSkills", serde_json::json!({"query": "security"})),
        MockResponse::Text("Found security skills.".into()),
    ]);
    let events = env.run_inference(&provider).await;

    let output = events
        .iter()
        .find_map(|e| {
            if let EngineEvent::ToolCallResult { output, name, .. } = e
                && name == "ListSkills"
            {
                return Some(output.clone());
            }
            None
        })
        .unwrap();
    assert!(output.contains("security-audit"));
    assert!(!output.contains("code-review"));
}

#[tokio::test]
async fn test_activate_skill_injects_content() {
    let env = Env::new().await;
    env.insert_user_message("review my code").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "ActivateSkill",
            serde_json::json!({"skill_name": "code-review"}),
        ),
        MockResponse::Text("Starting code review per the skill instructions.".into()),
    ]);
    let events = env.run_inference(&provider).await;

    let output = events
        .iter()
        .find_map(|e| {
            if let EngineEvent::ToolCallResult { output, name, .. } = e
                && name == "ActivateSkill"
            {
                return Some(output.clone());
            }
            None
        })
        .unwrap();
    assert!(output.contains("Skill 'code-review' activated"));
    assert!(output.contains("# Code Review"));
    assert!(output.contains("Principles"));
}

#[tokio::test]
async fn test_activate_skill_not_found() {
    let env = Env::new().await;
    env.insert_user_message("use nonexistent skill").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "ActivateSkill",
            serde_json::json!({"skill_name": "nonexistent"}),
        ),
        MockResponse::Text("That skill doesn't exist.".into()),
    ]);
    let events = env.run_inference(&provider).await;

    let output = events
        .iter()
        .find_map(|e| {
            if let EngineEvent::ToolCallResult { output, name, .. } = e
                && name == "ActivateSkill"
            {
                return Some(output.clone());
            }
            None
        })
        .unwrap();
    assert!(output.contains("not found"));
    assert!(output.contains("code-review"));
}

#[tokio::test]
async fn test_activate_skill_missing_parameter() {
    let env = Env::new().await;
    env.insert_user_message("activate skill").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call("ActivateSkill", serde_json::json!({})),
        MockResponse::Text("Missing parameter.".into()),
    ]);
    let events = env.run_inference(&provider).await;

    let output = events
        .iter()
        .find_map(|e| {
            if let EngineEvent::ToolCallResult { output, name, .. } = e
                && name == "ActivateSkill"
            {
                return Some(output.clone());
            }
            None
        })
        .unwrap();
    assert!(output.contains("Missing"));
}

// ── Compaction ────────────────────────────────────────────────

#[tokio::test]
async fn test_compact_session_summarizes_and_reduces_messages() {
    let env = Env::new().await;

    for i in 0..10 {
        env.db
            .insert_message(
                &env.session_id,
                &Role::User,
                Some(&format!("User message {i} about implementing feature X")),
                None,
                None,
                None,
            )
            .await
            .unwrap();
        env.db
            .insert_message(
                &env.session_id,
                &Role::Assistant,
                Some(&format!(
                    "Assistant response {i}: I've made the changes to file_{i}.rs"
                )),
                None,
                None,
                None,
            )
            .await
            .unwrap();
    }

    let before = env.db.load_context(&env.session_id).await.unwrap();
    assert_eq!(before.len(), 20);

    let provider: Arc<RwLock<Box<dyn LlmProvider>>> =
        Arc::new(RwLock::new(Box::new(MockProvider::new(vec![
            MockResponse::Text("Summary: User implemented feature X across 10 files.".into()),
        ]))));

    let result = compact::compact_session(
        &env.db,
        &env.session_id,
        100_000,
        &env.config.model_settings,
        &provider,
    )
    .await
    .unwrap();

    let compact_result = result.unwrap();
    assert!(compact_result.deleted > 0);
    assert!(compact_result.summary_tokens > 0);

    let after = env.db.load_context(&env.session_id).await.unwrap();
    assert!(after.len() < before.len());

    let has_summary = after.iter().any(|m| {
        m.content
            .as_deref()
            .unwrap_or("")
            .contains("Compacted conversation summary")
    });
    assert!(has_summary);
}

#[tokio::test]
async fn test_compact_skips_short_conversation() {
    use koda_core::compact::CompactSkip;

    let env = Env::new().await;
    env.insert_user_message("hello").await;
    env.db
        .insert_message(
            &env.session_id,
            &Role::Assistant,
            Some("hi"),
            None,
            None,
            None,
        )
        .await
        .unwrap();

    let provider: Arc<RwLock<Box<dyn LlmProvider>>> =
        Arc::new(RwLock::new(Box::new(MockProvider::new(vec![]))));

    let result = compact::compact_session(
        &env.db,
        &env.session_id,
        100_000,
        &env.config.model_settings,
        &provider,
    )
    .await
    .unwrap();

    assert!(matches!(result, Err(CompactSkip::TooShort(2))));
}

// ── File ownership ────────────────────────────────────────────

/// Write creates a file, then Delete of that file auto-approves (#465).
#[tokio::test]
async fn test_write_then_delete_auto_approves_owned_file() {
    let env = Env::new().await;
    let target = env.root.join("ephemeral_draft.md");
    env.insert_user_message("create then cleanup").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "Write",
            serde_json::json!({
                "path": target.to_string_lossy(),
                "content": "draft content"
            }),
        ),
        MockResponse::tool_call(
            "Delete",
            serde_json::json!({"path": target.to_string_lossy()}),
        ),
        MockResponse::Text("Cleaned up.".into()),
    ]);
    let events = env.run_inference(&provider).await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::ToolCallResult { name, .. } if name == "Write")),
        "expected Write tool result"
    );

    let delete_result = events.iter().find_map(|e| {
        if let EngineEvent::ToolCallResult { output, name, .. } = e
            && name == "Delete"
        {
            return Some(output.clone());
        }
        None
    });
    assert!(
        delete_result.is_some(),
        "Delete should have executed (auto-approved for owned file)"
    );
    assert!(!target.exists(), "ephemeral file should be deleted");
    assert!(events.iter().any(|e| matches!(e, EngineEvent::TextDone)));
}
