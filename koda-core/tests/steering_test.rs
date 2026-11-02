//! Text sent with `QueueNext` reaches the model in the turn it was typed in,
//! or comes back to the client. It is never dropped, and it never waits
//! silently for the user's next prompt.

use koda_core::engine::sink::TestSink;
use koda_core::engine::{ApprovalDecision, EngineCommand, EngineEvent, EngineSink};
use koda_core::loop_guard::LoopContinuation;
use koda_core::persistence::Persistence;
use koda_core::providers::mock::{MockProvider, MockResponse};
use koda_core::providers::{ChatMessage, LlmProvider};
use koda_core::session::KodaSession;
use koda_core::tools::{ToolCatalog, ToolRegistry};
use koda_core::trust::TrustMode;
use koda_test_utils::Env;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(10);
const STEER: &str = "please-also-do-this";

/// A sink that "types" `text` the first time an event matches, before the
/// engine moves on. That pins exactly when the text lands in the channel.
struct Typist {
    events: Arc<TestSink>,
    cmd_tx: mpsc::Sender<EngineCommand>,
    when: fn(&EngineEvent) -> bool,
    text: &'static str,
    typed: AtomicBool,
}

impl EngineSink for Typist {
    fn emit(&self, event: EngineEvent) {
        if (self.when)(&event) && !self.typed.swap(true, Ordering::SeqCst) {
            let text = self.text.to_string();
            self.cmd_tx
                .try_send(EngineCommand::QueueNext { text })
                .unwrap();
        }
        self.events.emit(event);
    }
}

/// The first model call of the turn has started: text typed now is what a
/// user types while the model streams.
fn streaming(event: &EngineEvent) -> bool {
    matches!(event, EngineEvent::SpinnerStart { .. })
}

fn loop_cap(event: &EngineEvent) -> bool {
    matches!(event, EngineEvent::LoopCapReached { .. })
}

struct Harness {
    env: Env,
    session: KodaSession,
    calls: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
    events: Arc<TestSink>,
    cmd_tx: mpsc::Sender<EngineCommand>,
    cmd_rx: mpsc::Receiver<EngineCommand>,
}

