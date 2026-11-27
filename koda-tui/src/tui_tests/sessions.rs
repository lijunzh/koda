//! Sessions: new, renamed, resumed, deleted, and the commands that run in
//! a session of their own (`/side`, `/review`) or report on one
//! (`/feedback`, `/init`).

use crossterm::event::KeyCode;
use koda_test_utils::network::FakeLlmServer;
use koda_test_utils::openai::text;
use serde_json::json;

use super::{ctrl, esc, picker_lines};
use crate::test_tui::{BUSY_PROMPT, Driver, TestTui};

/// `/new` starts a session with none of the last one's history.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn new_starts_a_fresh_session() {
    let tui = TestTui::start(vec![text("All done."), text("All done.")]).await;
    tui.run(async |driver: Driver| {
        driver
            .submit_and_finish("echo topic please", "All done.")
            .await;
        driver.submit("/new").await;
        driver.wait_for_screen("new session").await;
        driver.submit_and_finish("a fresh start", "All done.").await;
        let sent = driver.model_call(2).await;
        assert!(sent.contains("a fresh start"), "{sent}");
        assert!(
            !sent.contains("echo topic please"),
            "history carried over: {sent}"
        );
        driver
    })
    .await;
}

/// `/rename` names the session and `/status` shows the name, the model and
/// the mode.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn rename_and_status() {
    let tui = TestTui::start_safe(vec![]).await;
    tui.run(async |driver: Driver| {
        driver.submit("/rename Parser work").await;
        driver.wait_for_screen("Renamed to \"Parser work\"").await;
        driver.submit("/status").await;
        driver.wait_for_screen("Instructions").await;
        let screen = driver.screen();
        assert!(screen.contains("Parser work"), "{screen}");
        assert!(screen.contains("stub-model"), "{screen}");
        assert!(screen.contains("safe"), "{screen}");
        driver
    })
    .await;
}

/// A session with one finished turn, from `prompt`, in a TUI that then
/// quits; its id.
async fn one_turn(tui: TestTui, prompt: &str) -> (Driver, String) {
    let driver = tui
        .run(async |driver: Driver| {
            driver.submit_and_finish(prompt, "All done.").await;
            driver
        })
        .await;
    let earlier = driver.sessions().await;
    let id = earlier[0].clone();
    (driver, id)
}

/// `/resume <id>` resumes by id; `/resume delete <id>` removes a
/// session, and the picker no longer offers it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn sessions_resume_and_delete_by_id() {
    let tui = TestTui::start(vec![text("All done."); 3]).await;
    let (driver, bravo) = one_turn(tui, "bravo session words").await;
    let (driver, charlie) = one_turn(
        TestTui::start_again(driver, None).await,
        "charlie session words",
    )
    .await;
    assert_ne!(bravo, charlie);

    let tui = TestTui::start_again(driver, None).await;
    tui.run(async |driver: Driver| {
        driver.submit(&format!("/resume {bravo}")).await;
        driver.wait_for_screen("bravo session words").await;
        // Its earlier turn is drawn as it was live (6h-b3).
        driver.wait_for_screen("session resumed").await;
        let screen = driver.screen();
        assert!(screen.contains("Worked for"), "{screen}");
        driver.submit_and_finish("bravo again", "All done.").await;
        let sent = driver.model_call(3).await;
        assert!(sent.contains("bravo session words"), "{sent}");
        assert!(!sent.contains("charlie session words"), "{sent}");

        driver.submit(&format!("/resume delete {charlie}")).await;
        driver
            .wait_for_screen(&format!("Deleted session {}", &charlie[..8]))
            .await;
        driver.submit("/resume").await;
        driver.wait_for_screen("Sessions").await;
        let listed = picker_lines(&driver.screen(), "Sessions");
        assert!(
            !listed.contains(&charlie[..8]),
            "deleted session listed:\n{listed}"
        );
        assert!(listed.contains(&bravo[..8]), "{listed}");
        driver.key(esc());
        driver.wait_for_screen_without("Sessions").await;
        driver
    })
    .await;
}

/// `/init` asks the model to write AGENTS.md, as Codex's `/init` does, and
/// leaves an existing instruction file alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn init_asks_for_agents_md_unless_there_is_one() {
    let tui = TestTui::start(vec![text("All done.")]).await;
    tui.run(async |driver: Driver| {
        driver.submit("/init").await;
        let sent = driver.model_call(1).await;
        assert!(sent.contains("Create an AGENTS.md file"), "{sent}");
        driver.wait_for_screen("All done.").await;
        driver.wait_for_screen_without(BUSY_PROMPT).await;

        std::fs::write(driver.project.path().join("AGENTS.md"), "# Notes\n").unwrap();
        driver.submit("/init").await;
        driver.wait_for_screen("AGENTS.md already exists").await;
        assert_eq!(driver.model_calls().await, 1);
        driver
    })
    .await;
}

