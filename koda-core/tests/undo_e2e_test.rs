//! E2E tests for the `/undo` system through the full inference loop.
//!
//! Closes priority 3 of #1264 ("Undo System — High: No E2E test for the
//! undo stack"). Unit tests in `src/undo.rs` cover the [`UndoStack`]
//! data structure in isolation; this file exercises the production
//! pipeline end-to-end:
//!
//! ```text
//! MockProvider → inference_loop → tool dispatch → undo.snapshot()
//!                                              ↓
//!                                       session.run_turn()
//!                                              ↓
//!                                       undo.commit_turn()  ← #1264 fix
//! ```
//!
//! ## What these tests would have caught (and did)
//!
//! Before the #1264 fix, `commit_turn()` was *never called in production
//! code* — only from `undo.rs`'s own unit tests. That meant `pending`
//! accumulated forever, `entries` stayed empty, and the `/undo` slash
//! command always reported "Nothing to undo" no matter how many files
//! had been written. The very first test in this file
//! (`undo_restores_overwritten_file_through_inference_loop`) reproduces
//! that bug deterministically.
//!
//! ## Why E2E and not just more unit tests
//!
//! The unit tests in `src/undo.rs` exercise `UndoStack` correctly — and
//! the bug above proves that's not enough. The contract this file
//! protects is *"file mutations made through the inference loop are
//! recoverable via `/undo`"*, which spans three modules
//! (`tools/mod.rs`, `session.rs`, `undo.rs`) and is exactly the kind of
//! seam that unit tests can't see.

use koda_core::providers::{ToolCall, mock::MockResponse};
use koda_test_utils::{Env, MockProvider};

// ── Helpers ──────────────────────────────────────────────────

/// Build a `Write` tool-call with a unique id (parallel calls in one
/// `MockResponse::ToolCalls` need distinct ids). Sets `overwrite: true`
/// because the Write tool refuses to clobber an existing file by
/// default — a separate safety guard, and not what these undo tests
/// are exercising.
fn write_call(id: &str, file_path: &str, content: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        function_name: "Write".into(),
        arguments: serde_json::json!({
            "file_path": file_path,
            "content": content,
            "overwrite": true,
        })
        .to_string(),
        thought_signature: None,
    }
}

/// Snapshot the current undo-stack depth. Uses `expect` so a poisoned
/// mutex (which only happens if a panicking test left the lock held)
/// fails the assertion with a clear message rather than a `Result`
/// chain that drowns the real failure.
fn undo_depth(env: &Env) -> usize {
    env.tools
        .undo
        .lock()
        .expect("undo mutex poisoned — a previous test panicked while holding it")
        .depth()
}

/// Pop the most recent undo entry and return the human-readable summary.
/// Panics if the stack is empty (use `undo_depth` first if that's a
/// possibility you need to assert against).
fn undo_one(env: &Env) -> String {
    env.tools
        .undo
        .lock()
        .expect("undo mutex poisoned")
        .undo()
        .expect("undo stack should have at least one entry")
}

// ── Tests ────────────────────────────────────────────────────

/// Baseline: a single Write through the inference loop is snapshotted
/// AND committed, so /undo restores the original content.
///
/// This test failed before the #1264 fix because `commit_turn()` was
/// never called — `depth()` stayed at 0 even after a successful Write.
#[tokio::test]
async fn undo_restores_overwritten_file_through_inference_loop() {
    let env = Env::new().await;
    let path = env.root.join("greeting.txt");
    std::fs::write(&path, "hello, world").unwrap();

    env.insert_user_message("rewrite greeting").await;
    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "Write",
            serde_json::json!({
                "file_path": "greeting.txt",
                "content": "GOODBYE, WORLD",
                "overwrite": true,
            }),
        ),
        MockResponse::Text("Done.".into()),
    ]);
    env.run_inference(&provider).await;

    // Sanity: the Write actually happened.
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "GOODBYE, WORLD");
    // The fix: turn-end committed the snapshot into an undoable entry.
    assert_eq!(
        undo_depth(&env),
        1,
        "exactly one turn happened; expected one undo entry"
    );

    // The point: undo restores the original.
    let summary = undo_one(&env);
    assert!(
        summary.contains("restored"),
        "expected 'restored' in summary, got: {summary}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello, world");
}

