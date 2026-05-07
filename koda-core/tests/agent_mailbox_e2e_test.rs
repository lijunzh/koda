//! End-to-end test for #1325 Phase 2: peer-agent mailbox integration.
//!
//! Phase 2 wired [`koda_core::session::KodaSession`]'s mailbox into
//! the turn lifecycle. This test pins the **observable contract** of
//! that wiring from outside the session module:
//!
//! 1. Mail sent via [`KodaSession::mailbox()`] before a turn lands as
//!    a `Role::User` row in the persisted conversation, and shows up
//!    in the messages sent to the LLM provider on the next turn.
//! 2. Mail enqueued via [`KodaSession::enqueue_for_next_turn`]
//!    behaves identically to (1) — it's the "while idle" sibling of
//!    the live mailbox drain.
//! 3. With no mail, `run_turn` is a no-op for the mailbox path
//!    (regression guard against accidentally inserting empty
//!    user-role rows on every turn).
//!
//! Why a separate test file (vs. extending `session_test.rs`): this
//! exercises the integration *boundary* between the mailbox
//! substrate (`agent::mailbox` + `agent::mail_message`) and the turn
//! lifecycle (`session::run_turn`). Keeping it isolated means a
//! Phase 3 refactor that touches both can update one file's
//! assertions without contaminating session-lifecycle tests.

use koda_core::{
    agent::{AgentPath, InterAgentCommunication, KodaAgent, Mailbox},
    engine::EngineCommand,
    persistence::{Persistence, Role},
    session::{KodaSession, SessionCancel},
    tools::{ToolCatalog, ToolRegistry},
    trust::TrustMode,
};
use koda_test_utils::{Env, MockProvider, MockResponse, TestSink};
use std::sync::Arc;
use tokio::sync::{Mutex as AsyncMutex, mpsc};

/// Build a `KodaSession` wired to the supplied provider, returning
/// the session, its cancellation handle, and a clone of the recorded-
/// calls handle so tests can assert on what the LLM saw.
///
/// Mirrors the pattern in `session_test.rs` — duplicated here rather
/// than imported because cross-test-binary helpers would force a
/// `koda-test-utils` extension just for this one test file.
async fn make_session_with_recorder(
    env: &Env,
    provider: MockProvider,
) -> (
    KodaSession,
    SessionCancel,
    Arc<std::sync::Mutex<Vec<Vec<koda_test_utils::ChatMessage>>>>,
) {
    let cancel = SessionCancel::new();
    let recorded = provider.recorded_calls_handle();

    let tools = ToolRegistry::new(env.root.clone(), env.config.max_context_tokens);
    let agent = Arc::new(KodaAgent {
        project_root: env.root.clone(),
        tools,
        tool_defs: ToolCatalog::new().get_definitions(&[], &[]),
        system_prompt: "You are a test assistant.".to_string(),
        semantic_memory: String::new(),
    });
    agent
        .tools
        .set_session(Arc::new(env.db.clone()), env.session_id.clone());

    let file_tracker =
        koda_core::file_tracker::FileTracker::new(&env.session_id, env.db.clone()).await;

    let (mailbox, mailbox_rx) = Mailbox::new();
    let mailbox = Arc::new(mailbox);
    // Phase 3: pre-register /root → mailbox; mirror what KodaSession::new
    // does. Tests that build sessions by struct-literal still need to
    // honor the substrate invariant (registry contains /root).
    let mailbox_registry = Arc::new(koda_core::agent::MailboxRegistry::new());
    mailbox_registry.register(
        koda_core::agent::AgentPath::root(),
        Arc::clone(&mailbox),
    );
    agent
        .tools
        .set_mailbox_registry(Arc::clone(&mailbox_registry));

    let session = KodaSession {
        id: env.session_id.clone(),
        agent,
        db: env.db.clone(),
        provider: Box::new(provider),
        mode: TrustMode::Auto,
        cancel: cancel.clone(),
        file_tracker,
        title_set: false,
        proxy: None,
        socks5_proxy: None,
        bg_agents: koda_core::child_agent::new_shared(),
        sub_agent_cache: koda_core::sub_agent_cache::SubAgentCache::new(),
        event_forwarder: None,
        mailbox,
        mailbox_rx: Arc::new(AsyncMutex::new(mailbox_rx)),
        idle_pending_input: Arc::new(AsyncMutex::new(Vec::new())),
        mailbox_registry,
    };
    (session, cancel, recorded)
}

