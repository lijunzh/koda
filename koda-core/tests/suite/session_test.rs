//! Session lifecycle tests: TurnStart/TurnEnd events, cancellation, and DB persistence.
//!
//! Uses MockProvider + TestSink from koda-test-utils.
//! Run with: `cargo test -p koda-core`

use anyhow::Result;
use async_trait::async_trait;
use koda_core::{
    engine::{EngineCommand, EngineEvent, event::TurnEndReason},
    providers::{LlmResponse, ModelInfo},
    session::{KodaSession, SessionCancel},
    tools::ToolRegistry,
    trust::TrustMode,
};
use koda_state::session_store::SessionStore;
use koda_test_utils::{
    ChatMessage, Env, LlmProvider, MockProvider, MockResponse, TestSink, ToolDefinition,
};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// ── Session-specific helper ──────────────────────────────────────────────────

/// Build a KodaSession, injecting an arbitrary provider.
///
/// Bypasses `KodaSession::new()` (which calls `create_provider` internally)
/// so tests can supply a pre-configured MockProvider.  A fresh ToolRegistry
/// is created each call because ToolRegistry does not implement Clone.
async fn make_session(env: &Env, provider: Box<dyn LlmProvider>) -> (KodaSession, SessionCancel) {
    let cancel = SessionCancel::new();
    let tools = ToolRegistry::new(env.root.clone(), env.config.max_context_tokens);

    let agent = Arc::new(koda_core::agent::KodaAgent {
        project_root: env.root.clone(),
        tools,
        // #1265 item 5 PR-2: read definitions from a fresh
        // `ToolCatalog` instead of spinning up a second
        // `ToolRegistry`. Cheaper and clearer — we don't need any of
        // the registry's stateful fields just to list tools.
        tool_defs: koda_core::tools::ToolCatalog::new().get_definitions(&[], &[]),
        system_prompt: "You are a test assistant.".to_string(),
        prompt_model: String::new(),
        semantic_memory: String::new(),
        interactive: false,
    });

    // Wire the DB+session into the ToolRegistry so RecallContext works.
    agent
        .tools
        .set_session(Arc::new(env.db.clone()), env.session_id.clone());

    let session = KodaSession {
        id: env.session_id.clone(),
        agent,
        db: env.db.clone(),
        provider,
        mode: TrustMode::Auto,
        cancel: cancel.clone(),
        title_set: false,
        proxy: None,
        socks5_proxy: None,
        bg_agents: koda_core::child_agent::new_shared(),
        git_snapshot: Default::default(),
        sub_agent_cache: koda_core::sub_agent_cache::SubAgentCache::new(),
        session_allows: Default::default(),
        // #1321: tests don't need the bg-event forwarder; production
        // (the TUI) calls `attach_event_sink` to spawn it.
        event_forwarder: None,
        queued_steers: Vec::new(),
        output_schema: None,
    };
    (session, cancel)
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// `run_turn()` must emit `TurnStart` as the first event and
/// `TurnEnd { reason: Complete }` as the last event after a successful turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_run_turn_emits_turn_start_and_end() {
    let env = Env::new().await;
    env.insert_user_message("say hello").await;

    let provider = Box::new(MockProvider::new(vec![MockResponse::Text(
        "Hello!".to_string(),
    )]));
    let (mut session, _cancel) = make_session(&env, provider).await;

    let sink = TestSink::new();
    let (_, mut cmd_rx) = mpsc::channel::<EngineCommand>(1);

    let result = session
        .run_turn(&env.config, None, &sink, &mut cmd_rx, None)
        .await;
    assert!(
        result.is_ok(),
        "run_turn should succeed: {:?}",
        result.err()
    );

    let events = sink.events();

    // TurnStart must be the first event emitted.
    let first = events.first().expect("expected at least one event");
    assert!(
        matches!(first, EngineEvent::TurnStart { .. }),
        "first event must be TurnStart, got: {first:?}"
    );

    // TurnEnd must be the last event emitted.
    let last = events.last().expect("expected at least one event");
    assert!(
        matches!(last, EngineEvent::TurnEnd { .. }),
        "last event must be TurnEnd, got: {last:?}"
    );

    // Reason must be Complete on a successful turn.
    if let EngineEvent::TurnEnd { reason, .. } = last {
        assert_eq!(
            *reason,
            TurnEndReason::Complete,
            "TurnEnd reason should be Complete after successful turn"
        );
    }

    // The turn_id in TurnStart and TurnEnd must match.
    let start_id = if let EngineEvent::TurnStart { turn_id } = first {
        turn_id.clone()
    } else {
        unreachable!()
    };
    let end_id = if let EngineEvent::TurnEnd { turn_id, .. } = last {
        turn_id.clone()
    } else {
        unreachable!()
    };
    assert_eq!(
        start_id, end_id,
        "TurnStart and TurnEnd must share the same turn_id"
    );
}

