//! Sessions: new, renamed, resumed, deleted, and the commands that run in
//! a session of their own (`/side`, `/review`) or report on one
//! (`/feedback`, `/init`).

use std::collections::VecDeque;
use std::sync::Mutex;

use crossterm::event::KeyCode;
use koda_test_utils::network::FakeLlmServer;
use koda_test_utils::openai::{text, tool_calls};
use serde_json::json;

use super::{asked_write, ctrl, esc, go, json_reply, picker_lines, wait_for_go};
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
    let tui = TestTui::start(vec![]).await;
    tui.run(async |driver: Driver| {
        driver.submit("/rename Parser work").await;
        driver.wait_for_screen("Renamed to \"Parser work\"").await;
        driver.submit("/status").await;
        driver.wait_for_screen("Instructions").await;
        let screen = driver.screen();
        assert!(screen.contains("Parser work"), "{screen}");
        assert!(screen.contains("stub-model"), "{screen}");
        assert!(screen.contains("auto"), "{screen}");
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

/// Typing in the `/resume` picker searches what was said: a session whose
/// name doesn't have the word is listed with the words around it, the typed
/// text stays out of the input box, and Enter resumes the session found.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn typing_in_the_picker_finds_a_session_by_what_was_said() {
    let replies = vec![
        text("The zebra crossing is the safe place."),
        text("All done."),
        text("All done."),
    ];
    let driver = TestTui::start(replies)
        .await
        .run(async |driver: Driver| {
            driver
                .submit_and_finish("bravo session words", "zebra crossing")
                .await;
            driver
        })
        .await;
    let bravo = driver.sessions().await[0].clone();
    let (driver, charlie) = one_turn(
        TestTui::start_again(driver, None).await,
        "charlie session words",
    )
    .await;
    assert_ne!(bravo, charlie);

    let tui = TestTui::start_again(driver, None).await;
    tui.run(async |driver: Driver| {
        driver.submit("/resume").await;
        driver.wait_for_screen("Sessions").await;
        let listed = picker_lines(&driver.screen(), "Sessions");
        assert!(listed.contains(&charlie[..8]), "{listed}");

        driver.type_keys("zebra");
        // The host's answer: bravo, by its reply, where its date was.
        driver.wait_for_screen("The zebra crossing").await;
        let screen = driver.screen();
        let listed = picker_lines(&screen, "Sessions matching \"zebra\"");
        let row = listed
            .lines()
            .find(|line| line.contains(&bravo[..8]))
            .unwrap_or_else(|| panic!("bravo isn't listed:\n{listed}"));
        assert!(row.contains("bravo session words"), "{row}");
        assert!(
            row.contains("The zebra crossing is the safe place."),
            "{row}"
        );
        assert!(!listed.contains(&charlie[..8]), "{listed}");
        let typed = crate::test_tui::input_box(&screen);
        assert!(!typed.contains("zebra"), "typed into the prompt: {typed}");

        driver.press(KeyCode::Enter);
        driver
            .wait_for_screen(&format!("Resumed session {}", &bravo[..8]))
            .await;
        driver.wait_for_screen("session resumed").await;
        driver.submit_and_finish("bravo again", "All done.").await;
        let sent = driver.model_call(3).await;
        assert!(sent.contains("bravo session words"), "{sent}");
        assert!(!sent.contains("charlie session words"), "{sent}");
        driver
    })
    .await;
}

/// `/confidential` marks the session and `/confidential off` unmarks it,
/// each saying what it did in one line, and `/status` shows the mark. The
/// host's own notice of the mark, which follows the answer, isn't said as
/// well.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn confidential_marks_and_unmarks_the_session() {
    use koda_state::session_store::SessionStore;
    let tui = TestTui::start(vec![text("All done.")]).await;
    tui.run(async |driver: Driver| {
        let marked = async || {
            let id = driver.sessions().await[0].clone();
            let stored = driver.db().await.get_session(&id).await.unwrap().unwrap();
            stored.confidential
        };
        assert!(!marked().await);
        driver.submit("/confidential").await;
        driver
            .wait_for_screen("Marked confidential: only a local model can read this session")
            .await;
        assert!(marked().await);
        // A turn's messages come after the host's notice of the mark: once
        // it has finished, the TUI has read the notice.
        driver.submit_and_finish("carry on", "All done.").await;
        let screen = driver.screen();
        assert!(
            !screen.contains("This session is now confidential"),
            "said twice:\n{screen}"
        );
        driver.submit("/status").await;
        driver.wait_for_screen("Instructions").await;
        let screen = driver.screen();
        assert!(
            screen.contains("Confidential yes: only a local model can read it"),
            "{screen}"
        );

        driver.submit("/confidential off").await;
        driver.wait_for_screen("No longer confidential").await;
        assert!(!marked().await);
        driver.submit("/confidential maybe").await;
        driver.wait_for_screen("Usage: /confidential marks").await;
        assert!(!marked().await);
        driver
    })
    .await;
}

