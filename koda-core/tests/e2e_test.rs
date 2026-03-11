//! End-to-end tests: mock provider → inference loop → real tools → DB persistence.
//!
//! These tests exercise the full engine pipeline without a real LLM.
//! All file operations happen in isolated temp directories.

use anyhow::Result;
use async_trait::async_trait;
use koda_core::persistence::Persistence;
use koda_core::{
    approval::ApprovalMode,
    config::{KodaConfig, ModelSettings, ProviderType},
    db::{Database, Role},
    engine::{EngineCommand, EngineEvent, sink::TestSink},
    inference::{self, InferenceContext},
    providers::{
        ChatMessage, LlmProvider, LlmResponse, ModelInfo, StreamChunk, ToolDefinition,
        mock::{MockProvider, MockResponse},
    },
    settings::Settings,
    tools::ToolRegistry,
};
use std::path::PathBuf;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Mutex to serialize tests that share process-global env vars
/// (KODA_MOCK_RESPONSES). `#[tokio::test]` runs tests concurrently
/// within the same process, so unsynchronized set_var/remove_var
/// on the same env var is a data race.
static ENV_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ── Test harness ──────────────────────────────────────────────

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    db: Database,
    session_id: String,
    config: KodaConfig,
    tools: ToolRegistry,
}

impl Env {
    async fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let db = Database::init(&root).await.unwrap();
        let session_id = db.create_session("test-agent", &root).await.unwrap();
        let config = KodaConfig::default_for_testing(ProviderType::LMStudio);
        let tools = ToolRegistry::new(root.clone(), config.max_context_tokens);
        Self {
            _tmp: tmp,
            root,
            db,
            session_id,
            config,
            tools,
        }
    }

    fn tool_defs(&self) -> Vec<koda_core::providers::ToolDefinition> {
        self.tools.get_definitions(&[])
    }

    async fn insert_user_message(&self, text: &str) {
        self.db
            .insert_message(&self.session_id, &Role::User, Some(text), None, None, None)
            .await
            .unwrap();
    }

    async fn run_inference(&self, provider: &MockProvider) -> Vec<EngineEvent> {
        let sink = TestSink::new();
        let (_, mut cmd_rx) = mpsc::channel::<EngineCommand>(1);
        let mut settings = Settings::load();
        let tool_defs = self.tool_defs();

        let result = inference::inference_loop(InferenceContext {
            project_root: &self.root,
            config: &self.config,
            db: &self.db,
            session_id: &self.session_id,
            system_prompt: "You are a test assistant.",
            provider,
            tools: &self.tools,
            tool_defs: &tool_defs,
            pending_images: None,
            mode: ApprovalMode::Auto,
            settings: &mut settings,
            sink: &sink,
            cancel: CancellationToken::new(),
            cmd_rx: &mut cmd_rx,
            skip_probe: true,
        })
        .await;

        assert!(result.is_ok(), "inference_loop failed: {:?}", result.err());
        sink.events()
    }
}

// ── Tests ─────────────────────────────────────────────────────

#[tokio::test]
async fn test_text_response_streams_and_persists() {
    let env = Env::new().await;
    env.insert_user_message("say hello").await;

    let provider = MockProvider::new(vec![MockResponse::Text("Hello, world!".into())]);
    let events = env.run_inference(&provider).await;

    // Should have streaming text events
    let text_deltas: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, EngineEvent::TextDelta { .. }))
        .collect();
    assert!(!text_deltas.is_empty(), "expected TextDelta events");
    assert!(
        events.iter().any(|e| matches!(e, EngineEvent::TextDone)),
        "expected TextDone"
    );

    // Should have persisted to DB
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

#[tokio::test]
async fn test_tool_call_executes_and_returns() {
    let env = Env::new().await;
    env.insert_user_message("run echo").await;

    // Mock: first call returns a Bash tool call, second returns final text.
    let provider = MockProvider::new(vec![
        MockResponse::tool_call("Bash", serde_json::json!({"command": "echo hello"})),
        MockResponse::Text("Done! The command printed hello.".into()),
    ]);
    let events = env.run_inference(&provider).await;

    // Should have tool call start + result events
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

    // Should end with text response
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

#[tokio::test]
async fn test_read_tool_in_sandbox() {
    let env = Env::new().await;

    // Create a file in the sandbox for the Read tool to find.
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

    // Tool result should contain the file content
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

#[tokio::test]
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

#[tokio::test]
async fn test_provider_error_emits_error_event() {
    let env = Env::new().await;
    env.insert_user_message("trigger error").await;

    let provider = MockProvider::new(vec![MockResponse::Error("Internal server error".into())]);
    let sink = TestSink::new();
    let (_, mut cmd_rx) = mpsc::channel::<EngineCommand>(1);
    let mut settings = Settings::load();
    let tool_defs = env.tool_defs();

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
        mode: ApprovalMode::Auto,
        settings: &mut settings,
        sink: &sink,
        cancel: CancellationToken::new(),
        cmd_rx: &mut cmd_rx,
        skip_probe: true,
    })
    .await;

    // Provider error should propagate as an Err (wrapped by inference_loop)
    assert!(result.is_err(), "expected error from provider failure");
    let err = result.unwrap_err();
    let chain = format!("{err:?}"); // debug format shows full error chain
    assert!(
        chain.contains("Internal server error"),
        "error chain should contain provider message, got: {chain}"
    );
}

