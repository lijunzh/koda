//! Where a writing sub-agent's work ends up.
//!
//! Each test runs a real sub-agent in a real git repo. A sub-agent writes in
//! the project and, where the main agent would ask, asks in the main session;
//! only an agent that asks for one (`isolation: worktree`) gets a worktree.
//!
//! Run with:
//!   cargo test -p koda-core \
//!              --test e2e_sub_agent_workspace_test

use koda_core::approval_flow::{AgentApproval, Approver};
use koda_core::engine::ApprovalDecision;
use koda_core::{engine::EngineEvent, trust::TrustMode};
use koda_test_utils::network::FakeLlmServer;
use koda_test_utils::{ENV_MUTEX, Env, MockProvider, MockResponse};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

const WRITTEN: &str = "sub_agent_wrote_this.txt";

/// Answers every sub-agent request with `decision`, and keeps the requests.
struct Answering {
    decision: ApprovalDecision,
    asked: Mutex<Vec<AgentApproval>>,
}

impl Answering {
    fn on(env: &Env, decision: ApprovalDecision) -> Arc<Self> {
        let approver = Arc::new(Self {
            decision,
            asked: Mutex::new(Vec::new()),
        });
        env.tools.set_approver(approver.clone());
        approver
    }

    fn asked(&self) -> Vec<AgentApproval> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Approver for Answering {
    async fn approve(&self, request: AgentApproval) -> Option<ApprovalDecision> {
        self.asked.lock().unwrap().push(request);
        Some(self.decision.clone())
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q"]);
    std::fs::write(dir.join("README.md"), "hello").unwrap();
    git(dir, &["add", "README.md"]);
    git(dir, &["commit", "-qm", "init"]);
}

fn write_agent(env: &Env, server: &FakeLlmServer, isolation: Option<&str>) {
    write_agent_denying(env, server, isolation, &[]);
}

fn write_agent_denying(
    env: &Env,
    server: &FakeLlmServer,
    isolation: Option<&str>,
    denied: &[&str],
) {
    let agents_dir = env.root.join(".koda/agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    let mut agent = serde_json::json!({
        "name": "writer",
        "system_prompt": "You write files.",
        "allowed_tools": [],
        "disallowed_tools": denied,
        "trust": "safe",
        "provider": "lmstudio",
        "model": "stub",
        "base_url": server.url()
    });
    if let Some(isolation) = isolation {
        agent["isolation"] = isolation.into();
    }
    std::fs::write(agents_dir.join("writer.json"), agent.to_string()).unwrap();
}

/// Have the agent write [`WRITTEN`] and return what `InvokeAgent` told the
/// parent model.
async fn delegate_a_write(env: &Env, server: &FakeLlmServer) -> String {
    let sub_responses = serde_json::json!([
        { "tool": "Write", "args": { "file_path": WRITTEN, "content": "from the sub-agent" } },
        { "text": "Done writing." }
    ]);
    server.mount_script_json(&sub_responses.to_string()).await;
    env.insert_user_message("delegate a write").await;
    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "InvokeAgent",
            serde_json::json!({ "agent_name": "writer", "prompt": "write the file" }),
        ),
        MockResponse::Text("All done.".into()),
    ]);
    let events = env.run_inference(&provider).await;
    events
        .iter()
        .find_map(|e| match e {
            EngineEvent::ToolCallResult { name, output, .. } if name == "InvokeAgent" => {
                Some(output.clone())
            }
            _ => None,
        })
        .expect("InvokeAgent must produce a result")
}