/// `/confidential brief` is about what models on remote providers are told
/// of a confidential session. A local model's draft is said in one line
/// when it arrives, with how to accept it; bare, the command shows the
/// brief and the draft; `accept` makes the draft the brief, a text sets
/// the user's own and `clear` removes it, each said once; `/status` shows
/// the brief. A session that isn't confidential has none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn confidential_brief_shows_accepts_sets_and_clears_the_brief() {
    use koda_state::session_store::SessionStore;
    let replies = vec![text("All done."), json_reply("\"Planning a trip\"")];
    let tui = TestTui::start(replies).await;
    tui.run(async |driver: Driver| {
        let briefs = async || {
            let id = driver.sessions().await[0].clone();
            let stored = driver.db().await.get_session(&id).await.unwrap().unwrap();
            (stored.confidential_brief, stored.confidential_brief_draft)
        };
        let some = |text: &str| Some(text.to_string());
        driver.submit("/confidential brief").await;
        driver
            .wait_for_screen("This session isn't confidential, so it has no brief.")
            .await;
        driver.submit("/confidential brief Planning").await;
        driver
            .wait_for_screen_that(
                |screen| screen.matches("so it has no brief").count() == 2,
                "the brief refused again",
            )
            .await;

        // Marked while idle on its local model, which has answered: the
        // model drafts the brief, and the TUI says so.
        driver
            .submit_and_finish("plan the okapi trip", "All done.")
            .await;
        driver.submit("/confidential").await;
        driver
            .wait_for_screen("A local model drafted this session's brief")
            .await;
        let screen = driver.screen();
        assert!(
            screen.contains("\"Planning a trip\"") && screen.contains("/confidential brief accept"),
            "{screen}"
        );
        assert_eq!(briefs().await, (None, some("Planning a trip")));
        let asked = driver.model_call(2).await;
        assert!(asked.contains("plan the okapi trip"), "{asked}");

        driver.submit("/confidential brief").await;
        driver
            .wait_for_screen("No brief: models on remote providers are told only that")
            .await;
        driver.submit("/confidential brief accept").await;
        driver
            .wait_for_screen("Brief set: models on remote providers are told \"Planning a trip\"")
            .await;
        assert_eq!(briefs().await, (some("Planning a trip"), None));
        driver.submit("/status").await;
        driver.wait_for_screen("Instructions").await;
        let screen = driver.screen();
        let row = format!("{:<13}\"Planning a trip\"", "Brief");
        assert!(screen.contains(&row), "{screen}");
        // The host's notice of the set came before `/status` was answered:
        // it isn't said as another client's.
        assert!(!screen.contains("from another client"), "{screen}");

        driver
            .submit("/confidential brief  Booking travel for a family ")
            .await;
        driver
            .wait_for_screen("are told \"Booking travel for a family\"")
            .await;
        assert_eq!(briefs().await, (some("Booking travel for a family"), None));
        driver.submit("/confidential brief").await;
        driver
            .wait_for_screen("Brief: \"Booking travel for a family\".")
            .await;
        let screen = driver.screen();
        assert!(!screen.contains("from another client"), "{screen}");

        driver.submit("/confidential brief clear").await;
        driver.wait_for_screen("Brief cleared").await;
        assert_eq!(briefs().await, (None, None));
        driver.submit("/confidential brief accept").await;
        driver.wait_for_screen("No draft to accept.").await;
        // The server's own refusal, as it words it.
        driver
            .submit(&format!("/confidential brief {}", "long ".repeat(21)))
            .await;
        driver
            .wait_for_screen("a brief is at most 100 characters")
            .await;
        assert_eq!(briefs().await, (None, None));
        assert_eq!(driver.model_calls().await, 2, "the turn and one draft");
        driver
    })
    .await;
}