#[tokio::test]
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

    // Verify both messages are in the DB
    let messages = env.db.load_context(&env.session_id, 100_000).await.unwrap();

    let contents: Vec<String> = messages.iter().filter_map(|m| m.content.clone()).collect();

    assert!(
        contents.iter().any(|c| c.contains("first question")),
        "history should contain first user message"
    );
    assert!(
        contents.iter().any(|c| c.contains("first answer")),
        "history should contain first assistant response"
    );
    assert!(
        contents.iter().any(|c| c.contains("second question")),
        "history should contain second user message"
    );
    assert!(
        contents.iter().any(|c| c.contains("second answer")),
        "history should contain second assistant response"
    );
}

#[tokio::test]
async fn test_cancel_during_streaming() {
    let env = Env::new().await;
    env.insert_user_message("hello").await;

    struct HangingProvider;

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
        ) -> Result<mpsc::Receiver<StreamChunk>> {
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

    let sink = TestSink::new();
    let (_, mut cmd_rx) = mpsc::channel::<EngineCommand>(1);
    let mut settings = Settings::load();
    let tool_defs = env.tool_defs();
    let cancel = CancellationToken::new();

    // Cancel after 100ms
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        cancel_clone.cancel();
    });

    let start = std::time::Instant::now();
    let result = inference::inference_loop(InferenceContext {
        project_root: &env.root,
        config: &env.config,
        db: &env.db,
        session_id: &env.session_id,
        system_prompt: "You are a test assistant.",
        provider: &HangingProvider,
        tools: &env.tools,
        tool_defs: &tool_defs,
        pending_images: None,
        mode: ApprovalMode::Auto,
        settings: &mut settings,
        sink: &sink,
        cancel,
        cmd_rx: &mut cmd_rx,
        skip_probe: true,
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

#[tokio::test]
async fn test_glob_tool_in_sandbox() {
    let env = Env::new().await;

    // Create some files for Glob to find
    let src_dir = env.root.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(src_dir.join("main.rs"), "fn main() {}").unwrap();
    std::fs::write(src_dir.join("lib.rs"), "pub mod foo;").unwrap();

    env.insert_user_message("find rust files").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call("Glob", serde_json::json!({"pattern": "src/*.rs"})),
        MockResponse::Text("Found 2 Rust files.".into()),
    ]);
    let events = env.run_inference(&provider).await;

    let tool_result = events.iter().find_map(|e| {
        if let EngineEvent::ToolCallResult { output, name, .. } = e
            && name == "Glob"
        {
            return Some(output.clone());
        }
        None
    });
    assert!(tool_result.is_some(), "expected Glob tool result");
    let output = tool_result.unwrap();
    assert!(output.contains("main.rs"), "Glob should find main.rs");
    assert!(output.contains("lib.rs"), "Glob should find lib.rs");
}

// ── Sub-agent E2E ─────────────────────────────────────────────

#[tokio::test]
async fn test_sub_agent_invocation_e2e() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::new().await;

    // Create a project-level agent config that uses the Mock provider.
    let agents_dir = env.root.join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join("echo-agent.json"),
        serde_json::json!({
            "name": "echo-agent",
            "system_prompt": "You are a simple echo agent. Repeat back the user's prompt verbatim.",
            "allowed_tools": [],
            "provider": "mock",
            "base_url": "http://localhost:0"
        })
        .to_string(),
    )
    .unwrap();

    // Set mock responses for the sub-agent (read by MockProvider::from_env).
    // SAFETY: ENV_MUTEX serializes all tests that touch this env var.
    unsafe {
        std::env::set_var(
            "KODA_MOCK_RESPONSES",
            r#"[{"text": "Echo: review the auth module"}]"#,
        );
    }

    env.insert_user_message("delegate to echo-agent").await;

    // Parent mock: first returns InvokeAgent tool call, then final text.
    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "InvokeAgent",
            serde_json::json!({
                "agent_name": "echo-agent",
                "prompt": "review the auth module"
            }),
        ),
        MockResponse::Text("Sub-agent says: Echo: review the auth module".into()),
    ]);
    let events = env.run_inference(&provider).await;

    // Clean up env var
    // SAFETY: ENV_MUTEX serializes all tests that touch this env var.
    unsafe {
        std::env::remove_var("KODA_MOCK_RESPONSES");
    }

    // Should have SubAgentStart event
    assert!(
        events.iter().any(
            |e| matches!(e, EngineEvent::SubAgentStart { agent_name } if agent_name == "echo-agent")
        ),
        "expected SubAgentStart for echo-agent, got: {events:?}"
    );

    // Should have tool result containing the sub-agent's output
    let tool_result = events.iter().find_map(|e| {
        if let EngineEvent::ToolCallResult { output, name, .. } = e
            && name == "InvokeAgent"
        {
            return Some(output.clone());
        }
        None
    });
    assert!(
        tool_result.is_some(),
        "expected InvokeAgent tool result, got: {events:?}"
    );
    assert!(
        tool_result
            .unwrap()
            .contains("Echo: review the auth module"),
        "sub-agent result should contain echoed prompt"
    );

    // Should end with final text response
    let last = env
        .db
        .last_assistant_message(&env.session_id)
        .await
        .unwrap();
    assert!(
        last.contains("Sub-agent says"),
        "final response should reference sub-agent output: {last}"
    );
}