/// Cancelling the token during inference causes `TurnEnd` to carry reason `Cancelled`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_cancellation_produces_turn_end_cancelled() {
    let env = Env::new().await;
    env.insert_user_message("hello").await;

    // A provider that signals when entered, then hangs forever so that
    // cancellation can be observed deterministically (#1109 F3).
    struct HangingProvider {
        entered: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    }

    #[async_trait]
    impl LlmProvider for HangingProvider {
        async fn chat(
            &self,
            _: &[ChatMessage],
            _: &[ToolDefinition],
            _: &koda_core::config::ModelSettings,
        ) -> Result<LlmResponse> {
            unreachable!()
        }
        async fn chat_stream(
            &self,
            _: &[ChatMessage],
            _: &[ToolDefinition],
            _: &koda_core::config::ModelSettings,
        ) -> Result<koda_core::providers::stream_collector::SseCollector> {
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
    let (mut session, cancel) = make_session(&env, Box::new(provider)).await;

    // **#1109 F3**: was `sleep(100ms).await` — replaced with oneshot wait
    // so cancel fires the moment inference reaches the provider.
    let cancel_clone = cancel.clone();
    let cancelled_at = Arc::new(Mutex::new(None));
    let cancelled_at_writer = Arc::clone(&cancelled_at);
    tokio::spawn(async move {
        let _ = entered_rx.await;
        *cancelled_at_writer.lock().unwrap() = Some(std::time::Instant::now());
        cancel_clone.interrupt();
    });

    let sink = TestSink::new();
    let (_, mut cmd_rx) = mpsc::channel::<EngineCommand>(1);

    let result = session
        .run_turn(&env.config, None, &sink, &mut cmd_rx, None)
        .await;

    // Measured from the cancel signal, not from before the call: the loop's
    // own setup (database, context assembly) is not what this bounds, and on a
    // loaded CI runner it alone pushed the old 2-second total over the line.
    // Five seconds still proves the provider's 60-second hang was not waited out.
    let elapsed = cancelled_at
        .lock()
        .unwrap()
        .expect("cancel fired once the provider was entered")
        .elapsed();
    assert!(
        result.is_ok(),
        "cancellation should be graceful, not an error"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "cancellation should unblock within 5s of the signal, took {elapsed:?}"
    );

    let events = sink.events();

    // A TurnEnd event must be present with reason Cancelled.
    let turn_end_reason = events.iter().find_map(|e| {
        if let EngineEvent::TurnEnd { reason, .. } = e {
            Some(reason.clone())
        } else {
            None
        }
    });
    assert!(
        turn_end_reason.is_some(),
        "expected a TurnEnd event after cancellation, got: {events:?}"
    );
    assert_eq!(
        turn_end_reason.unwrap(),
        TurnEndReason::Cancelled,
        "TurnEnd reason must be Cancelled when the token is cancelled"
    );
}

/// **#1208 regression.** A per-turn cancel token (the `turn_cancel`
/// argument added so the TUI can fire Ctrl+C without nuking
/// `session.cancel`, which bg agents derive from — see #1200) must
/// stop the inference loop just like the session-token path does.
///
/// Without the wiring, firing the per-turn child token left the
/// inference loop spinning because it was checking `session.cancel`
/// instead. The bug surfaced as: bg sub-agents stopped, master turn
/// kept running, model retried by spawning more bg agents.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_turn_cancel_token_stops_inference() {
    let env = Env::new().await;
    env.insert_user_message("hello").await;

    // Same hanging-provider trick as `session_cancellation_produces_turn_end_cancelled`:
    // signal once we're inside the LLM call, then sleep so the test
    // drives the cancel deterministically.
    struct HangingProvider {
        entered: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    }

    #[async_trait]
    impl LlmProvider for HangingProvider {
        async fn chat(
            &self,
            _: &[ChatMessage],
            _: &[ToolDefinition],
            _: &koda_core::config::ModelSettings,
        ) -> Result<LlmResponse> {
            unreachable!()
        }
        async fn chat_stream(
            &self,
            _: &[ChatMessage],
            _: &[ToolDefinition],
            _: &koda_core::config::ModelSettings,
        ) -> Result<koda_core::providers::stream_collector::SseCollector> {
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
    let (mut session, session_cancel) = make_session(&env, Box::new(provider)).await;

    // The whole point of #1208: cancel the *per-turn* token, NOT
    // session.cancel — then assert the turn still ends, AND
    // session.cancel is still alive afterwards (so bg agents survive).
    let turn_cancel = CancellationToken::new();
    let turn_cancel_clone = turn_cancel.clone();
    let cancelled_at = Arc::new(Mutex::new(None));
    let cancelled_at_writer = Arc::clone(&cancelled_at);
    tokio::spawn(async move {
        let _ = entered_rx.await;
        *cancelled_at_writer.lock().unwrap() = Some(std::time::Instant::now());
        turn_cancel_clone.cancel();
    });

    let sink = TestSink::new();
    let (_, mut cmd_rx) = mpsc::channel::<EngineCommand>(1);

    let result = session
        .run_turn(
            &env.config,
            None,
            &sink,
            &mut cmd_rx,
            Some(turn_cancel.clone()),
        )
        .await;
    // Measured from the cancel signal, not from before the call: the loop's
    // own setup (database, context assembly) is not what this bounds, and on a
    // loaded CI runner it alone pushed the old 2-second total over the line.
    // Five seconds still proves the provider's 60-second hang was not waited out.
    let elapsed = cancelled_at
        .lock()
        .unwrap()
        .expect("cancel fired once the provider was entered")
        .elapsed();

    assert!(
        result.is_ok(),
        "per-turn cancellation should be graceful, not an error"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "per-turn cancel should unblock inference within 5s of the signal, took {elapsed:?}"
    );
    assert!(
        turn_cancel.is_cancelled(),
        "per-turn token should remain in the cancelled state"
    );
    assert!(
        !session_cancel.current().is_cancelled(),
        "session.cancel must NOT be fired when only the per-turn token cancels \
         (this is the whole #1208 fix — bg agents survive across cancelled turns)"
    );
}

/// Messages from two separate `run_turn()` calls on the same session must
/// both appear in the DB afterward.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_persists_messages_across_two_turns() {
    let env = Env::new().await;

    // --- Turn 1 ---
    env.insert_user_message("first question").await;

    let provider1 = Box::new(MockProvider::new(vec![MockResponse::Text(
        "first answer".to_string(),
    )]));
    let (mut session1, _cancel1) = make_session(&env, provider1).await;
    let sink1 = TestSink::new();
    let (_, mut cmd_rx1) = mpsc::channel::<EngineCommand>(1);
    session1
        .run_turn(&env.config, None, &sink1, &mut cmd_rx1, None)
        .await
        .expect("turn 1 should succeed");

    // Verify turn 1 ended with Complete.
    assert!(
        sink1.events().iter().any(|e| matches!(
            e,
            EngineEvent::TurnEnd {
                reason: TurnEndReason::Complete,
                ..
            }
        )),
        "turn 1 should end with Complete"
    );

    // --- Turn 2 ---
    env.insert_user_message("second question").await;

    let provider2 = Box::new(MockProvider::new(vec![MockResponse::Text(
        "second answer".to_string(),
    )]));
    // A new KodaSession sharing the same DB and session_id represents the
    // continuation of the conversation after, e.g., a model swap.
    let (mut session2, _cancel2) = make_session(&env, provider2).await;
    let sink2 = TestSink::new();
    let (_, mut cmd_rx2) = mpsc::channel::<EngineCommand>(1);
    session2
        .run_turn(&env.config, None, &sink2, &mut cmd_rx2, None)
        .await
        .expect("turn 2 should succeed");

    // Verify both turns' messages are in the DB.
    let messages: Vec<koda_state::session_store::Message> =
        env.db.load_context(&env.session_id).await.unwrap();
    let contents: Vec<String> = messages
        .iter()
        .filter_map(|m: &koda_state::session_store::Message| m.content.clone())
        .collect();

    assert!(
        contents
            .iter()
            .any(|c: &String| c.contains("first question")),
        "DB should contain first user message"
    );
    assert!(
        contents.iter().any(|c: &String| c.contains("first answer")),
        "DB should contain first assistant response"
    );
    assert!(
        contents
            .iter()
            .any(|c: &String| c.contains("second question")),
        "DB should contain second user message"
    );
    assert!(
        contents
            .iter()
            .any(|c: &String| c.contains("second answer")),
        "DB should contain second assistant response"
    );
}

/// Run one turn against a provider that fails the way `response` says and
/// return the `TurnEnd` reason, asserting that `run_turn` itself still
/// returns `Ok` (the TUI relies on that) and that the user saw `warn_text`.
async fn early_stop_turn_end(
    responses: Vec<MockResponse>,
    images: Option<Vec<koda_core::providers::ImageData>>,
    warn_text: &str,
) -> TurnEndReason {
    let mut env = Env::new().await;
    // The subject is how the turn ends; retries would only wait out backoff.
    env.config.request_max_retries = 0;
    env.config.stream_max_retries = 0;
    // A model the config says takes images, so an image reaches the server.
    env.config.input_modalities = Some(vec![
        koda_config::InputModality::Text,
        koda_config::InputModality::Image,
    ]);
    env.insert_user_message("hello").await;

    let provider = Box::new(MockProvider::new(responses));
    let (mut session, _cancel) = make_session(&env, provider).await;

    let sink = TestSink::new();
    let (_, mut cmd_rx) = mpsc::channel::<EngineCommand>(1);
    let result = session
        .run_turn(&env.config, images, &sink, &mut cmd_rx, None)
        .await;
    assert!(result.is_ok(), "early stop stays Ok: {:?}", result.err());

    let events = sink.events();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::Warn { message } if message.contains(warn_text))),
        "expected a warning containing {warn_text:?}: {events:?}"
    );
    match events.last() {
        Some(EngineEvent::TurnEnd { reason, .. }) => reason.clone(),
        other => panic!("last event must be TurnEnd, got {other:?}"),
    }
}