fn branches(env: &Env) -> Vec<String> {
    git(&env.root, &["branch", "--list", "koda/wt/*"])
        .lines()
        .map(|b| b.trim().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_auto_the_edit_lands_in_the_project() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::builder().trust(TrustMode::Auto).build().await;
    let server = FakeLlmServer::spawn().await;
    init_repo(&env.root);
    write_agent(&env, &server, None);

    let told = delegate_a_write(&env, &server).await;

    let content =
        std::fs::read_to_string(env.root.join(WRITTEN)).expect("the edit must reach the project");
    assert_eq!(content, "from the sub-agent");
    assert!(branches(&env).is_empty(), "no branch in Auto");
    assert!(told.contains("Done writing."), "{told}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_can_ask_for_a_worktree_in_auto() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::builder().trust(TrustMode::Auto).build().await;
    let server = FakeLlmServer::spawn().await;
    init_repo(&env.root);
    write_agent(&env, &server, Some("worktree"));

    let told = delegate_a_write(&env, &server).await;

    assert!(!env.root.join(WRITTEN).exists());
    let branches = branches(&env);
    assert_eq!(branches.len(), 1, "{branches:?}");
    assert!(told.contains(&branches[0]), "parent must hear: {told}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_requested_worktree_outside_git_stops_the_agent_in_auto_too() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::builder().trust(TrustMode::Auto).build().await;
    let server = FakeLlmServer::spawn().await;
    write_agent(&env, &server, Some("worktree"));

    let told = delegate_a_write(&env, &server).await;

    assert!(!env.root.join(WRITTEN).exists());
    assert!(told.contains("not a git repository"), "{told}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_auto_outside_git_the_edit_still_lands_in_the_project() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::builder().trust(TrustMode::Auto).build().await;
    let server = FakeLlmServer::spawn().await;
    write_agent(&env, &server, None);

    delegate_a_write(&env, &server).await;

    assert!(env.root.join(WRITTEN).exists());
}

/// In Safe a sub-agent asks for its edit in the main session, as the main
/// agent does (Claude Code), and an approved edit lands in the project.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_safe_the_sub_agent_asks_and_an_approved_edit_lands_in_the_project() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::builder().trust(TrustMode::Safe).build().await;
    let server = FakeLlmServer::spawn().await;
    init_repo(&env.root);
    write_agent(&env, &server, None);
    let approver = Answering::on(&env, ApprovalDecision::Approve);

    let told = delegate_a_write(&env, &server).await;

    let content = std::fs::read_to_string(env.root.join(WRITTEN))
        .expect("the approved edit must reach the project");
    assert_eq!(content, "from the sub-agent");
    assert!(
        branches(&env).is_empty(),
        "no worktree unless the agent asks"
    );
    let asked = approver.asked();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert_eq!(asked[0].agent_name, "writer");
    assert_eq!(asked[0].tool_name, "Write");
    assert_eq!(asked[0].agent_id, None, "a blocking sub-agent has no id");
    assert!(told.contains("Done writing."), "{told}");
}

/// A refused edit isn't made, and the sub-agent hears the person said no.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_safe_a_refused_edit_leaves_the_project_as_it_was() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::builder().trust(TrustMode::Safe).build().await;
    let server = FakeLlmServer::spawn().await;
    init_repo(&env.root);
    write_agent(&env, &server, None);
    Answering::on(&env, ApprovalDecision::Reject);

    delegate_a_write(&env, &server).await;

    assert!(!env.root.join(WRITTEN).exists());
    let calls = server.chat_messages().await;
    let second = serde_json::to_string(&calls[1]).unwrap();
    assert!(second.contains("User rejected this action."), "{second}");
}

/// With nobody to ask, the edit is refused and the sub-agent is told why.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_safe_with_nobody_to_ask_the_edit_is_refused() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::builder().trust(TrustMode::Safe).build().await;
    let server = FakeLlmServer::spawn().await;
    init_repo(&env.root);
    write_agent(&env, &server, None);

    delegate_a_write(&env, &server).await;

    assert!(!env.root.join(WRITTEN).exists());
    let calls = server.chat_messages().await;
    let second = serde_json::to_string(&calls[1]).unwrap();
    assert!(second.contains("nobody could be asked"), "{second}");
}

/// "Allow for session" given to a sub-agent covers the main agent's same
/// call, and the reverse: one set for the session, as in Claude Code.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sub_agents_allow_for_session_is_the_sessions() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::builder().trust(TrustMode::Safe).build().await;
    let server = FakeLlmServer::spawn().await;
    init_repo(&env.root);
    write_agent(&env, &server, None);
    let approver = Answering::on(&env, ApprovalDecision::ApproveForSession);
    // Two sub-agents, one after the other, each writing the same file.
    let write =
        serde_json::json!({ "tool": "Write", "args": { "file_path": WRITTEN, "content": "x" } });
    let script = serde_json::json!([write, { "text": "one" }, write, { "text": "two" }]);
    server.mount_script_json(&script.to_string()).await;
    env.insert_user_message("delegate twice").await;
    let invoke = |prompt: &str| {
        MockResponse::tool_call(
            "InvokeAgent",
            serde_json::json!({ "agent_name": "writer", "prompt": prompt }),
        )
    };
    let provider = MockProvider::new(vec![
        invoke("write it"),
        invoke("write it again"),
        MockResponse::Text("All done.".into()),
    ]);
    env.run_inference(&provider).await;

    assert!(env.root.join(WRITTEN).exists());
    // The second agent's write wasn't asked about.
    assert_eq!(approver.asked().len(), 1);
}