/// A Write that *creates* a brand-new file should be undoable by
/// *removing* the file (not by writing empty content).
#[tokio::test]
async fn undo_removes_newly_created_file_through_inference_loop() {
    let env = Env::new().await;
    let path = env.root.join("new_file.txt");
    assert!(!path.exists(), "precondition: file must not exist");

    env.insert_user_message("create new_file.txt").await;
    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "Write",
            serde_json::json!({
                "file_path": "new_file.txt",
                "content": "fresh content",
            }),
        ),
        MockResponse::Text("Created.".into()),
    ]);
    env.run_inference(&provider).await;

    assert!(path.exists(), "Write should have created the file");

    let summary = undo_one(&env);
    assert!(
        summary.contains("removed") || summary.contains("newly created"),
        "summary should mention removal of newly-created file, got: {summary}"
    );
    assert!(
        !path.exists(),
        "undo should have removed the newly-created file"
    );
}

/// Edit on an existing file: snapshot captures pre-edit content, undo
/// restores it byte-for-byte.
#[tokio::test]
async fn undo_after_edit_restores_original_through_inference_loop() {
    let env = Env::new().await;
    let path = env.root.join("config.toml");
    let original = "name = \"alpha\"\nversion = \"1.0\"\n";
    std::fs::write(&path, original).unwrap();

    env.insert_user_message("rename to beta").await;
    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "Edit",
            serde_json::json!({
                "file_path": "config.toml",
                "replacements": [
                    {"old_str": "alpha", "new_str": "beta"}
                ],
            }),
        ),
        MockResponse::Text("Renamed.".into()),
    ]);
    env.run_inference(&provider).await;

    // Sanity: edit landed.
    assert!(std::fs::read_to_string(&path).unwrap().contains("beta"));

    undo_one(&env);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        original,
        "undo must restore the exact pre-edit content (whitespace + all)"
    );
}

/// Delete: snapshot captures the file contents, undo recreates the file
/// with those contents.
///
/// Note: Delete on a *non-Koda-owned* file emits `ApprovalRequest`
/// (destructive op) which `Env::run_inference` has no responder wired
/// up for, so the Delete would silently never execute. We sidestep
/// that by creating the file *through Koda* in a setup turn (Write
/// tool) so Koda owns it; per `trust.rs` and #465, deletion of
/// Koda-owned files auto-approves. This is the same pattern the
/// existing `test_delete_tool_standalone_e2e` uses.
#[tokio::test]
async fn undo_after_delete_restores_file_through_inference_loop() {
    let env = Env::new().await;
    let path = env.root.join("doomed.txt");
    let content = "important data\nline 2\n";

    // ── Setup turn: create via Write so Koda owns the file ──
    env.insert_user_message("create doomed.txt").await;
    let setup = MockProvider::new(vec![
        MockResponse::tool_call(
            "Write",
            serde_json::json!({
                "path": path.to_string_lossy(),
                "content": content,
            }),
        ),
        MockResponse::Text("Created.".into()),
    ]);
    env.run_inference(&setup).await;
    assert!(path.exists(), "setup: Write must have created the file");
    assert_eq!(undo_depth(&env), 1, "setup turn = 1 entry");

    // ── Test turn: delete it ──
    env.insert_user_message("delete it").await;
    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "Delete",
            serde_json::json!({"path": path.to_string_lossy()}),
        ),
        MockResponse::Text("Gone.".into()),
    ]);
    env.run_inference(&provider).await;

    assert!(!path.exists(), "Delete should have removed the file");
    assert_eq!(undo_depth(&env), 2, "setup + delete = 2 entries");

    // Undo the delete: file comes back with original content.
    undo_one(&env);
    assert!(path.exists(), "undo should have recreated the file");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
    assert_eq!(undo_depth(&env), 1);
}
/// Multiple file mutations within a single turn must collapse into ONE
/// undo entry (not N entries). This is the contract that lets users
/// undo "what the agent did this turn" as an atomic unit, regardless
/// of how many files it touched.
///
/// Scripts a turn with three sequential Write tool-calls (one Write per
/// roundtrip inside the same `run_inference` call) so we exercise the
/// `pending`-deduplication path explicitly.
#[tokio::test]
async fn multiple_mutations_in_one_turn_share_one_undo_entry() {
    let env = Env::new().await;
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(env.root.join(name), format!("orig-{name}")).unwrap();
    }

    env.insert_user_message("rewrite all three").await;
    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "Write",
            serde_json::json!({"file_path": "a.txt", "content": "NEW-A", "overwrite": true}),
        ),
        MockResponse::tool_call(
            "Write",
            serde_json::json!({"file_path": "b.txt", "content": "NEW-B", "overwrite": true}),
        ),
        MockResponse::tool_call(
            "Write",
            serde_json::json!({"file_path": "c.txt", "content": "NEW-C", "overwrite": true}),
        ),
        MockResponse::Text("All three rewritten.".into()),
    ]);
    env.run_inference(&provider).await;

    assert_eq!(
        undo_depth(&env),
        1,
        "three writes in one turn must produce exactly one undo entry"
    );

    let summary = undo_one(&env);
    assert!(
        summary.contains("3 file"),
        "summary should report 3 files restored, got: {summary}"
    );

    // All three files restored to original.
    for name in ["a.txt", "b.txt", "c.txt"] {
        assert_eq!(
            std::fs::read_to_string(env.root.join(name)).unwrap(),
            format!("orig-{name}"),
            "{name} should be restored to original"
        );
    }

    // Stack now empty.
    assert_eq!(undo_depth(&env), 0);
}