/// A provider server error ends the turn on a warning. `TurnEnd` must say
/// `Error` with that message so headless reports `success: false`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_server_error_turn_ends_with_error() {
    let reason = early_stop_turn_end(
        vec![
            MockResponse::Http {
                status: 500,
                body: "Internal Server Error".into(),
            };
            1
        ],
        None,
        "server error",
    )
    .await;
    match reason {
        TurnEndReason::Error { message } => assert!(message.contains("server error"), "{message}"),
        other => panic!("expected Error, got {other:?}"),
    }
}

/// An image the model can't take ends the turn on a warning, also an `Error`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_image_rejection_turn_ends_with_error() {
    let image = koda_core::providers::ImageData {
        media_type: "image/png".into(),
        base64: "AAAA".into(),
    };
    let reason = early_stop_turn_end(
        vec![MockResponse::Http {
            status: 400,
            body: "this model does not support image input".into(),
        }],
        Some(vec![image]),
        "rejected the image",
    )
    .await;
    match reason {
        TurnEndReason::Error { message } => {
            assert!(message.contains("rejected the image"), "{message}")
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

/// A connection dropped mid-stream discards the turn: also an `Error`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_network_drop_turn_ends_with_error() {
    let reason = early_stop_turn_end(
        vec![
            MockResponse::NetworkError {
                partial_text: "partial".into(),
                error: "connection reset".into(),
            };
            1
        ],
        None,
        "Stream failed",
    )
    .await;
    match reason {
        TurnEndReason::Error { message } => {
            assert!(message.contains("connection reset"), "{message}")
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

/// The contexts stored for `env`'s session, oldest first.
async fn stored_contexts(env: &Env) -> Vec<koda_state::session_store::TurnContextItem> {
    env.db
        .load_turn_records(&env.session_id)
        .await
        .unwrap()
        .into_iter()
        .filter(|record| record.kind == koda_state::session_store::TURN_CONTEXT_KIND)
        .map(|record| serde_json::from_str(&record.payload).unwrap())
        .collect()
}

/// Each turn stores what it ran with, as Codex stores a `TurnContextItem`
/// per user turn, so a change of model or mode between turns shows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_turn_stores_the_context_it_ran_with() {
    let env = Env::new().await;

    env.insert_user_message("first question").await;
    let provider = Box::new(MockProvider::new(vec![MockResponse::Text("one".into())]));
    let (mut session, _cancel) = make_session(&env, provider).await;
    let (_, mut cmd_rx) = mpsc::channel::<EngineCommand>(1);
    session
        .run_turn_as(
            "t1".into(),
            &env.config,
            None,
            &TestSink::new(),
            &mut cmd_rx,
            None,
        )
        .await
        .unwrap();

    env.insert_user_message("second question").await;
    let mut switched = env.config.clone();
    switched.model = "another-model".into();
    switched.model_settings.model = "another-model".into();
    switched.model_settings.reasoning_effort = Some("high".into());
    let provider = Box::new(MockProvider::new(vec![MockResponse::Text("two".into())]));
    let (mut session, _cancel) = make_session(&env, provider).await;
    session.mode = TrustMode::Auto;
    session
        .run_turn_as(
            "t2".into(),
            &switched,
            None,
            &TestSink::new(),
            &mut cmd_rx,
            None,
        )
        .await
        .unwrap();

    let contexts = stored_contexts(&env).await;
    assert_eq!(contexts.len(), 2, "{contexts:#?}");
    let (first, second) = (&contexts[0], &contexts[1]);

    assert_eq!(first.turn_id, "t1");
    assert_eq!(first.model, env.config.model);
    assert_eq!(first.trust_mode, "auto");
    assert_eq!(first.cwd, env.root.display().to_string());
    assert_eq!(first.koda_version, env!("CARGO_PKG_VERSION"));
    assert!(
        first.tools.iter().any(|tool| tool == "Read"),
        "{:?}",
        first.tools
    );
    let hash = first.system_prompt_sha256.as_deref().unwrap();
    assert_eq!(hash.len(), 64);
    assert!(first.project_memory_sha256.is_some());

    assert_eq!(second.turn_id, "t2");
    assert_eq!(second.model, "another-model");
    assert_eq!(second.reasoning_effort.as_deref(), Some("high"));
    assert_eq!(second.trust_mode, "safe");
}

/// A compaction turn stores its context too, as Codex's does: the model
/// that wrote the summary. It offers no tools and sends Koda's own prompt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_compaction_turn_stores_its_context() {
    let env = Env::new().await;
    let provider = Box::new(MockProvider::new(vec![MockResponse::Text(
        "summary".into(),
    )]));
    let (mut session, _cancel) = make_session(&env, provider).await;
    session
        .compact_as("c1".into(), &env.config, &TestSink::new(), None)
        .await
        .unwrap();

    let contexts = stored_contexts(&env).await;
    assert_eq!(contexts.len(), 1, "{contexts:#?}");
    assert_eq!(contexts[0].turn_id, "c1");
    assert_eq!(contexts[0].model, env.config.model);
    assert!(contexts[0].tools.is_empty());
    assert_eq!(contexts[0].system_prompt_sha256, None);
    assert_eq!(contexts[0].project_memory_sha256, None);
}

/// An image sent with a prompt stays in the conversation, as in Claude
/// Code: the call after a tool call and the next turn still carry it
/// (before Read v2 only the turn's first call did). A model that takes no
/// images gets a placeholder instead, and the stored image comes back for
/// one that does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_image_stays_in_the_conversation() {
    use koda_config::InputModality;
    use koda_core::inference_helpers::IMAGE_OMITTED;
    let mut env = Env::new().await;
    // A local vision model, which the config says takes images: Koda's
    // table has local models as text only.
    let vision = Some(vec![InputModality::Text, InputModality::Image]);
    env.config.input_modalities = vision.clone();
    // Room for the image's estimate, so no compaction folds it away.
    env.config.max_context_tokens = 200_000;
    env.config.model_settings.max_context_tokens = 200_000;
    let provider = MockProvider::new(vec![
        MockResponse::tool_call("Read", serde_json::json!({"file_path": "missing.txt"})),
        MockResponse::Text("A cat.".into()),
        MockResponse::Text("Still a cat.".into()),
        MockResponse::Text("I can't see it.".into()),
        MockResponse::Text("A cat again.".into()),
    ]);
    let calls = provider.recorded_calls_handle();
    let (mut session, _cancel) = make_session(&env, Box::new(provider)).await;
    let image = koda_core::providers::ImageData {
        media_type: "image/png".into(),
        base64: "AAAA".into(),
    };
    let mut turn = async |env: &Env, prompt: &str, images| {
        env.insert_user_message(prompt).await;
        let sink = TestSink::new();
        let (_, mut cmd_rx) = mpsc::channel::<EngineCommand>(1);
        session
            .run_turn(&env.config, images, &sink, &mut cmd_rx, None)
            .await
            .unwrap();
    };
    let images_in = |call: &[koda_core::providers::ChatMessage]| -> usize {
        call.iter()
            .map(|m| m.images.as_ref().map_or(0, Vec::len))
            .sum()
    };

    turn(&env, "What's this?", Some(vec![image])).await;
    turn(&env, "And now?", None).await;
    {
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 3, "a tool call, its answer, the next turn");
        for (i, call) in calls.iter().enumerate() {
            assert_eq!(
                images_in(call),
                1,
                "call {i} lost the image: {:?}",
                call.iter()
                    .map(|m| (
                        m.role.clone(),
                        m.content
                            .clone()
                            .unwrap_or_default()
                            .chars()
                            .take(40)
                            .collect::<String>(),
                        m.images.as_ref().map(Vec::len)
                    ))
                    .collect::<Vec<_>>()
            );
        }
    }

    // A model that takes text only: the image is a placeholder.
    env.config.input_modalities = Some(vec![InputModality::Text]);
    turn(&env, "Describe it again.", None).await;
    {
        let calls = calls.lock().unwrap();
        let last = calls.last().unwrap();
        assert_eq!(images_in(last), 0);
        assert!(
            last.iter().any(|m| m
                .content
                .as_deref()
                .is_some_and(|c| c.contains(IMAGE_OMITTED))),
            "no placeholder: {last:?}"
        );
    }

    // Back to one that takes images: the stored image returns.
    env.config.input_modalities = vision;
    turn(&env, "Once more.", None).await;
    let calls = calls.lock().unwrap();
    assert_eq!(images_in(calls.last().unwrap()), 1);
}