/// `/feedback` asks Codex's three questions (what kind, logs or not, a
/// note) and saves a ZIP with the report, the log and the session's rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn feedback_saves_a_report_with_the_session() {
    let tui = TestTui::start(vec![text("All done.")]).await;
    let driver = tui
        .run(async |driver: Driver| {
            driver.submit_and_finish("hello", "All done.").await;

            driver.submit("/feedback").await;
            driver.wait_for_screen("How was this?").await;
            // "bug" comes first, then "yes" to the logs.
            driver.press(KeyCode::Enter);
            driver
                .wait_for_screen("Include logs and session history?")
                .await;
            driver.press(KeyCode::Enter);
            driver.wait_for_screen("Note (optional)").await;
            driver.type_keys("it hung");
            driver.wait_for_screen("it hung").await;
            driver.press(KeyCode::Enter);
            driver.wait_for_screen("Feedback saved").await;
            driver.wait_for_screen("open an issue").await;
            driver
        })
        .await;

    let dir = driver.config_dir().join("feedback");
    let zip = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "zip"))
        .expect("a feedback ZIP");
    let mut archive = zip::ZipArchive::new(std::fs::File::open(&zip).unwrap()).unwrap();
    let names: Vec<String> = archive
        .file_names()
        .map(|name| name.unwrap().to_string())
        .collect();
    assert!(names.contains(&"report.json".to_string()), "{names:?}");
    assert!(
        names.iter().any(|n| n.ends_with("/koda-logs.log")),
        "{names:?}"
    );
    assert!(names.iter().any(|n| n.contains("/session-")), "{names:?}");
    let mut report = String::new();
    std::io::Read::read_to_string(&mut archive.by_name("report.json").unwrap(), &mut report)
        .unwrap();
    assert!(report.contains("\"reason\": \"it hung\""), "{report}");
    assert!(report.contains("\"classification\": \"bug\""), "{report}");
}

/// `/side` asks in a fork with Codex's side instructions and boundary, and
/// Ctrl+C returns: the main session goes on without the side question, and
/// the side conversation is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn side_asks_in_a_fork_and_ctrl_c_returns() {
    let tui = TestTui::start(vec![text("All done."); 3]).await;
    let driver = tui
        .run(async |driver: Driver| {
            driver.submit_and_finish("pick a colour", "All done.").await;

            driver.submit("/side which colour was it").await;
            driver.wait_for_screen("side conversation").await;
            let sent = driver.model_call(2).await;
            assert!(sent.contains("You are in a side conversation"), "{sent}");
            assert!(sent.contains("side_conversation_boundary"), "{sent}");
            assert!(sent.contains("pick a colour"), "history lost: {sent}");
            assert!(sent.contains("which colour was it"), "{sent}");
            driver.wait_for_screen_without(BUSY_PROMPT).await;
            driver.wait_for_screen("Ask a side question").await;

            driver.key(ctrl('c'));
            driver.wait_for_screen("Back in the main session").await;
            driver.submit_and_finish("carry on", "All done.").await;
            let sent = driver.model_call(3).await;
            assert!(sent.contains("pick a colour"), "{sent}");
            assert!(sent.contains("carry on"), "{sent}");
            assert!(
                !sent.contains("which colour was it"),
                "the side leaked: {sent}"
            );
            assert!(!sent.contains("side conversation"), "{sent}");
            driver
        })
        .await;

    let sessions = driver.sessions().await;
    assert_eq!(sessions.len(), 1, "the side conversation was kept");
    assert!(
        driver
            .stored_messages(&sessions[0])
            .await
            .contains("carry on")
    );
}

/// `/review` runs Codex's rubric in a session of its own, lists the
/// findings back in the main session, and the next prompt there carries
/// them, as Codex's review hands its results to the thread.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn review_lists_findings_and_the_next_prompt_carries_them() {
    let findings = json!({
        "findings": [{
            "title": "[P1] Off by one in the loop",
            "body": "The loop skips the last item.",
            "confidence_score": 0.9,
            "priority": 1,
            "code_location": {
                "absolute_file_path": "/tmp/x/src/lib.rs",
                "line_range": {"start": 3, "end": 4}
            }
        }],
        "overall_correctness": "patch is incorrect",
        "overall_explanation": "It drops data.",
        "overall_confidence_score": 0.8
    })
    .to_string();
    let server = FakeLlmServer::spawn().await;
    server
        .mount_fn("POST", r".*/chat/completions$", move |request| {
            let body = String::from_utf8_lossy(&request.body);
            if body.contains("Review guidelines") {
                text(&findings)
            } else {
                text("All done.")
            }
        })
        .await;
    let tui = TestTui::start_on(server).await;
    let driver = tui
        .run(async |driver: Driver| {
            driver.submit_and_finish("make a change", "All done.").await;

            driver.submit("/review").await;
            let sent = driver.model_call(2).await;
            assert!(sent.contains("Review the current code changes"), "{sent}");
            assert!(
                !sent.contains("make a change"),
                "the review saw the session: {sent}"
            );
            driver.wait_for_screen("Review of current changes").await;
            driver.wait_for_screen("[P1] Off by one in the loop").await;
            driver.wait_for_screen("Verdict: patch is incorrect").await;

            driver.submit_and_finish("fix the P1", "All done.").await;
            let sent = driver.model_call(3).await;
            assert!(sent.contains("<user_action>"), "{sent}");
            assert!(sent.contains("Off by one in the loop"), "{sent}");
            assert!(sent.contains("make a change"), "{sent}");
            assert!(!sent.contains("Review guidelines"), "{sent}");
            driver
        })
        .await;

    let sessions = driver.sessions().await;
    assert_eq!(sessions.len(), 1, "the review session was kept");
    assert!(
        driver
            .stored_messages(&sessions[0])
            .await
            .contains("fix the P1")
    );
}