fn sample_mail(content: &str) -> InterAgentCommunication {
    InterAgentCommunication {
        author: "/root/peer".parse().unwrap(),
        recipient: AgentPath::root(),
        other_recipients: Vec::new(),
        content: content.to_string(),
        trigger_turn: true,
    }
}

/// **Contract**: mail sent before a turn lands as a `Role::User` row
/// in the DB and appears in the messages handed to the provider.
///
/// This is the load-bearing pin for Phase 2 — if a future refactor
/// drops the drain call, splits the persistence path, or accidentally
/// converts mail to a non-user role, this test fails loudly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mailbox_send_lands_as_user_message_in_next_turn() {
    let env = Env::new().await;
    // No prior user message — the mail itself is the only user input.
    let provider = MockProvider::new(vec![MockResponse::Text("ack".to_string())]);
    let (mut session, _cancel, recorded) = make_session_with_recorder(&env, provider).await;

    // Send mail *before* run_turn — simulates a peer agent posting
    // mail while the recipient session is idle between turns.
    session.mailbox().send(sample_mail("hello from peer"));

    let sink = TestSink::new();
    let (_tx, mut cmd_rx) = mpsc::channel::<EngineCommand>(1);
    session
        .run_turn(&env.config, None, &sink, &mut cmd_rx, None)
        .await
        .expect("turn should succeed");

    // Assertion 1: mail persisted as Role::User.
    let messages = env
        .db
        .load_all_messages(&env.session_id)
        .await
        .expect("load_all_messages");
    let user_msgs: Vec<_> = messages.iter().filter(|m| m.role == Role::User).collect();
    assert_eq!(
        user_msgs.len(),
        1,
        "expected exactly one user message (the drained mail), got {}: {:?}",
        user_msgs.len(),
        user_msgs.iter().map(|m| &m.content).collect::<Vec<_>>()
    );
    let body = user_msgs[0]
        .content
        .as_deref()
        .expect("user message must have content");
    assert!(
        body.contains("hello from peer"),
        "user message must contain mail body, got: {body}"
    );
    assert!(
        body.contains("[mail from /root/peer"),
        "user message must contain the mail header, got: {body}"
    );

    // Assertion 2: the LLM saw the mail in its context.
    let calls = recorded.lock().unwrap().clone();
    assert!(!calls.is_empty(), "provider should have been called");
    let last_call = calls.last().unwrap();
    let saw_mail = last_call.iter().any(|m| {
        m.content
            .as_deref()
            .is_some_and(|c| c.contains("hello from peer"))
    });
    assert!(
        saw_mail,
        "LLM context must include the mail body; messages were: {:?}",
        last_call.iter().map(|m| &m.content).collect::<Vec<_>>()
    );
}

