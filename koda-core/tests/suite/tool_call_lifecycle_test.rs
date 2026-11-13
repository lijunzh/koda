//! Every tool call that starts ends: each `ToolCallStart` gets one
//! `ToolCallResult` saying how it ended, whether the call ran, was refused at
//! an approval, was blocked by plan mode, or was a sub-agent's. Requests about
//! a call name it.

use koda_core::engine::event::ToolOutcome;
use koda_core::engine::sink::TestSink;
use koda_core::engine::{ApprovalDecision, EngineCommand, EngineEvent, EngineSink};
use koda_core::providers::LlmProvider;
use koda_core::providers::mock::{MockProvider, MockResponse};
use koda_core::session::KodaSession;
use koda_core::tools::{ToolCatalog, ToolRegistry};
use koda_core::trust::TrustMode;
use koda_test_utils::Env;
use serde_json::json;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(20);

/// A user who answers each approval with the next scripted decision.
struct User {
    events: TestSink,
    cmd_tx: mpsc::Sender<EngineCommand>,
    answers: Mutex<VecDeque<ApprovalDecision>>,
}

impl EngineSink for User {
    fn emit(&self, event: EngineEvent) {
        if let EngineEvent::ApprovalRequest { id, .. } = &event {
            let decision = self
                .answers
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected prompt");
            self.cmd_tx
                .try_send(EngineCommand::ApprovalResponse {
                    id: id.clone(),
                    decision,
                })
                .unwrap();
        }
        self.events.emit(event);
    }
}

/// Run one turn at `trust`, answering approvals from `answers`.
async fn run(
    trust: TrustMode,
    responses: Vec<MockResponse>,
    answers: Vec<ApprovalDecision>,
) -> Vec<EngineEvent> {
    let env = Env::new().await;
    let agent = Arc::new(koda_core::agent::KodaAgent {
        project_root: env.root.clone(),
        tools: ToolRegistry::new(env.root.clone(), env.config.max_context_tokens),
        tool_defs: ToolCatalog::new().get_definitions(&[], &[]),
        system_prompt: "You are a test assistant.".to_string(),
        semantic_memory: String::new(),
    });
    let mut session = KodaSession::new(
        env.session_id.clone(),
        agent,
        env.db.clone(),
        &env.config,
        trust,
    )
    .await;
    session.provider = Box::new(MockProvider::new(responses)) as Box<dyn LlmProvider>;
    env.insert_user_message("go").await;
    let (cmd_tx, mut cmd_rx) = mpsc::channel(8);
    let user = User {
        events: TestSink::new(),
        cmd_tx,
        answers: Mutex::new(answers.into()),
    };
    tokio::time::timeout(
        WAIT,
        session.run_turn(&env.config, None, &user, &mut cmd_rx, None),
    )
    .await
    .expect("the turn did not end")
    .unwrap();
    assert!(
        user.answers.lock().unwrap().is_empty(),
        "fewer prompts than scripted"
    );
    user.events.events()
}

/// A result: the call's id, how it ended, and what the model read.
type Ended = (String, ToolOutcome, String);

/// `(id, name)` of every started call, and every result, in order.
fn calls(events: &[EngineEvent]) -> (Vec<(String, String)>, Vec<Ended>) {
    let mut started = Vec::new();
    let mut ended = Vec::new();
    for event in events {
        match event {
            EngineEvent::ToolCallStart { id, name, .. } => started.push((id.clone(), name.clone())),
            EngineEvent::ToolCallResult {
                id,
                outcome,
                output,
                ..
            } => ended.push((id.clone(), *outcome, output.clone())),
            _ => {}
        }
    }
    (started, ended)
}

/// Each started call has exactly one result. Returns the result for `name`.
fn ended_once(events: &[EngineEvent], name: &str) -> Ended {
    let (started, ended) = calls(events);
    for (id, tool) in &started {
        let results = ended.iter().filter(|(r, ..)| r == id).count();
        assert_eq!(
            results, 1,
            "{tool} call {id} has {results} results: {events:#?}"
        );
    }
    let id = &started.iter().find(|(_, tool)| tool == name).expect(name).0;
    ended.into_iter().find(|(r, ..)| r == id).unwrap()
}

fn write(path: &str) -> MockResponse {
    MockResponse::tool_call("Write", json!({"file_path": path, "content": "x"}))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_call_ends_declined_and_its_approval_names_it() {
    let events = run(
        TrustMode::Safe,
        vec![
            write("a.txt"),
            MockResponse::tool_call("Bash", json!({"command": "touch b.txt"})),
            MockResponse::Text("done".into()),
        ],
        vec![ApprovalDecision::Reject, ApprovalDecision::Approve],
    )
    .await;

    let (write_id, outcome, output) = ended_once(&events, "Write");
    assert_eq!(outcome, ToolOutcome::Declined);
    assert_eq!(output, "User rejected this action.");
    let (_, outcome, _) = ended_once(&events, "Bash");
    assert_eq!(outcome, ToolOutcome::Success);

    let asked: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::ApprovalRequest { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(asked.len(), 2);
    assert_eq!(asked[0], write_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_plan_mode_blocks_ends_declined_and_the_block_names_it() {
    let events = run(
        TrustMode::Plan,
        vec![write("a.txt"), MockResponse::Text("done".into())],
        vec![],
    )
    .await;

    let (write_id, outcome, output) = ended_once(&events, "Write");
    assert_eq!(outcome, ToolOutcome::Declined);
    assert_eq!(output, koda_core::trust::PLAN_BLOCKED_MESSAGE);
    let blocked = events.iter().find_map(|e| match e {
        EngineEvent::ActionBlocked { tool_call_id, .. } => Some(tool_call_id.clone()),
        _ => None,
    });
    assert_eq!(blocked, Some(write_id));
}

#[cfg(feature = "test-support")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sub_agent_s_calls_end_too() {
    use koda_core::runtime_env;
    use koda_test_utils::ENV_MUTEX;
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::builder().trust(TrustMode::Auto).build().await;
    let agents = env.root.join("agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join("helper.json"),
        json!({
            "name": "helper",
            "system_prompt": "You help.",
            "allowed_tools": [],
            "trust": "safe",
            "provider": "mock",
            "base_url": "http://localhost:0"
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(env.root.join("notes.txt"), "a note\n").unwrap();
    let notes = env.root.join("notes.txt").to_string_lossy().to_string();

    // The sub-agent reads a file, then tries a destructive command, which a
    // sub-agent never runs.
    runtime_env::set(
        "KODA_MOCK_RESPONSES",
        json!([
            { "tool": "Read", "args": { "file_path": notes } },
            { "tool": "Bash", "args": { "command": "rm -rf build" } },
            { "text": "Read it." }
        ])
        .to_string(),
    );
    env.insert_user_message("ask the helper").await;
    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "InvokeAgent",
            json!({"agent_name": "helper", "prompt": "read notes", "background": false}),
        ),
        MockResponse::Text("done".into()),
    ]);
    let events = env.run_inference(&provider).await;
    runtime_env::remove("KODA_MOCK_RESPONSES");

    let (_, outcome, output) = ended_once(&events, "Read");
    assert_eq!(outcome, ToolOutcome::Success, "{output}");
    let (_, outcome, _) = ended_once(&events, "Bash");
    assert_eq!(outcome, ToolOutcome::Declined);
    let (_, outcome, _) = ended_once(&events, "InvokeAgent");
    assert_eq!(outcome, ToolOutcome::Success);
}