/// Read v2: an image file the model reads reaches it. A server that takes
/// images inside a tool result gets it there; any other gets it in a user
/// message right after the results. Stored with the result, it reaches the
/// next turn too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_image_reaches_the_model_inside_or_after_the_result() {
    use koda_config::InputModality;
    use koda_core::inference_helpers::TOOL_IMAGES_FOLLOW;
    for inside in [false, true] {
        let mut env = Env::new().await;
        env.config.input_modalities = Some(vec![InputModality::Text, InputModality::Image]);
        env.config.max_context_tokens = 200_000;
        env.config.model_settings.max_context_tokens = 200_000;
        if inside {
            env.config.reported_tool_images = Some((env.config.model.clone(), true));
        }
        let shot = env.root.join("shot.png");
        image_bytes(&shot);
        let provider = MockProvider::new(vec![
            MockResponse::tool_call("Read", serde_json::json!({"file_path": "shot.png"})),
            MockResponse::Text("Red.".into()),
            MockResponse::Text("Still red.".into()),
        ]);
        let calls = provider.recorded_calls_handle();
        let (mut session, _cancel) = make_session(&env, Box::new(provider)).await;
        for prompt in ["What color is shot.png?", "And now?"] {
            env.insert_user_message(prompt).await;
            let sink = TestSink::new();
            let (_, mut cmd_rx) = mpsc::channel::<EngineCommand>(1);
            session
                .run_turn(&env.config, None, &sink, &mut cmd_rx, None)
                .await
                .unwrap();
        }
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 3, "the read, its answer, the next turn");
        for call in &calls[1..] {
            let tool = call
                .iter()
                .find(|m| m.role == "tool")
                .expect("no tool result");
            assert!(
                tool.content
                    .as_deref()
                    .unwrap_or_default()
                    .starts_with("shot.png: an image, 1×1"),
                "{tool:?}"
            );
            let after = call
                .iter()
                .find(|m| m.content.as_deref() == Some(TOOL_IMAGES_FOLLOW));
            if inside {
                assert_eq!(
                    tool.images.as_ref().map(Vec::len),
                    Some(1),
                    "inside={inside}"
                );
                assert!(after.is_none());
            } else {
                assert!(tool.images.is_none());
                assert_eq!(
                    after.and_then(|m| m.images.as_ref()).map(Vec::len),
                    Some(1),
                    "inside={inside}"
                );
            }
        }
    }
}

/// A small real PNG at `path`.
fn image_bytes(path: &std::path::Path) {
    // 1×1, red.
    std::fs::write(
        path,
        b"\x89\x50\x4e\x47\x0d\x0a\x1a\x0a\x00\x00\x00\x0d\x49\x48\x44\x52\x00\x00\x00\x01\x00\x00\x00\x01\x08\x02\x00\x00\x00\x90\x77\x53\xde\x00\x00\x00\x0c\x49\x44\x41\x54\x78\x9c\x63\xb8\x23\x22\x02\x00\x02\xd4\x01\x05\x59\x70\xd3\xc2\x00\x00\x00\x00\x49\x45\x4e\x44\xae\x42\x60\x82",
    )
    .unwrap();
}