/// `/confidential` in a session that was forked marks the fork too, since
/// it holds a copy of the conversation, and says so; `/confidential off`
/// unmarks the session alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn confidential_marks_the_sessions_forks_and_says_so() {
    use koda_state::session_store::SessionStore;
    let tui = TestTui::start(vec![text("All done.")]).await;
    tui.run(async |driver: Driver| {
        driver
            .submit_and_finish("alpha session words", "All done.")
            .await;
        let alpha = driver.sessions().await[0].clone();
        driver.submit("/fork").await;
        driver.wait_for_screen("Forked into").await;
        let sessions = driver.sessions().await;
        let fork = sessions.iter().find(|id| **id != alpha).unwrap().clone();
        let marked = async |id: &str| {
            let stored = driver.db().await.get_session(id).await.unwrap().unwrap();
            stored.confidential
        };

        // Back in the session the fork was made from.
        driver.submit(&format!("/resume {alpha}")).await;
        driver
            .wait_for_screen(&format!("Resumed session {}", &alpha[..8]))
            .await;
        driver.submit("/confidential").await;
        driver
            .wait_for_screen("Marked confidential, with its fork: only a local model can read")
            .await;
        assert!(marked(&alpha).await && marked(&fork).await);

        driver.submit("/confidential off").await;
        driver.wait_for_screen("No longer confidential").await;
        assert!(!marked(&alpha).await && marked(&fork).await);
        driver
    })
    .await;
}

/// The `/resume` picker shows a lock on a confidential session's row and
/// on no other. A session whose model is then given that session's text by
/// `SearchSessions` becomes confidential itself: the TUI says so in one
/// line, and the tool's result told the model.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn the_picker_marks_a_confidential_session_and_reading_one_is_said() {
    use crate::widgets::session_menu::CONFIDENTIAL_MARK;
    use koda_state::session_store::SessionStore;
    let replies = vec![
        text("All done."),
        text("The zebra crossing is the safe place."),
        // Marked while idle on its local model: its brief is drafted.
        json_reply("Crossing a road"),
        tool_calls(&[("call_1", "SearchSessions", json!({"query": "zebra"}))]),
        text("Found it."),
    ];
    let (driver, alpha) = one_turn(TestTui::start(replies).await, "alpha session words").await;
    let driver = TestTui::start_again(driver, None)
        .await
        .run(async |driver: Driver| {
            driver
                .submit_and_finish("bravo session words", "zebra crossing")
                .await;
            driver.submit("/confidential").await;
            driver.wait_for_screen("Marked confidential").await;
            driver
                .wait_for_screen("A local model drafted this session's brief")
                .await;
            driver
        })
        .await;
    let bravo = driver.sessions().await[0].clone();
    assert_ne!(alpha, bravo);

    let tui = TestTui::start_again(driver, None).await;
    tui.run(async |driver: Driver| {
        driver.submit("/resume").await;
        driver.wait_for_screen("Sessions").await;
        let listed = picker_lines(&driver.screen(), "Sessions");
        let row = |id: &str| {
            let row = listed.lines().find(|line| line.contains(&id[..8]));
            row.unwrap_or_else(|| panic!("{id} isn't listed:\n{listed}"))
        };
        assert!(
            row(&bravo).contains(&format!("{CONFIDENTIAL_MARK} bravo session words")),
            "{listed}"
        );
        assert!(!row(&alpha).contains(CONFIDENTIAL_MARK), "{listed}");
        assert_eq!(listed.matches(CONFIDENTIAL_MARK).count(), 1, "{listed}");
        driver.key(esc());
        driver.wait_for_screen_without("Sessions").await;

        // This session runs on a local model, so its search returns the
        // confidential session's text.
        driver.submit("charlie looks for the crossing").await;
        driver
            .wait_for_screen("This session is now confidential")
            .await;
        driver.wait_for_screen("Found it.").await;
        driver.wait_for_screen_without(BUSY_PROMPT).await;
        let screen = driver.screen();
        assert_eq!(
            screen.matches("This session is now confidential").count(),
            1,
            "{screen}"
        );
        let sent = driver.model_call(5).await;
        assert!(
            sent.contains("The zebra crossing is the safe place.")
                && sent.contains("This session is now confidential too"),
            "{sent}"
        );
        let charlie = driver.sessions().await[0].clone();
        assert!(charlie != bravo && charlie != alpha);
        let stored = driver.db().await.get_session(&charlie).await;
        assert!(stored.unwrap().unwrap().confidential);
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

/// Ctrl+C back from a side conversation while the main session's turn runs:
/// the TUI follows that turn at once, without waiting for a settings
/// report, which the host sends only once the turn has ended.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn ctrl_c_back_to_a_running_turn_follows_it_at_once() {
    let server = FakeLlmServer::spawn().await;
    let main = Mutex::new(VecDeque::from([
        tool_calls(&[("call_1", "Bash", wait_for_go())]),
        text("Main done."),
    ]));
    server
        .mount_fn("POST", r".*/chat/completions$", move |request| {
            if String::from_utf8_lossy(&request.body).contains("You are in a side conversation") {
                return text("Side answer.");
            }
            main.lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| text("(no more replies)"))
        })
        .await;
    let tui = TestTui::start_on(server).await;
    tui.run(async |driver: Driver| {
        driver.submit("first prompt").await;
        driver.model_call(1).await;
        driver.wait_for_screen(BUSY_PROMPT).await;
        driver.submit("/side what now").await;
        driver.wait_for_screen("Side answer.").await;
        driver.wait_for_screen("Ask a side question").await;

        driver.key(ctrl('c'));
        driver.wait_for_screen("Back in the main session").await;
        driver.wait_for_screen(BUSY_PROMPT).await;
        go(&driver);
        driver.wait_for_screen("Main done.").await;
        driver.wait_for_screen_without(BUSY_PROMPT).await;
        driver
    })
    .await;
}