/// **Contract**: `enqueue_for_next_turn` is the "while idle" sibling
/// of the live mailbox drain — same end-state, different producer.
///
/// Pins the FIFO order: idle queue first, then mailbox.  If a Phase 3
/// refactor reverses the order, downstream peer-agent reasoning that
/// expects "earlier-queued mail appears earlier in the LLM context"
/// breaks silently.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_queue_drains_before_live_mailbox_in_same_turn() {
    let env = Env::new().await;
    let provider = MockProvider::new(vec![MockResponse::Text("ack".to_string())]);
    let (mut session, _cancel, _recorded) = make_session_with_recorder(&env, provider).await;

    // Idle queue gets one message; mailbox gets another.
    session
        .enqueue_for_next_turn(sample_mail("idle-first"))
        .await;
    session.mailbox().send(sample_mail("mailbox-second"));

    let sink = TestSink::new();
    let (_tx, mut cmd_rx) = mpsc::channel::<EngineCommand>(1);
    session
        .run_turn(&env.config, None, &sink, &mut cmd_rx, None)
        .await
        .expect("turn should succeed");

    let messages = env
        .db
        .load_all_messages(&env.session_id)
        .await
        .expect("load_all_messages");
    let user_bodies: Vec<String> = messages
        .iter()
        .filter(|m| m.role == Role::User)
        .filter_map(|m| m.content.clone())
        .collect();
    assert_eq!(
        user_bodies.len(),
        2,
        "expected 2 drained mail rows, got {}: {user_bodies:?}",
        user_bodies.len()
    );
    let idle_idx = user_bodies
        .iter()
        .position(|b| b.contains("idle-first"))
        .expect("idle-first must be present");
    let mailbox_idx = user_bodies
        .iter()
        .position(|b| b.contains("mailbox-second"))
        .expect("mailbox-second must be present");
    assert!(
        idle_idx < mailbox_idx,
        "idle queue must drain before live mailbox; got idle={idle_idx}, mailbox={mailbox_idx}"
    );
}

/// **Regression guard**: with no mail at all, the drain must not
/// pollute the conversation with empty user-role rows.
///
/// Cheap pin against a class of bug where "always insert" creeps in
/// during refactors.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_mailbox_does_not_create_user_row() {
    let env = Env::new().await;
    env.insert_user_message("real user input").await;
    let provider = MockProvider::new(vec![MockResponse::Text("ack".to_string())]);
    let (mut session, _cancel, _recorded) = make_session_with_recorder(&env, provider).await;

    let sink = TestSink::new();
    let (_tx, mut cmd_rx) = mpsc::channel::<EngineCommand>(1);
    session
        .run_turn(&env.config, None, &sink, &mut cmd_rx, None)
        .await
        .expect("turn should succeed");

    let messages = env
        .db
        .load_all_messages(&env.session_id)
        .await
        .expect("load_all_messages");
    let user_msgs: Vec<_> = messages.iter().filter(|m| m.role == Role::User).collect();
    assert_eq!(
        user_msgs.len(),
        1,
        "only the real user message should exist; drain must not insert empty rows. \
         Got {} user rows: {:?}",
        user_msgs.len(),
        user_msgs.iter().map(|m| &m.content).collect::<Vec<_>>()
    );
    assert_eq!(
        user_msgs[0].content.as_deref(),
        Some("real user input"),
        "the only user row must be the real one"
    );
}

/// **Phase 3 substrate pin**: every constructed session must have its
/// own mailbox registered at `/root` in the registry, and the entry
/// must point at the same `Mailbox` the session exposes via
/// `mailbox()`. Without this, the LLM-facing `send_message` /
/// `wait_for_mail` tools would have nowhere to route mail.
///
/// This is a load-bearing pin: a regression that breaks the
/// pre-registration silently makes peer tools see an empty registry
/// and respond with "no such recipient" — confusing the LLM, but
/// not failing any non-peer test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_pre_registers_root_in_mailbox_registry() {
    let env = Env::new().await;
    let provider = MockProvider::new(vec![MockResponse::Text("ack".to_string())]);
    let (session, _cancel, _recorded) = make_session_with_recorder(&env, provider).await;

    // Registry has exactly one entry, /root, after construction.
    assert_eq!(
        session.mailbox_registry.len(),
        1,
        "freshly-constructed session must have exactly /root registered"
    );
    let registered = session
        .mailbox_registry
        .get(&koda_core::agent::AgentPath::root())
        .expect("/root must resolve in the registry");

    // Round-trip: send via the registry's Arc<Mailbox>, drain via
    // the session's mailbox_rx — if these aren't the same channel,
    // the receiver sees nothing.
    registered.send(sample_mail("via-registry"));
    let drained = session.mailbox_rx.lock().await.drain();
    assert_eq!(
        drained.len(),
        1,
        "registry's /root entry must be the same channel the session drains"
    );
    assert_eq!(
        drained[0].content, "via-registry",
        "the drained mail must be the one we sent through the registry"
    );
}
