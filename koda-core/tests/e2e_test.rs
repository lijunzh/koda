//! End-to-end tests: core pipeline.
//!
//! Mock provider → inference loop → real tools → DB persistence.
//! All file operations happen in isolated temp directories.

use anyhow::Result;
use async_trait::async_trait;
use koda_core::{
    config::ModelSettings,
    engine::{EngineCommand, EngineEvent},
    inference::{self, InferenceContext},
    persistence::Persistence,
    providers::{LlmResponse, ModelInfo},
    trust::TrustMode,
};
use koda_test_utils::{ChatMessage, Env, LlmProvider, MockProvider, MockResponse, ToolDefinition};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// ── Core pipeline tests ───────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_text_response_streams_and_persists() {
    let env = Env::new().await;
    env.insert_user_message("say hello").await;

    let provider = MockProvider::new(vec![MockResponse::Text("Hello, world!".into())]);
    let events = env.run_inference(&provider).await;

    let text_deltas: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, EngineEvent::TextDelta { .. }))
        .collect();
    assert!(!text_deltas.is_empty(), "expected TextDelta events");
    assert!(
        events.iter().any(|e| matches!(e, EngineEvent::TextDone)),
        "expected TextDone"
    );

    let last = env
        .db
        .last_assistant_message(&env.session_id)
        .await
        .unwrap();
    assert!(
        last.contains("Hello, world!"),
        "DB should contain response: {last}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_tool_call_executes_and_returns() {
    let env = Env::new().await;
    env.insert_user_message("run echo").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call("Bash", serde_json::json!({"command": "echo hello"})),
        MockResponse::Text("Done! The command printed hello.".into()),
    ]);
    let events = env.run_inference(&provider).await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::ToolCallStart { name, .. } if name == "Bash")),
        "expected ToolCallStart for Bash"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::ToolCallResult { name, .. } if name == "Bash")),
        "expected ToolCallResult for Bash"
    );
    assert!(
        events.iter().any(|e| matches!(e, EngineEvent::TextDone)),
        "expected TextDone after tool execution"
    );

    let last = env
        .db
        .last_assistant_message(&env.session_id)
        .await
        .unwrap();
    assert!(
        last.contains("Done!"),
        "DB should contain final response: {last}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_read_tool_in_sandbox() {
    let env = Env::new().await;
    let test_file = env.root.join("test_data.txt");
    std::fs::write(&test_file, "sandbox content here").unwrap();

    env.insert_user_message("read the file").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "Read",
            serde_json::json!({"path": test_file.to_string_lossy()}),
        ),
        MockResponse::Text("The file contains sandbox content.".into()),
    ]);
    let events = env.run_inference(&provider).await;

    let tool_result = events.iter().find_map(|e| {
        if let EngineEvent::ToolCallResult { output, name, .. } = e
            && name == "Read"
        {
            return Some(output.clone());
        }
        None
    });
    assert!(
        tool_result.is_some(),
        "expected Read tool result in events: {events:?}"
    );
    assert!(
        tool_result.unwrap().contains("sandbox content here"),
        "Read tool should return file content"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_write_tool_creates_file_in_sandbox() {
    let env = Env::new().await;
    env.insert_user_message("create a file").await;

    let target = env.root.join("created.txt");
    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "Write",
            serde_json::json!({
                "path": target.to_string_lossy(),
                "content": "hello from mock"
            }),
        ),
        MockResponse::Text("File created.".into()),
    ]);
    env.run_inference(&provider).await;

    assert!(target.exists(), "Write tool should create the file");
    let content = std::fs::read_to_string(&target).unwrap();
    assert_eq!(content, "hello from mock");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_provider_error_emits_error_event() {
    let env = Env::new().await;
    env.insert_user_message("trigger error").await;

    let provider = MockProvider::new(vec![MockResponse::Error("Internal server error".into())]);
    let sink = koda_core::engine::sink::TestSink::new();
    let (_, mut cmd_rx) = mpsc::channel::<EngineCommand>(1);
    let tool_defs = env.tool_defs();
    let mut file_tracker =
        koda_core::file_tracker::FileTracker::new(&env.session_id, env.db.clone()).await;

    let result = inference::inference_loop(InferenceContext {
        project_root: &env.root,
        config: &env.config,
        db: &env.db,
        session_id: &env.session_id,
        system_prompt: "You are a test assistant.",
        provider: &provider,
        tools: &env.tools,
        tool_defs: &tool_defs,
        pending_images: None,
        mode: TrustMode::Auto,
        sink: &sink,
        cancel: CancellationToken::new(),
        cmd_rx: &mut cmd_rx,
        file_tracker: &mut file_tracker,
        bg_agents: &koda_core::child_agent::new_shared(),
        sub_agent_cache: &koda_core::sub_agent_cache::SubAgentCache::new(),
        agent_path: &koda_core::agent::AgentPath::root(),
    })
    .await;

    assert!(
        result.is_ok(),
        "server errors should end gracefully, not crash"
    );

    // The warning should be emitted as an event, not as a fatal error
    let events = sink.events();
    let has_warn = events.iter().any(|e| {
        if let EngineEvent::Warn { message } = e {
            message.contains("server error")
        } else {
            false
        }
    });
    assert!(
        has_warn,
        "expected a Warn event about server error, got events: {events:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_session_history_persists_across_turns() {
    let env = Env::new().await;

    // Turn 1
    env.insert_user_message("first question").await;
    let provider1 = MockProvider::new(vec![MockResponse::Text("first answer".into())]);
    env.run_inference(&provider1).await;

    // Turn 2
    env.insert_user_message("second question").await;
    let provider2 = MockProvider::new(vec![MockResponse::Text("second answer".into())]);
    env.run_inference(&provider2).await;

    let messages = env.db.load_context(&env.session_id).await.unwrap();
    let contents: Vec<String> = messages.iter().filter_map(|m| m.content.clone()).collect();

    assert!(contents.iter().any(|c| c.contains("first question")));
    assert!(contents.iter().any(|c| c.contains("first answer")));
    assert!(contents.iter().any(|c| c.contains("second question")));
    assert!(contents.iter().any(|c| c.contains("second answer")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cancel_during_streaming() {
    let env = Env::new().await;
    env.insert_user_message("hello").await;

    struct HangingProvider {
        entered: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    }

    #[async_trait]
    impl LlmProvider for HangingProvider {
        async fn chat(
            &self,
            _: &[ChatMessage],
            _: &[ToolDefinition],
            _: &ModelSettings,
        ) -> Result<LlmResponse> {
            unreachable!()
        }
        async fn chat_stream(
            &self,
            _: &[ChatMessage],
            _: &[ToolDefinition],
            _: &ModelSettings,
        ) -> Result<koda_core::providers::stream_collector::SseCollector> {
            // **#1109 F3**: signal entry so the test cancels deterministically
            // instead of guessing a wall-clock delay.
            if let Some(tx) = self.entered.lock().unwrap().take() {
                let _ = tx.send(());
            }
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            unreachable!()
        }
        async fn list_models(&self) -> Result<Vec<ModelInfo>> {
            Ok(vec![])
        }
        fn provider_name(&self) -> &str {
            "hanging"
        }
    }

    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let provider = HangingProvider {
        entered: std::sync::Mutex::new(Some(entered_tx)),
    };

    let sink = koda_core::engine::sink::TestSink::new();
    let (_, mut cmd_rx) = mpsc::channel::<EngineCommand>(1);
    let tool_defs = env.tool_defs();
    let cancel = CancellationToken::new();
    let mut file_tracker =
        koda_core::file_tracker::FileTracker::new(&env.session_id, env.db.clone()).await;

    // **#1109 F3**: was `sleep(100ms).await` — replaced with oneshot wait.
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        let _ = entered_rx.await;
        cancel_clone.cancel();
    });

    let start = std::time::Instant::now();
    let result = inference::inference_loop(InferenceContext {
        project_root: &env.root,
        config: &env.config,
        db: &env.db,
        session_id: &env.session_id,
        system_prompt: "You are a test assistant.",
        provider: &provider,
        tools: &env.tools,
        tool_defs: &tool_defs,
        pending_images: None,
        mode: TrustMode::Auto,
        sink: &sink,
        cancel,
        cmd_rx: &mut cmd_rx,
        file_tracker: &mut file_tracker,
        bg_agents: &koda_core::child_agent::new_shared(),
        sub_agent_cache: &koda_core::sub_agent_cache::SubAgentCache::new(),
        agent_path: &koda_core::agent::AgentPath::root(),
    })
    .await;

    let elapsed = start.elapsed();
    assert!(result.is_ok(), "cancel should be graceful");
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "should cancel quickly, took {elapsed:?}"
    );
    assert!(
        sink.events()
            .iter()
            .any(|e| matches!(e, EngineEvent::Warn { message } if message == "Interrupted")),
        "should emit Interrupted warning"
    );
}