impl Harness {
    async fn new(env: Env, responses: Vec<MockResponse>) -> Self {
        let tools = ToolRegistry::new(env.root.clone(), env.config.max_context_tokens);
        let agent = Arc::new(koda_core::agent::KodaAgent {
            project_root: env.root.clone(),
            tools,
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
        let provider = MockProvider::new(responses);
        let calls = provider.recorded_calls_handle();
        session.provider = Box::new(provider) as Box<dyn LlmProvider>;
        env.insert_user_message("go").await;
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        Self {
            env,
            session,
            calls,
            events: Arc::new(TestSink::new()),
            cmd_tx,
            cmd_rx,
        }
    }

    fn typist(&self, when: fn(&EngineEvent) -> bool, text: &'static str) -> Typist {
        Typist {
            events: Arc::clone(&self.events),
            cmd_tx: self.cmd_tx.clone(),
            when,
            text,
            typed: AtomicBool::new(false),
        }
    }

    fn model_calls(&self) -> Vec<Vec<ChatMessage>> {
        self.calls.lock().unwrap().clone()
    }

    fn call_saw(&self, call: usize, text: &str) -> bool {
        format!("{:?}", self.model_calls()[call]).contains(text)
    }

    async fn history_holds(&self, text: &str) -> bool {
        let history = self
            .env
            .db
            .load_context(&self.env.session_id)
            .await
            .unwrap();
        history
            .iter()
            .any(|m| m.content.as_deref().is_some_and(|c| c.contains(text)))
    }
}

fn approval_id(event: &EngineEvent) -> Option<String> {
    match event {
        EngineEvent::ApprovalRequest { id, .. } => Some(id.clone()),
        _ => None,
    }
}

fn ask_id(event: &EngineEvent) -> Option<String> {
    match event {
        EngineEvent::AskUserRequest { id, .. } => Some(id.clone()),
        _ => None,
    }
}

fn cap_id(event: &EngineEvent) -> Option<String> {
    match event {
        EngineEvent::LoopCapReached { id, .. } => Some(id.clone()),
        _ => None,
    }
}

/// Wait for a request the turn makes, failing if the turn ends first.
async fn request<F, T>(
    turn: &mut (impl Future<Output = anyhow::Result<()>> + Unpin),
    events: &TestSink,
    id: F,
) -> T
where
    F: Fn(&EngineEvent) -> Option<T>,
{
    tokio::select! {
        id = events.wait_for_map(WAIT, id) => id.expect("the turn never asked"),
        result = turn => panic!("the turn ended without asking: {result:?}"),
    }
}

async fn finish(turn: impl Future<Output = anyhow::Result<()>>) {
    tokio::time::timeout(WAIT, turn)
        .await
        .expect("the turn did not end")
        .unwrap();
}

fn write() -> MockResponse {
    MockResponse::tool_call(
        "Write",
        serde_json::json!({"file_path": "a.txt", "content": "x"}),
    )
}

fn ask() -> MockResponse {
    MockResponse::tool_call("AskUser", serde_json::json!({"question": "which?"}))
}

fn list() -> MockResponse {
    MockResponse::tool_call("List", serde_json::json!({}))
}

fn text(reply: &str) -> MockResponse {
    MockResponse::Text(reply.into())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_typed_before_an_approval_reaches_the_next_call() {
    let env = Env::new().await;
    let mut h = Harness::new(env, vec![write(), text("done")]).await;
    let sink = h.typist(streaming, STEER);

    let mut turn = Box::pin(
        h.session
            .run_turn(&h.env.config, None, &sink, &mut h.cmd_rx, None),
    );
    let id = request(&mut turn, &h.events, approval_id).await;
    h.cmd_tx
        .send(EngineCommand::ApprovalResponse {
            id,
            decision: ApprovalDecision::Approve,
        })
        .await
        .unwrap();
    finish(turn).await;

    assert_eq!(h.model_calls().len(), 2);
    assert!(h.call_saw(1, STEER), "the approval wait lost the text");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_typed_before_a_question_reaches_the_next_call() {
    let env = Env::new().await;
    let mut h = Harness::new(env, vec![ask(), text("done")]).await;
    let sink = h.typist(streaming, STEER);

    let mut turn = Box::pin(
        h.session
            .run_turn(&h.env.config, None, &sink, &mut h.cmd_rx, None),
    );
    let id = request(&mut turn, &h.events, ask_id).await;
    h.cmd_tx
        .send(EngineCommand::AskUserResponse {
            id,
            answer: "that one".into(),
        })
        .await
        .unwrap();
    finish(turn).await;

    assert_eq!(h.model_calls().len(), 2);
    assert!(h.call_saw(1, STEER), "the ask-user wait lost the text");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_sent_during_the_loop_cap_wait_reaches_the_next_call() {
    let mut env = Env::new().await;
    env.config.max_iterations = 1;
    let mut h = Harness::new(env, vec![list(), text("done")]).await;
    let sink = h.typist(loop_cap, STEER);

    let mut turn = Box::pin(
        h.session
            .run_turn(&h.env.config, None, &sink, &mut h.cmd_rx, None),
    );
    let id = request(&mut turn, &h.events, cap_id).await;
    h.cmd_tx
        .send(EngineCommand::LoopDecision {
            id,
            action: LoopContinuation::Continue50,
        })
        .await
        .unwrap();
    finish(turn).await;

    assert_eq!(h.model_calls().len(), 2);
    assert!(h.call_saw(1, STEER), "the loop-cap wait lost the text");
}

/// The user said stop, so the text must not sit unanswered in the history.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_at_the_loop_cap_hands_the_text_back() {
    let mut env = Env::new().await;
    env.config.max_iterations = 1;
    let mut h = Harness::new(env, vec![list(), text("never sent")]).await;
    let sink = h.typist(streaming, STEER);

    let mut turn = Box::pin(
        h.session
            .run_turn(&h.env.config, None, &sink, &mut h.cmd_rx, None),
    );
    let id = request(&mut turn, &h.events, cap_id).await;
    h.cmd_tx
        .send(EngineCommand::LoopDecision {
            id,
            action: LoopContinuation::Stop,
        })
        .await
        .unwrap();
    finish(turn).await;

    assert!(!h.history_holds(STEER).await);
    assert_eq!(h.session.take_queued_steers(&mut h.cmd_rx), vec![STEER]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_typed_before_a_final_reply_reaches_the_model_in_the_same_turn() {
    let env = Env::new().await;
    let mut h = Harness::new(env, vec![text("first"), text("second")]).await;
    let sink = h.typist(streaming, STEER);

    let turn = h
        .session
        .run_turn(&h.env.config, None, &sink, &mut h.cmd_rx, None);
    finish(turn).await;

    assert_eq!(
        h.model_calls().len(),
        2,
        "the turn ended on the first reply"
    );
    assert!(h.call_saw(1, STEER));
    assert!(h.call_saw(1, "first"), "the model lost its own reply");

    let events = h.events.events();
    let ends = events
        .iter()
        .filter(|e| matches!(e, EngineEvent::TurnEnd { .. }))
        .count();
    assert_eq!(ends, 1, "continuing must not start a second turn");
    let footers: Vec<usize> = events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::Footer { total_chars, .. } => Some(*total_chars),
            _ => None,
        })
        .collect();
    assert_eq!(
        footers,
        vec!["first".len() + "second".len()],
        "one footer, counting both replies"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steers_reach_the_model_in_the_order_they_were_typed() {
    let env = Env::new().await;
    let mut h = Harness::new(env, vec![write(), text("done")]).await;
    let sink = h.typist(streaming, "typed-first");

    let mut turn = Box::pin(
        h.session
            .run_turn(&h.env.config, None, &sink, &mut h.cmd_rx, None),
    );
    let id = request(&mut turn, &h.events, approval_id).await;
    for command in [
        EngineCommand::QueueNext {
            text: "typed-second".into(),
        },
        EngineCommand::ApprovalResponse {
            id,
            decision: ApprovalDecision::Approve,
        },
        EngineCommand::QueueNext {
            text: "typed-third".into(),
        },
    ] {
        h.cmd_tx.send(command).await.unwrap();
    }
    finish(turn).await;

    let saw = format!("{:?}", h.model_calls()[1]);
    let first = saw.find("typed-first").expect("first text lost");
    let second = saw.find("typed-second").expect("second text lost");
    let third = saw.find("typed-third").expect("third text lost");
    assert!(first < second && second < third, "{saw}");
}

/// A cancelled turn injects nothing: the user stopped it, so the text must
/// not reach the model behind their next prompt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_wait_hands_the_text_back() {
    let env = Env::new().await;
    let mut h = Harness::new(env, vec![write(), text("never sent")]).await;
    let sink = h.typist(streaming, STEER);
    let cancel = h.session.cancel_handle();

    let mut turn = Box::pin(
        h.session
            .run_turn(&h.env.config, None, &sink, &mut h.cmd_rx, None),
    );
    request(&mut turn, &h.events, approval_id).await;
    cancel.interrupt();
    finish(turn).await;

    assert!(!h.history_holds(STEER).await);
    assert_eq!(h.session.take_queued_steers(&mut h.cmd_rx), vec![STEER]);
}

/// Both a cancel and a queued steer can land in the same reply.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_reply_hands_the_text_back() {
    let env = Env::new().await;
    let mut h = Harness::new(env, vec![text("first"), text("second")]).await;
    let cancel = h.session.cancel_handle();
    let sink = CancelOnText {
        typist: h.typist(streaming, STEER),
        cancel,
    };

    let turn = h
        .session
        .run_turn(&h.env.config, None, &sink, &mut h.cmd_rx, None);
    finish(turn).await;

    assert_eq!(
        h.model_calls().len(),
        1,
        "a cancelled turn must not continue"
    );
    assert!(!h.history_holds(STEER).await);
    assert_eq!(h.session.take_queued_steers(&mut h.cmd_rx), vec![STEER]);
}

/// Types like [`Typist`], and interrupts once the reply starts.
struct CancelOnText {
    typist: Typist,
    cancel: koda_core::session::SessionCancel,
}

impl EngineSink for CancelOnText {
    fn emit(&self, event: EngineEvent) {
        if matches!(event, EngineEvent::TextDelta { .. }) {
            self.cancel.interrupt();
        }
        self.typist.emit(event);
    }
}

/// The client can send text in the moment between the engine's last look at
/// the channel and the turn's end. It comes back with the rest.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_sent_as_the_turn_ends_is_handed_back_after_it() {
    let env = Env::new().await;
    let mut h = Harness::new(env, vec![text("done")]).await;
    let sink = h.typist(|_| false, STEER);

    let turn = h
        .session
        .run_turn(&h.env.config, None, &sink, &mut h.cmd_rx, None);
    finish(turn).await;

    h.cmd_tx
        .send(EngineCommand::QueueNext { text: STEER.into() })
        .await
        .unwrap();
    assert_eq!(h.session.take_queued_steers(&mut h.cmd_rx), vec![STEER]);
    assert!(h.session.take_queued_steers(&mut h.cmd_rx).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_still_ends_a_wait_with_text_queued() {
    for responses in [
        vec![write(), text("never sent")],
        vec![ask(), text("never sent")],
    ] {
        let env = Env::new().await;
        let mut h = Harness::new(env, responses).await;
        let sink = h.typist(streaming, STEER);
        let cmd_tx = h.cmd_tx.clone();

        let mut turn =
            Box::pin(
                h.session
                    .run_turn(&h.env.config, None, &sink, &mut h.cmd_rx, None),
            );
        let asked = |e: &EngineEvent| approval_id(e).or_else(|| ask_id(e));
        request(&mut turn, &h.events, asked).await;
        cmd_tx.send(EngineCommand::Interrupt).await.unwrap();
        finish(turn).await;

        assert_eq!(h.model_calls().len(), 1, "the turn went on");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_still_ends_the_loop_cap_wait_with_text_queued() {
    let mut env = Env::new().await;
    env.config.max_iterations = 1;
    let mut h = Harness::new(env, vec![list(), text("never sent")]).await;
    let sink = h.typist(loop_cap, STEER);
    let cmd_tx = h.cmd_tx.clone();

    let mut turn = Box::pin(
        h.session
            .run_turn(&h.env.config, None, &sink, &mut h.cmd_rx, None),
    );
    request(&mut turn, &h.events, cap_id).await;
    cmd_tx.send(EngineCommand::Interrupt).await.unwrap();
    finish(turn).await;

    assert_eq!(h.model_calls().len(), 1, "the turn went on");
}