#[tokio::test]
async fn test_sub_agent_cache_hit_skips_llm() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::new().await;

    // Create the same echo-agent
    let agents_dir = env.root.join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join("echo-agent.json"),
        serde_json::json!({
            "name": "echo-agent",
            "system_prompt": "You are a simple echo agent.",
            "allowed_tools": [],
            "provider": "mock",
            "base_url": "http://localhost:0"
        })
        .to_string(),
    )
    .unwrap();

    // Sub-agent mock: only ONE response available.
    // If the cache doesn't work, the second InvokeAgent call will hit
    // the mock again and get an empty text (exhausted), or error.
    // SAFETY: ENV_MUTEX serializes all tests that touch this env var.
    unsafe {
        std::env::set_var("KODA_MOCK_RESPONSES", r#"[{"text": "cached result"}]"#);
    }
    env.insert_user_message("call the agent twice").await;

    // Parent mock: calls the SAME sub-agent with the SAME prompt twice,
    // then returns final text. The second call should hit the cache.
    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "InvokeAgent",
            serde_json::json!({"agent_name": "echo-agent", "prompt": "do the thing"}),
        ),
        MockResponse::tool_call(
            "InvokeAgent",
            serde_json::json!({"agent_name": "echo-agent", "prompt": "do the thing"}),
        ),
        MockResponse::Text("Done with both calls.".into()),
    ]);
    let events = env.run_inference(&provider).await;
    // SAFETY: ENV_MUTEX serializes all tests that touch this env var.
    unsafe {
        std::env::remove_var("KODA_MOCK_RESPONSES");
    }

    // Should have cache hit info event on the second invocation
    let cache_hit = events
        .iter()
        .any(|e| matches!(e, EngineEvent::Info { message } if message.contains("cache hit")));
    assert!(cache_hit, "expected cache hit event, got: {events:?}");

    // Should still produce final response
    let last = env
        .db
        .last_assistant_message(&env.session_id)
        .await
        .unwrap();
    assert!(
        last.contains("Done with both calls"),
        "should complete with final response: {last}"
    );
}

// ── Compaction E2E ────────────────────────────────────────────

#[tokio::test]
async fn test_compact_session_summarizes_and_reduces_messages() {
    use koda_core::compact;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    let env = Env::new().await;

    // Stuff 10 user/assistant message pairs into the session
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

    // Verify we have 20 messages
    let before = env.db.load_context(&env.session_id, 100_000).await.unwrap();
    assert_eq!(before.len(), 20);

    // Create a mock provider that returns a summary
    let provider: Arc<RwLock<Box<dyn LlmProvider>>> =
        Arc::new(RwLock::new(Box::new(MockProvider::new(vec![
            MockResponse::Text("Summary: User implemented feature X across 10 files.".into()),
        ]))));

    // Run compaction
    let result = compact::compact_session(
        &env.db,
        &env.session_id,
        100_000,
        &env.config.model_settings,
        &provider,
    )
    .await
    .unwrap();

    // Should succeed
    let compact_result = result.unwrap();
    assert!(compact_result.deleted > 0, "should have deleted messages");
    assert!(
        compact_result.summary_tokens > 0,
        "should have summary tokens"
    );

    // Verify message count decreased
    let after = env.db.load_context(&env.session_id, 100_000).await.unwrap();
    assert!(
        after.len() < before.len(),
        "message count should decrease after compaction: {} < {}",
        after.len(),
        before.len()
    );

    // Verify the summary is in the history
    let has_summary = after.iter().any(|m| {
        m.content
            .as_deref()
            .unwrap_or("")
            .contains("Compacted conversation summary")
    });
    assert!(has_summary, "should contain compaction summary message");
}

#[tokio::test]
async fn test_compact_skips_short_conversation() {
    use koda_core::compact::{self, CompactSkip};
    use std::sync::Arc;
    use tokio::sync::RwLock;

    let env = Env::new().await;

    // Only 2 messages — too short
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

    assert!(
        matches!(result, Err(CompactSkip::TooShort(2))),
        "should skip compaction for short conversations"
    );
}
