//! The iteration cap is a request with an id: only its own answer resumes
//! the turn, so a stale decision can't unblock a later cap.

use koda_core::engine::{EngineCommand, EngineEvent};
use koda_core::inference::{self, InferenceContext};
use koda_core::loop_guard::LoopContinuation;
use koda_core::providers::mock::{MockProvider, MockResponse};
use koda_test_utils::{Env, TestSink};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_matching_decision_resumes_the_turn() {
    let mut env = Env::new().await;
    env.config.max_iterations = 1;
    env.insert_message(&koda_core::db::Role::User, "go").await;

    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::tool_call("List", serde_json::json!({})),
        MockResponse::Text("finished".into()),
    ]));
    let sink = Arc::new(TestSink::new());
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<EngineCommand>(4);

    let tool_defs = env.tool_defs();
    let mut file_tracker =
        koda_core::file_tracker::FileTracker::new(&env.session_id, env.db.clone()).await;
    let cache = koda_core::sub_agent_cache::SubAgentCache::new();
    let git = koda_core::git::GitSnapshot::default();
    let mut steers = Vec::new();
    let turn = inference::inference_loop(InferenceContext {
        project_root: &env.root,
        config: &env.config,
        db: &env.db,
        session_id: &env.session_id,
        system_prompt: "You are a test assistant.",
        git_snapshot: &git,
        provider: provider.as_ref(),
        tools: &env.tools,
        tool_defs: &tool_defs,
        pending_images: None,
        mode: env.trust,
        sink: sink.as_ref(),
        cancel: CancellationToken::new(),
        cmd_rx: &mut cmd_rx,
        queued_steers: &mut steers,
        file_tracker: &mut file_tracker,
        bg_agents: &env.bg_agents,
        sub_agent_cache: &cache,
    });
    tokio::pin!(turn);

    let id = tokio::select! {
        id = sink.wait_for_map(Duration::from_secs(10), |e| match e {
            EngineEvent::LoopCapReached { id, .. } => Some(id.clone()),
            _ => None,
        }) => id.expect("no loop cap"),
        result = &mut turn => panic!("turn ended first: {result:?}"),
    };

    cmd_tx
        .send(EngineCommand::LoopDecision {
            id: "stale".into(),
            action: LoopContinuation::Continue50,
        })
        .await
        .unwrap();
    let waited = tokio::time::timeout(Duration::from_millis(200), &mut turn).await;
    assert!(waited.is_err(), "a stale decision resumed the turn");
    assert_eq!(provider.recorded_calls().len(), 1);

    cmd_tx
        .send(EngineCommand::LoopDecision {
            id,
            action: LoopContinuation::Continue50,
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), &mut turn)
        .await
        .expect("the matching decision did not resume the turn")
        .unwrap();
    assert_eq!(provider.recorded_calls().len(), 2);
}
