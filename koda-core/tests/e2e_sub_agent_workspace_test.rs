//! Where a writing sub-agent's work ends up.
//!
//! Each test runs a real sub-agent in a real git repo. On macOS its writes used
//! to go to a copy of the project that was deleted when it finished, so what
//! matters here is that the work is still somewhere afterwards.
//!
//! Run with:
//!   cargo test -p koda-core --features test-support \
//!              --test e2e_sub_agent_workspace_test

use koda_core::{engine::EngineEvent, runtime_env, trust::TrustMode};
use koda_test_utils::{ENV_MUTEX, Env, MockProvider, MockResponse};
use std::path::Path;
use std::process::Command;

const WRITTEN: &str = "sub_agent_wrote_this.txt";

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

fn write_agent(env: &Env, isolation: Option<&str>) {
    write_agent_denying(env, isolation, &[]);
}

fn write_agent_denying(env: &Env, isolation: Option<&str>, denied: &[&str]) {
    let agents_dir = env.root.join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    let mut agent = serde_json::json!({
        "name": "writer",
        "system_prompt": "You write files.",
        "allowed_tools": [],
        "disallowed_tools": denied,
        "trust": "safe",
        "provider": "mock",
        "base_url": "http://localhost:0"
    });
    if let Some(isolation) = isolation {
        agent["isolation"] = isolation.into();
    }
    std::fs::write(agents_dir.join("writer.json"), agent.to_string()).unwrap();
}

/// Have the agent write [`WRITTEN`] and return what `InvokeAgent` told the
/// parent model.
async fn delegate_a_write(env: &Env) -> String {
    let sub_responses = serde_json::json!([
        { "tool": "Write", "args": { "file_path": WRITTEN, "content": "from the sub-agent" } },
        { "text": "Done writing." }
    ]);
    runtime_env::set("KODA_MOCK_RESPONSES", sub_responses.to_string());
    env.insert_user_message("delegate a write").await;
    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "InvokeAgent",
            serde_json::json!({ "agent_name": "writer", "prompt": "write the file" }),
        ),
        MockResponse::Text("All done.".into()),
    ]);
    let events = env.run_inference(&provider).await;
    runtime_env::remove("KODA_MOCK_RESPONSES");
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

#[cfg(feature = "test-support")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_auto_the_edit_lands_in_the_project_and_undo_takes_it_back() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::builder().trust(TrustMode::Auto).build().await;
    init_repo(&env.root);
    write_agent(&env, None);

    let told = delegate_a_write(&env).await;

    let written = env.root.join(WRITTEN);
    let content = std::fs::read_to_string(&written).expect("the edit must reach the project");
    assert_eq!(content, "from the sub-agent");
    assert!(branches(&env).is_empty(), "no branch in Auto");
    assert!(told.contains("Done writing."), "{told}");

    env.tools.undo.lock().unwrap().undo().expect("undo entry");
    assert!(!written.exists(), "undo must cover a sub-agent's edit");
}

#[cfg(feature = "test-support")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_safe_the_work_waits_on_a_branch_the_parent_hears_about() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::builder().trust(TrustMode::Safe).build().await;
    init_repo(&env.root);
    write_agent(&env, None);

    let told = delegate_a_write(&env).await;

    assert!(
        !env.root.join(WRITTEN).exists(),
        "Safe: nothing reaches the project without the user's OK"
    );
    let branches = branches(&env);
    assert_eq!(branches.len(), 1, "{branches:?}");
    let on_branch = git(&env.root, &["show", &format!("{}:{WRITTEN}", branches[0])]);
    assert_eq!(on_branch, "from the sub-agent");
    assert!(told.contains(&branches[0]), "parent must hear: {told}");
}

#[cfg(feature = "test-support")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_can_ask_for_a_worktree_in_auto() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::builder().trust(TrustMode::Auto).build().await;
    init_repo(&env.root);
    write_agent(&env, Some("worktree"));

    let told = delegate_a_write(&env).await;

    assert!(!env.root.join(WRITTEN).exists());
    let branches = branches(&env);
    assert_eq!(branches.len(), 1, "{branches:?}");
    assert!(told.contains(&branches[0]), "parent must hear: {told}");
}

#[cfg(feature = "test-support")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_safe_outside_git_the_writer_does_not_start() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::builder().trust(TrustMode::Safe).build().await;
    write_agent(&env, None);

    let told = delegate_a_write(&env).await;

    assert!(!env.root.join(WRITTEN).exists(), "Safe's promise broke");
    assert!(told.contains("not a git repository"), "{told}");
}

#[cfg(feature = "test-support")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_requested_worktree_outside_git_stops_the_agent_in_auto_too() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::builder().trust(TrustMode::Auto).build().await;
    write_agent(&env, Some("worktree"));

    let told = delegate_a_write(&env).await;

    assert!(!env.root.join(WRITTEN).exists());
    assert!(told.contains("not a git repository"), "{told}");
}

#[cfg(feature = "test-support")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_auto_outside_git_the_edit_still_lands_in_the_project() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::builder().trust(TrustMode::Auto).build().await;
    write_agent(&env, None);

    delegate_a_write(&env).await;

    assert!(env.root.join(WRITTEN).exists());
}

#[cfg(feature = "test-support")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_denied_file_tool_does_not_hide_a_writer() {
    let _lock = ENV_MUTEX.lock().await;
    for (mode, isolation) in [(TrustMode::Safe, None), (TrustMode::Auto, Some("worktree"))] {
        let env = Env::builder().trust(mode).build().await;
        init_repo(&env.root);
        write_agent_denying(&env, isolation, &["Edit"]);

        let told = delegate_a_write(&env).await;

        assert!(!env.root.join(WRITTEN).exists(), "{mode:?}: {told}");
        assert_eq!(branches(&env).len(), 1, "{mode:?}: {told}");
    }
}

#[cfg(feature = "test-support")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edit_only_agent_gets_a_worktree_in_safe() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::builder().trust(TrustMode::Safe).build().await;
    init_repo(&env.root);
    write_agent_denying(&env, None, &["Write"]);
    let sub_responses = serde_json::json!([
        {
            "tool": "Edit",
            "args": { "file_path": "README.md", "old_str": "hello", "new_str": "edited" }
        },
        { "text": "Done editing." }
    ]);
    runtime_env::set("KODA_MOCK_RESPONSES", sub_responses.to_string());
    env.insert_user_message("delegate an edit").await;
    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "InvokeAgent",
            serde_json::json!({ "agent_name": "writer", "prompt": "edit the file" }),
        ),
        MockResponse::Text("All done.".into()),
    ]);
    env.run_inference(&provider).await;
    runtime_env::remove("KODA_MOCK_RESPONSES");

    let readme = std::fs::read_to_string(env.root.join("README.md")).unwrap();
    assert_eq!(readme, "hello", "the project must stay as it was");
    assert_eq!(branches(&env).len(), 1);
}
