//! What reaches a turn while it runs: a rejection's feedback, a monitor's
//! notice, and its parent's cancel reaching a foreground sub-agent.

use async_trait::async_trait;
use koda_core::config::ModelSettings;
use koda_core::engine::{ApprovalDecision, EngineCommand, EngineEvent, EngineSink, ToolOutcome};
use koda_core::providers::stream_collector::SseCollector;
use koda_core::providers::{ChatMessage, LlmProvider, LlmResponse, ModelInfo, ToolDefinition};
use koda_core::session::KodaSession;
use koda_core::tools::bg_process::BgRegistry;
use koda_core::tools::{ToolCatalog, ToolRegistry};
use koda_core::trust::TrustMode;
use koda_test_utils::network::{FakeLlmServer, ResponseTemplate};
use koda_test_utils::{Env, MockProvider, MockResponse, TestSink};
use serde_json::json;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const WAIT: Duration = Duration::from_secs(20);

/// The id of the one call in a `MockResponse::tool_call`.
fn call_id(response: &MockResponse) -> String {
    match response {
        MockResponse::ToolCalls(calls) => calls[0].id.clone(),
        other => panic!("not a tool call: {other:?}"),
    }
}

/// The messages of the request whose text mentions `needle`.
fn mentioning<'a>(request: &'a [ChatMessage], needle: &str) -> Vec<&'a ChatMessage> {
    request
        .iter()
        .filter(|m| m.content.as_deref().is_some_and(|c| c.contains(needle)))
        .collect()
}

// ── 1. Reject with feedback ─────────────────────────────────────────────

/// A user who answers every prompt with the same decision.
struct User {
    events: TestSink,
    cmd_tx: mpsc::Sender<EngineCommand>,
    decision: ApprovalDecision,
    prompts: Mutex<Vec<String>>,
}

impl EngineSink for User {
    fn emit(&self, event: EngineEvent) {
        if let EngineEvent::ApprovalRequest { id, detail, .. } = &event {
            self.prompts.lock().unwrap().push(detail.clone());
            self.cmd_tx
                .try_send(EngineCommand::ApprovalResponse {
                    id: id.clone(),
                    decision: self.decision.clone(),
                })
                .unwrap();
        }
        self.events.emit(event);
    }
}

/// When the user turns a call down with feedback, the call doesn't run and
/// the model's next request carries the feedback in that call's result, so
/// the model can act on what the user said instead of guessing why. The
/// model asks for `rm -f keep-me.txt` (destructive, so it asks even in
/// Auto); the user rejects it saying to rename the file instead. A
/// dispatcher that ran the call anyway would delete the file, and one that
/// treated the answer as a bare rejection would send "User rejected this
/// action." without the feedback.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejection_with_feedback_skips_the_call_and_tells_the_model_why() {
    const FEEDBACK: &str = "rename it to keep-me.bak instead of deleting it";
    let env = Env::new().await;
    std::fs::write(env.root.join("keep-me.txt"), "precious").unwrap();

    let rm = MockResponse::tool_call("Bash", json!({"command": "rm -f keep-me.txt"}));
    let rm_id = call_id(&rm);
    let provider = MockProvider::new(vec![rm, MockResponse::Text("understood".into())]);
    let calls = provider.recorded_calls_handle();
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
    session.provider = Box::new(provider) as Box<dyn LlmProvider>;

    env.insert_user_message("clean up keep-me.txt").await;
    let (cmd_tx, mut cmd_rx) = mpsc::channel(8);
    let user = User {
        events: TestSink::new(),
        cmd_tx,
        decision: ApprovalDecision::RejectWithFeedback {
            feedback: FEEDBACK.into(),
        },
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
    assert_eq!(prompts.len(), 1, "one prompt, for the rm: {prompts:?}");
    assert_eq!(
        std::fs::read_to_string(env.root.join("keep-me.txt")).ok(),
        Some("precious".into()),
        "the rejected command ran"
    );

    let calls = calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2, "the model is asked once more, after the no");
    let results: Vec<_> = calls[1]
        .iter()
        .filter(|m| m.role == "tool" && m.tool_call_id.as_deref() == Some(rm_id.as_str()))
        .collect();
    assert_eq!(
        results.len(),
        1,
        "one result for the rm call: {:#?}",
        calls[1]
    );
    let result = results[0].content.as_deref().unwrap_or_default();
    assert!(
        result.contains("rejected") && result.contains(FEEDBACK),
        "the rm call's result should say the user rejected it, with the feedback: {result:?}"
    );
    let outcome = user.events.events().into_iter().find_map(|e| match e {
        EngineEvent::ToolCallResult { id, outcome, .. } if id == rm_id => Some(outcome),
        _ => None,
    });
    assert_eq!(outcome, Some(ToolOutcome::Declined));
}