/// Two separate turns produce two independent undo entries. Undoing
/// pops them in LIFO order, restoring intermediate state first.
#[tokio::test]
async fn two_turns_create_two_independent_undo_entries() {
    let env = Env::new().await;
    let path = env.root.join("evolving.txt");
    std::fs::write(&path, "v1").unwrap();

    // ── Turn 1 ──
    env.insert_user_message("upgrade to v2").await;
    let provider1 = MockProvider::new(vec![
        MockResponse::tool_call(
            "Write",
            serde_json::json!({"file_path": "evolving.txt", "content": "v2", "overwrite": true}),
        ),
        MockResponse::Text("Upgraded to v2.".into()),
    ]);
    env.run_inference(&provider1).await;
    assert_eq!(undo_depth(&env), 1);

    // ── Turn 2 ──
    env.insert_user_message("upgrade to v3").await;
    let provider2 = MockProvider::new(vec![
        MockResponse::tool_call(
            "Write",
            serde_json::json!({"file_path": "evolving.txt", "content": "v3", "overwrite": true}),
        ),
        MockResponse::Text("Upgraded to v3.".into()),
    ]);
    env.run_inference(&provider2).await;
    assert_eq!(undo_depth(&env), 2, "second turn must add a second entry");

    // Undo turn 2 → file is back at v2 (NOT v1).
    undo_one(&env);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "v2");
    assert_eq!(undo_depth(&env), 1);

    // Undo turn 1 → file is back at v1.
    undo_one(&env);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "v1");
    assert_eq!(undo_depth(&env), 0);
}

/// Read-only tools (Glob, Grep, Read) must not push anything into the
/// undo stack — that would clutter the user's `/undo` history with
/// no-op entries and waste memory snapshotting unread bytes.
///
/// Asserts via `commit_turn`'s "no-op when pending is empty" contract:
/// after a turn that only invokes read-only tools, `depth()` stays 0.
#[tokio::test]
async fn read_only_tools_dont_affect_undo_stack() {
    let env = Env::new().await;
    let src = env.root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("main.rs"), "fn main() {}").unwrap();
    std::fs::write(src.join("lib.rs"), "pub mod foo;").unwrap();

    env.insert_user_message("explore the codebase").await;
    let provider = MockProvider::new(vec![
        MockResponse::tool_call("Glob", serde_json::json!({"pattern": "src/*.rs"})),
        MockResponse::tool_call("Grep", serde_json::json!({"pattern": "fn", "path": "."})),
        MockResponse::tool_call("Read", serde_json::json!({"file_path": "src/main.rs"})),
        MockResponse::Text("Explored.".into()),
    ]);
    env.run_inference(&provider).await;

    assert_eq!(
        undo_depth(&env),
        0,
        "read-only tools must not produce any undo entries"
    );
}

/// Parallel tool calls (multiple ToolCalls in a single LLM response)
/// in one turn must also collapse into ONE undo entry. This protects
/// against a regression where someone "fixes" the dispatch loop to
/// commit per-tool-call instead of per-turn.
#[tokio::test]
async fn parallel_writes_in_one_response_share_one_undo_entry() {
    let env = Env::new().await;
    for name in ["x.txt", "y.txt"] {
        std::fs::write(env.root.join(name), format!("original-{name}")).unwrap();
    }

    env.insert_user_message("rewrite x and y in parallel").await;
    let provider = MockProvider::new(vec![
        // Two ToolCalls in ONE LlmResponse — i.e. "parallel tool use".
        MockResponse::ToolCalls(vec![
            write_call("call_x", "x.txt", "AFTER-X"),
            write_call("call_y", "y.txt", "AFTER-Y"),
        ]),
        MockResponse::Text("Both rewritten.".into()),
    ]);
    env.run_inference(&provider).await;

    assert_eq!(
        undo_depth(&env),
        1,
        "two parallel writes in one response must produce one entry, not two"
    );

    undo_one(&env);
    assert_eq!(
        std::fs::read_to_string(env.root.join("x.txt")).unwrap(),
        "original-x.txt"
    );
    assert_eq!(
        std::fs::read_to_string(env.root.join("y.txt")).unwrap(),
        "original-y.txt"
    );
}
