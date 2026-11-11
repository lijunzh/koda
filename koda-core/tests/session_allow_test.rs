//! A yes "for this session" (`s` at the prompt) stops the prompt for the same
//! command or file, and only for local changes: destructive calls keep asking,
//! and a new session starts with nothing allowed.

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

/// What a prompt showed: the tool, its detail, and whether `s` was offered.
type Prompt = (String, String, bool);

/// A user who answers each prompt with the next scripted decision.
struct User {
    events: TestSink,
    cmd_tx: mpsc::Sender<EngineCommand>,
    answers: Mutex<VecDeque<ApprovalDecision>>,
    prompts: Mutex<Vec<Prompt>>,
}

impl EngineSink for User {
    fn emit(&self, event: EngineEvent) {
        if let EngineEvent::ApprovalRequest {
            id,
            tool_name,
            detail,
            allow_for_session,
            ..
        } = &event
        {
            self.prompts.lock().unwrap().push((
                tool_name.clone(),
                detail.clone(),
                *allow_for_session,
            ));
            let decision = self.answers.lock().unwrap().pop_front().unwrap_or_else(|| {
                panic!("unexpected prompt: {tool_name} {detail} {allow_for_session}")
            });
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

async fn session(env: &Env, responses: Vec<MockResponse>) -> KodaSession {
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
        TrustMode::Safe,
    )
    .await;
    session.provider = Box::new(MockProvider::new(responses)) as Box<dyn LlmProvider>;
    session
}

/// Run one turn in `session`, answering prompts from `answers`, and return
/// the prompts it showed.
async fn run(env: &Env, session: &mut KodaSession, answers: Vec<ApprovalDecision>) -> Vec<Prompt> {
    env.insert_user_message("go").await;
    let (cmd_tx, mut cmd_rx) = mpsc::channel(8);
    let user = User {
        events: TestSink::new(),
        cmd_tx,
        answers: Mutex::new(answers.into()),
        prompts: Mutex::new(Vec::new()),
    };
    tokio::time::timeout(
        WAIT,
        session.run_turn(&env.config, None, &user, &mut cmd_rx, None),
    )
    .await
    .expect("the turn did not end")
    .unwrap();
    let prompts = user.prompts.into_inner().unwrap();
    assert!(
        user.answers.lock().unwrap().is_empty(),
        "the turn asked fewer times than scripted: {prompts:?}"
    );
    prompts
}

fn write(path: &str, content: &str) -> MockResponse {
    MockResponse::tool_call("Write", json!({"file_path": path, "content": content}))
}

fn bash(command: &str) -> MockResponse {
    MockResponse::tool_call("Bash", json!({"command": command}))
}

fn done() -> MockResponse {
    MockResponse::Text("done".into())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_yes_covers_the_same_file_and_command_only() {
    let env = Env::new().await;
    let mut session = session(
        &env,
        vec![
            write("a.txt", "one"),
            // The same file again: allowed, whatever the arguments.
            MockResponse::tool_call(
                "Write",
                json!({"file_path": "a.txt", "content": "two", "overwrite": true}),
            ),
            // Another file asks.
            write("b.txt", "one"),
            bash("touch c.txt"),
            bash("touch c.txt"),
            // A different command asks.
            bash("touch d.txt"),
            done(),
        ],
    )
    .await;
    use ApprovalDecision::{Approve, ApproveForSession};
    let prompts = run(
        &env,
        &mut session,
        vec![ApproveForSession, Approve, ApproveForSession, Approve],
    )
    .await;

    let shown: Vec<(&str, bool)> = prompts
        .iter()
        .map(|(tool, _, offered)| (tool.as_str(), *offered))
        .collect();
    assert_eq!(
        shown,
        [
            ("Write", true),
            ("Write", true),
            ("Bash", true),
            ("Bash", true)
        ]
    );
    assert!(prompts[1].1.contains("b.txt"), "{prompts:?}");
    assert!(prompts[3].1.contains("d.txt"), "{prompts:?}");
    assert_eq!(
        std::fs::read_to_string(env.root.join("a.txt")).unwrap(),
        "two"
    );
    assert!(env.root.join("c.txt").exists());

    // A new session starts with nothing allowed.
    let mut next = self::session(&env, vec![bash("touch c.txt"), done()]).await;
    let prompts = run(&env, &mut next, vec![Approve]).await;
    assert_eq!(prompts.len(), 1, "a new session should ask again");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_destructive_call_is_never_allowed_for_the_session() {
    let env = Env::new().await;
    std::fs::create_dir_all(env.root.join("build")).unwrap();
    let mut session = session(
        &env,
        vec![bash("rm -rf build"), bash("rm -rf build"), done()],
    )
    .await;
    // A client that answers "for this session" anyway gets a one-time yes.
    let prompts = run(
        &env,
        &mut session,
        vec![
            ApprovalDecision::ApproveForSession,
            ApprovalDecision::Approve,
        ],
    )
    .await;
    assert_eq!(prompts.len(), 2, "{prompts:?}");
    assert!(
        prompts.iter().all(|(_, _, offered)| !offered),
        "{prompts:?}"
    );
}