// ── 2. A Monitor notice ─────────────────────────────────────────────────

/// The line the monitor's command prints.
const LINE: &str = "build-finished-7f3a";

/// A scripted model whose second step, unless the monitor's notice is
/// already in its request, waits until the monitor has ended and its
/// notice is due, as a real model's next step would come later. The
/// notice is then taken just before the step after.
struct AfterTheMonitor<'a> {
    script: MockProvider,
    bg: &'a BgRegistry,
    steps: AtomicUsize,
}

impl AfterTheMonitor<'_> {
    async fn step(&self, messages: &[ChatMessage]) {
        if self.steps.fetch_add(1, Ordering::SeqCst) != 1 || !mentioning(messages, LINE).is_empty()
        {
            return;
        }
        let start = Instant::now();
        while !(self.bg.running_monitors() == 0 && self.bg.has_monitor_notices()) {
            assert!(
                start.elapsed() < WAIT / 2,
                "the monitor's notice never came due"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

#[async_trait]
impl LlmProvider for AfterTheMonitor<'_> {
    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        settings: &ModelSettings,
    ) -> anyhow::Result<LlmResponse> {
        self.step(messages).await;
        self.script.chat(messages, tools, settings).await
    }

    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        settings: &ModelSettings,
    ) -> anyhow::Result<SseCollector> {
        self.step(messages).await;
        self.script.chat_stream(messages, tools, settings).await
    }

    async fn list_models(&self) -> anyhow::Result<Vec<ModelInfo>> {
        self.script.list_models().await
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

/// A line a monitor's command prints reaches the model as a notice at a
/// later step, with how the command ended, and only once. The model starts
/// a Monitor whose command prints one line and exits, reads a file while
/// the line comes in, reads another, and finishes. The notice lands in the
/// second or the third request, depending on whether the command had
/// exited when the second began. The line is built by `printf` so it
/// appears nowhere in the call or its result: the only way into a request
/// is the notice. If the loop stopped taking monitor notices, the line
/// would never arrive; if a notice weren't marked told, it would be
/// injected again at the next step.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_monitor_line_reaches_the_model_once_with_how_it_ended() {
    let env = Env::new().await;
    std::fs::write(env.root.join("notes.txt"), "notes").unwrap();
    let read = || MockResponse::tool_call("Read", json!({"file_path": "notes.txt"}));
    let provider = AfterTheMonitor {
        script: MockProvider::new(vec![
            MockResponse::tool_call(
                "Monitor",
                json!({
                    "command": "printf 'build-%s\\n' finished-7f3a",
                    "description": "the build",
                }),
            ),
            read(),
            read(),
            MockResponse::Text("the build finished".into()),
        ]),
        bg: &env.tools.bg_registry,
        steps: AtomicUsize::new(0),
    };
    env.insert_user_message("watch the build").await;
    let events = tokio::time::timeout(WAIT, env.run_inference_dyn(&provider))
        .await
        .expect("the turn did not end");

    let calls = provider.script.recorded_calls();
    assert_eq!(calls.len(), 4, "four steps");
    let first = calls
        .iter()
        .position(|request| !mentioning(request, LINE).is_empty())
        .unwrap_or_else(|| panic!("the line never reached the model: {:#?}", calls[3]));
    assert!(
        (1..=2).contains(&first),
        "the line should arrive at the step after it was printed, or the one after the \
         wait, not at step {}",
        first + 1
    );
    // Once in, the notice stays in the history as one message, and no
    // second notice for the same monitor follows it.
    for (step, request) in calls.iter().enumerate().skip(first) {
        let notices: Vec<_> = request
            .iter()
            .filter(|m| {
                m.content
                    .as_deref()
                    .is_some_and(|c| c.contains("<monitor_events"))
            })
            .collect();
        assert_eq!(
            notices.len(),
            1,
            "request {} should carry one notice: {request:#?}",
            step + 1
        );
        let notice = notices[0].content.as_deref().unwrap();
        assert_eq!(notices[0].role, "user", "a notice is a user message");
        assert!(
            notice.contains("description=\"the build\"") && notice.matches(LINE).count() == 1,
            "the line should come once, as the monitor's event: {notice}"
        );
        assert!(
            notice.contains("its command exited with code 0"),
            "the notice should say how the watch ended: {notice}"
        );
    }
    let shown: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::Info { message } if message.contains("Monitor:") => Some(message.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        shown,
        ["  Monitor: 1 new line from 1 monitor"],
        "the client is told once"
    );
    assert!(!env.tools.bg_registry.has_notices(), "nothing left owed");
}

// ── 3. A foreground sub-agent cancelled mid-run ─────────────────────────

/// Cancelling the parent's turn while a foreground sub-agent waits on its
/// model ends the sub-agent's call at once: the sub-agent's result says the
/// parent cancelled it, it makes no further model call, and the parent's
/// turn ends interrupted without asking its own model again. The
/// sub-agent's model never answers its first request, so a sub-agent that
/// didn't stop on the cancel would hold the turn past the timeout; one that
/// went on after it would send a second request, which is answered so its
/// result would read "kept going" instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_the_turn_stops_a_foreground_sub_agent_mid_call() {
    let env = Env::new().await;
    let server = FakeLlmServer::spawn().await;
    let agents_dir = env.root.join(".koda/agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join("slow-agent.json"),
        json!({
            "name": "slow-agent",
            "system_prompt": "You are a slow agent.",
            "allowed_tools": [],
            "provider": "lmstudio",
            "model": "stub",
            "base_url": server.url()
        })
        .to_string(),
    )
    .unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&requests);
    server
        .mount_fn("POST", r".*/chat/completions$", move |_| {
            let reply = json!({
                "choices": [{"index": 0, "finish_reason": "stop",
                    "message": {"role": "assistant", "content": "kept going"}}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
            });
            let first = counted.fetch_add(1, Ordering::SeqCst) == 0;
            let template = ResponseTemplate::new(200).set_body_json(reply);
            if first {
                // Longer than the test waits: only the cancel ends it.
                template.set_delay(Duration::from_secs(120))
            } else {
                template
            }
        })
        .await;

    env.insert_user_message("ask slow-agent").await;
    let invoke = MockResponse::tool_call(
        "InvokeAgent",
        json!({"agent_name": "slow-agent", "prompt": "take your time"}),
    );
    let invoke_id = call_id(&invoke);
    let provider = MockProvider::new(vec![
        invoke,
        MockResponse::Text("the parent went on".into()),
    ]);
    let cancel = CancellationToken::new();
    let cancel_when_asked = async {
        let start = Instant::now();
        while requests.load(Ordering::SeqCst) == 0 {
            assert!(
                start.elapsed() < WAIT,
                "the sub-agent never called its model"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        cancel.cancel();
    };
    let ((result, events), ()) = tokio::time::timeout(WAIT, async {
        tokio::join!(
            env.run_inference_cancellable(&provider, cancel.clone()),
            cancel_when_asked
        )
    })
    .await
    .expect("the turn did not end after the cancel: the sub-agent's model call held it");
    result.unwrap();

    let output = events.iter().find_map(|e| match e {
        EngineEvent::ToolCallResult {
            id, name, output, ..
        } if id == &invoke_id && name == "InvokeAgent" => Some(output.clone()),
        _ => None,
    });
    let output = output.unwrap_or_else(|| panic!("no InvokeAgent result: {events:#?}"));
    assert!(
        output.contains("[cancelled by parent]"),
        "the sub-agent's result should say the parent cancelled it: {output:?}"
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "the sub-agent called its model again after the cancel"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::Warn { message } if message == "Interrupted")),
        "the parent's turn should end interrupted: {events:#?}"
    );
    assert_eq!(
        provider.recorded_calls().len(),
        1,
        "the parent asked its model again after the cancel"
    );
}