/// A turn Koda starts on the main session during a review, here woken by a
/// background command's exit, is followed once the review ends, with its
/// approval. The return ignored it: it ran on unseen and its approval was
/// lost, so it hung.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn a_turn_started_during_a_review_is_followed_after_it() {
    let mut background = wait_for_go();
    background["background"] = json!(true);
    let main = Mutex::new(VecDeque::from([
        tool_calls(&[("call_1", "Bash", background)]),
        text("Started."),
        tool_calls(&[("call_2", "Bash", asked_write("a.txt"))]),
        text("Woke up."),
    ]));
    let review = Mutex::new(VecDeque::from([
        tool_calls(&[("call_r", "Bash", asked_write("r.txt"))]),
        text("No findings."),
    ]));
    let server = FakeLlmServer::spawn().await;
    server
        .mount_fn("POST", r".*/chat/completions$", move |request| {
            let queue = if String::from_utf8_lossy(&request.body).contains("Review guidelines") {
                &review
            } else {
                &main
            };
            queue
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| text("(no more replies)"))
        })
        .await;
    let tui = TestTui::start_on(server).await;
    let driver = tui
        .run(async |driver: Driver| {
            driver.submit_and_finish("first prompt", "Started.").await;

            driver.submit("/review").await;
            driver.wait_for_screen("[y] approve").await;
            // The background command exits: Koda starts a turn on the main
            // session, which asks while the review is on screen.
            go(&driver);
            driver
                .wait_for_screen("The main session is waiting for you")
                .await;
            driver.press(KeyCode::Char('n'));
            driver.wait_for_screen("Back in the main session").await;

            driver.wait_for_screen("[y] approve").await;
            driver.press(KeyCode::Char('y'));
            driver.wait_for_screen("Woke up.").await;
            driver.wait_for_screen_without(BUSY_PROMPT).await;
            driver
        })
        .await;
    assert!(driver.project.path().join("a.txt").exists());
    assert!(!driver.project.path().join("r.txt").exists());
}
