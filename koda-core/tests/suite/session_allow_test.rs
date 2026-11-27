//! A yes "for this session" (`s` at the prompt) to a command leaving the
//! sandbox covers later commands starting with its `prefix_rule`, as a Codex
//! exec-policy prefix rule does: not another prefix, not a dangerous command
//! under the same prefix, and not a new session. Destructive calls keep
//! asking.

use koda_core::engine::{ApprovalDecision, EngineCommand, EngineEvent, EngineSink};
use koda_core::providers::LlmProvider;
use koda_core::session::KodaSession;
use koda_core::tools::{ToolCatalog, ToolRegistry};
use koda_core::trust::TrustMode;
use koda_test_utils::Env;
use koda_test_utils::TestSink;
use koda_test_utils::{MockProvider, MockResponse};
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
        prompt_model: String::new(),
        semantic_memory: String::new(),
        interactive: false,
    });
    let mut session = KodaSession::new(
        env.session_id.clone(),
        agent,
        env.db.clone(),
        &env.config,
        TrustMode::Auto,
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

fn bash(command: &str) -> MockResponse {
    MockResponse::tool_call("Bash", json!({"command": command}))
}

/// `command` asking to leave the sandbox, offering `prefix_rule` for the
/// session (none: no "this session" option).
fn outside(command: &str, prefix_rule: &[&str]) -> MockResponse {
    let mut args = json!({"command": command, "outside_sandbox": true, "reason": "test"});
    if !prefix_rule.is_empty() {
        args["prefix_rule"] = json!(prefix_rule);
    }
    MockResponse::tool_call("Bash", args)
}

fn done() -> MockResponse {
    MockResponse::Text("done".into())
}

fn git(env: &Env, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(&env.root)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A repo the commands can commit to outside the sandbox.
fn init_repo(env: &Env) {
    git(env, &["init", "-q"]);
    git(env, &["config", "user.name", "t"]);
    git(env, &["config", "user.email", "t@t"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_yes_covers_commands_with_the_same_prefix_only() {
    let env = Env::new().await;
    init_repo(&env);
    let mut session = session(
        &env,
        vec![
            outside("git commit --allow-empty -m one", &["git", "commit"]),
            // The same prefix: covered, whatever follows it.
            outside("git commit --allow-empty -m x", &["git", "commit"]),
            // Another prefix asks.
            outside("git push origin main", &["git", "push"]),
            // A prefix too broad to allow offers no "this session".
            outside("git status", &["git"]),
            // No prefix offered: no "this session" either.
            outside("git log", &[]),
            done(),
        ],
    )
    .await;
    use ApprovalDecision::{ApproveForSession, Reject};
    let prompts = run(
        &env,
        &mut session,
        vec![ApproveForSession, Reject, Reject, Reject],
    )
    .await;

    let shown: Vec<(&str, bool)> = prompts
        .iter()
        .map(|(tool, _, offered)| (tool.as_str(), *offered))
        .collect();
    assert_eq!(
        shown,
        [
            ("Bash", true),
            ("Bash", true),
            ("Bash", false),
            ("Bash", false)
        ]
    );
    assert!(prompts[1].1.contains("git push"), "{prompts:?}");
    assert_eq!(
        git(&env, &["rev-list", "--count", "HEAD"]),
        "2",
        "both commits ran outside the sandbox"
    );

    // A new session starts with nothing allowed.
    let mut next = self::session(
        &env,
        vec![
            outside("git commit --allow-empty -m y", &["git", "commit"]),
            done(),
        ],
    )
    .await;
    let prompts = run(&env, &mut next, vec![Reject]).await;
    assert_eq!(prompts.len(), 1, "a new session should ask again");
}

/// An allowed prefix never covers a command dangerous in itself:
/// `git push --force` under an allowed `git push` asks again, whether or not
/// the model offers the same prefix for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_allowed_prefix_does_not_cover_a_destructive_command() {
    let env = Env::new().await;
    init_repo(&env);
    let mut session = session(
        &env,
        vec![
            outside("git push origin main", &["git", "push"]),
            outside("git push --force origin main", &[]),
            outside("git push --force origin main", &["git", "push"]),
            done(),
        ],
    )
    .await;
    // The first push fails, as the repo has no remote; the others never run.
    use ApprovalDecision::{ApproveForSession, Reject};
    let prompts = run(&env, &mut session, vec![ApproveForSession, Reject, Reject]).await;
    assert_eq!(prompts.len(), 3, "{prompts:?}");
    assert!(prompts[1].1.contains("--force"), "{prompts:?}");
    assert!(prompts[2].1.contains("--force"), "{prompts:?}");
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
