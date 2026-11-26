//! One-shot runs: one prompt, one turn, then exit: `koda exec`. While the
//! model's background commands run, the run stays open for the turns the
//! session starts when they end, as `claude -p` does.
//!
//! The run goes through the client API, as every Koda front end will: it
//! starts Koda's host in process, opens a session whose requests Koda
//! answers itself (`approvalPolicy: never`), starts one turn and reads what
//! comes back. Koda approves what the mode allows, refuses destructive
//! actions and anything outside the sandbox, answers a question with nothing
//! and keeps going at the loop cap. Ctrl+C sends `turn/interrupt`. Exit code
//! 0 is success, 1 failure.
//!
//! ## Output
//!
//! - [`Output::Text`] (`koda exec`): the streamed reply on stdout,
//!   progress (tool calls, footer) on stderr, drawn from the protocol's
//!   notifications, as `codex exec`'s human output is.
//! - [`Output::Events`] (`koda exec --json`): stdout carries only JSON-RPC
//!   notifications from the client protocol (`koda-protocol`), one per
//!   line, as the host sends them: `session/started`, the turn and its
//!   items, each server request followed by `serverRequest/resolved` with
//!   the answer Koda took, and `exec/summary` last. Deltas only with
//!   `--json-deltas`.

use koda_client::events::Outgoing;
use koda_client::protocol::background::BackgroundTerminalsListParams;
use koda_client::protocol::goal::GoalStatus;
use koda_client::protocol::initialize::ClientInfo;
use koda_client::protocol::item::{
    FileUpdateChange, Item, OutputStream, PatchChangeKind, ToolCallStatus, ToolEffect,
};
use koda_client::protocol::mcp::{McpServerStartupState, McpServerStatusUpdatedNotification};
use koda_client::protocol::server::ExecSummary;
use koda_client::protocol::session::{
    ApprovalPolicy, SessionForkParams, SessionResumeParams, SessionStartParams, TokenUsage,
};
use koda_client::protocol::turn::{
    ImageInput, Mention, PlanStep, Turn, TurnInterruptParams, TurnStartParams, TurnStatus, Usage,
};
use koda_client::protocol::{ServerNotification, ServerRequest, methods};
use koda_client::{InProcessClient, StartArgs};
use koda_tui::input;
use koda_tui::protocol_text::{self, Tone};

use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

/// What a one-shot run writes to stdout. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// The reply as text.
    Text,
    /// The protocol's notifications, deltas included or not.
    Events {
        /// Whether to print `item/*/delta` notifications too.
        deltas: bool,
    },
}

/// One one-shot run.
#[derive(Debug, Clone)]
pub struct Run {
    /// The prompt.
    pub prompt: String,
    /// Images `--image` attached, read already.
    pub images: Vec<input::ImageData>,
    /// What goes to stdout.
    pub output: Output,
    /// Where to write the last reply (`koda exec -o`).
    pub last_message: Option<PathBuf>,
    /// The JSON Schema the last reply must match (`--output-schema`).
    pub output_schema: Option<serde_json::Value>,
    /// What `--provider`, `--base-url` and `--model` named (a provider by
    /// its id or a name for it, a model by alias or id). The host fills in the rest: a new
    /// session from the saved choice, a resumed one from its own, as
    /// `codex exec` does.
    pub named: Named,
}

/// What the command line named, each optional.
#[derive(Debug, Clone, Default)]
pub struct Named {
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
}

/// Run a single prompt and exit. `resume` is an earlier session's id, or
/// the start of one; without one a new session starts. Returns the process exit code (0 =
/// success).
pub async fn run_headless(
    project_root: PathBuf,
    resume: Option<koda_tui::session_id::Resume>,
    fork: bool,
    run: Run,
    on_session: Option<koda_tui::tui_app::SessionHook>,
) -> Result<i32> {
    // `@path` mentions go to the host, which reads them; dragged-in images
    // are loaded here.
    let processed = input::process_input(&run.prompt);
    let user_message = processed.prompt.clone();
    let mentions: Vec<Mention> = processed
        .mentions
        .iter()
        .map(|path| Mention { path: path.clone() })
        .collect();
    let images = run
        .images
        .iter()
        .cloned()
        .chain(processed.images)
        .map(|image| ImageInput {
            media_type: image.media_type,
            data: image.base64,
        })
        .collect();

    let mut client = InProcessClient::start(StartArgs {
        config_file: koda_tui::user_config::file()?,
        home: koda_config::config_dir()?,
        project_root,
        client_info: ClientInfo {
            name: "koda-exec".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            title: None,
        },
    })
    .await?;
    let mut view = View::new(run.output);
    // A mistyped id (once, a llama-server pid) stops here, before the turn.
    let resume = match &resume {
        Some(resume) => Some(koda_tui::session_id::resolve_target(&client, resume).await?),
        None => None,
    };

    // A provider goes by its id; `--provider` takes its name too.
    let provider = match run.named.provider.as_deref() {
        Some(typed) => Some(
            koda_tui::providers::Providers::fetch(&client)
                .await
                .id_for(typed),
        ),
        None => None,
    };
    let opened = match (fork, resume) {
        // `koda exec fork`: a copy of the session, opened, as `codex exec
        // fork` asks for `thread/fork`; flags override the copy's model.
        (true, Some(source)) => {
            client
                .request::<methods::SessionFork>(SessionForkParams {
                    session_id: source,
                    approval_policy: Some(ApprovalPolicy::Never),
                    model_provider: provider,
                    base_url: run.named.base_url.clone(),
                    model: run.named.model.clone(),
                    ..Default::default()
                })
                .await?
                .session
        }
        // `koda exec resume`, Codex's `thread/resume`: given flags win;
        // without them the session keeps its own model, and always its
        // provider.
        (_, Some(session_id)) => {
            client
                .request::<methods::SessionResume>(SessionResumeParams {
                    session_id,
                    base_url: run.named.base_url.clone(),
                    model: run.named.model.clone(),
                    approval_policy: Some(ApprovalPolicy::Never),
                    ..Default::default()
                })
                .await?
                .session
        }
        // A new session starts from the config, flags over it.
        (_, None) => {
            client
                .request::<methods::SessionStart>(SessionStartParams {
                    model_provider: provider,
                    base_url: run.named.base_url.clone(),
                    model: run.named.model.clone(),
                    approval_policy: Some(ApprovalPolicy::Never),
                    ..Default::default()
                })
                .await?
                .session
        }
    };
    let session_id = opened.id;
    if let Some(on_session) = on_session {
        on_session(&session_id);
    }
    // `koda exec fork` without a prompt: the fork is all there is to do.
    if fork && run.prompt.is_empty() {
        while let Some(message) = client.try_next_event() {
            view.before_turn(&message);
        }
        if let Output::Text = run.output {
            println!("{session_id}");
        }
        client.shutdown().await;
        return Ok(0);
    }
    // An open session always says what it runs.
    let model = opened.model.unwrap_or_default();
    let provider = opened.model_provider.unwrap_or_default();
    // What starting the session said: `session/started`, and the context
    // window Koda assumed when the server didn't report one.
    while let Some(message) = client.try_next_event() {
        view.before_turn(&message);
    }

    // From here a Ctrl+C asks the turn to stop instead of killing Koda.
    let mut ctrl_c = CtrlC::listen()?;
    let started = Instant::now();
    // `/goal <objective>` sets the session's goal instead of sending a
    // message, and the run follows the goal's turns until it's no longer
    // active, as `claude -p "/goal …"` runs the loop to completion.
    let goal = goal_objective(&run.prompt);
    let first_turn = match goal {
        Some(objective) => {
            set_goal(&client, &session_id, objective).await?;
            None
        }
        None => Some(
            client
                .request::<methods::TurnStart>(TurnStartParams {
                    session_id: session_id.clone(),
                    input: user_message,
                    images,
                    mentions,
                    skills: Vec::new(),
                    client_user_message_id: None,
                    mode: None,
                    output_schema: run.output_schema.clone(),
                })
                .await?
                .turn,
        ),
    };
    let requests = client.request_handle();
    let mut interrupt = None;
    // The turn a Ctrl+C stops: this run's, then each one the session starts
    // for its goal or its background commands. `None` while waiting.
    let mut running = first_turn.map(|turn| turn.id);
    let mut last: Option<Turn> = None;
    // Whether the goal set above is still being worked on.
    let mut goal_active = goal.is_some();
    let completed: Turn = loop {
        tokio::select! {
            message = client.next_event() => match message {
                Some(message) => {
                    if let Outgoing::Notification(ServerNotification::TurnStarted(started)) =
                        &message
                    {
                        running = Some(started.turn.id.clone());
                    }
                    let goal_ended = match &message {
                        Outgoing::Notification(ServerNotification::SessionGoalUpdated(update))
                            if update.session_id == session_id =>
                        {
                            goal_active = update.goal.status == GoalStatus::Active;
                            !goal_active
                        }
                        // Koda stopped starting the goal's turns: nobody is
                        // here to send the next message, so the run ends.
                        Outgoing::Notification(ServerNotification::SessionGoalHeld(held))
                            if held.session_id == session_id =>
                        {
                            goal_active = false;
                            true
                        }
                        _ => false,
                    };
                    let Some(done) = view.protocol(message) else {
                        // A goal stopped between turns (blocked, its budget
                        // used up) starts no turn: stop waiting for one,
                        // unless background commands still will.
                        if goal_ended && running.is_none() && last.is_some() {
                            let listed = background(&client, &session_id).await?;
                            if !listed.waiting {
                                break last.take().context("no turn to end the run with")?;
                            }
                        }
                        continue;
                    };
                    running = None;
                    if interrupt.is_some() {
                        break done;
                    }
                    // As `claude -p` does, the run stays open while the
                    // goal is active, or the model's background commands
                    // run: each exit, or its time limit, starts a turn that
                    // tells the model.
                    //
                    // Whether a goal goes on is the host's call, as in the
                    // TUI, whatever the turn's status: a turn
                    // `turn_token_budget` stopped leaves it active, and the
                    // host starts the next turn, as Hermes and Codex go on
                    // after a turn cut short by a limit. A failed turn
                    // blocks it, and that update ends the run.
                    if goal.is_some() {
                        goal_active = goal_status(&client, &session_id).await?
                            == Some(GoalStatus::Active);
                    }
                    if goal_active {
                        last = Some(done);
                        continue;
                    }
                    if done.status != TurnStatus::Completed {
                        break done;
                    }
                    let listed = background(&client, &session_id).await?;
                    if !listed.waiting {
                        break done;
                    }
                    if let Output::Text = run.output {
                        let work: Vec<String> = listed
                            .agents
                            .iter()
                            .map(|a| format!("agent {} ({})", a.agent_id, a.agent_name))
                            .chain(listed.data.iter().map(|t| t.command.clone()))
                            .collect();
                        eprintln!(
                            "\x1b[2mWaiting for background work to finish: {}\x1b[0m",
                            work.join(", ")
                        );
                    }
                    last = Some(done);
                }
                None => anyhow::bail!("Koda's host stopped during the turn"),
            },
            () = ctrl_c.recv(), if interrupt.is_none() => {
                eprintln!("\n\x1b[33m\u{26a0} Interrupted\x1b[0m");
                // Waiting between turns: stop waiting. The background
                // commands stop with the session.
                let Some(turn_id) = running.clone() else {
                    break last.take().context("no turn to end the run with")?;
                };
                // Answered once the turn has stopped. The turn bounds its
                // own wind-down, so this can't hang the run.
                let requests = requests.clone();
                let params = TurnInterruptParams {
                    session_id: session_id.clone(),
                    turn_id,
                };
                interrupt = Some(tokio::spawn(async move {
                    requests.request::<methods::TurnInterrupt>(params).await
                }));
            }
        }
    };
    // What the host sent before it answered the last check, such as the
    // goal's final update, still belongs to the run.
    while let Some(message) = client.try_next_event() {
        view.protocol(message);
    }
    let elapsed_ms = started.elapsed().as_millis() as u64;

    let response = view.stats.last_reply.clone().unwrap_or_default();
    if let Some(path) = &run.last_message
        && let Err(e) = std::fs::write(path, &response)
    {
        eprintln!(
            "Error: couldn't write the last message to {}: {e}",
            path.display()
        );
    }

    // A turn that returned an error, such as one dropped where it stood
    // after a cancel, says so on `turn/completed`.
    let result: Result<()> = match &completed.error {
        Some(error) if completed.status != TurnStatus::Failed => {
            Err(anyhow::anyhow!(error.message.clone()))
        }
        _ => Ok(()),
    };
    // A completed run whose reply doesn't match `--output-schema` fails.
    if let Some(schema) = &run.output_schema
        && result.is_ok()
        && run_failure(&result, &view.stats).is_none()
    {
        view.stats.schema_mismatch = crate::output_schema::mismatch(schema, &response)
            .map(|why| format!("the last reply doesn't match --output-schema: {why}"));
        if let Some(why) = &view.stats.schema_mismatch {
            eprintln!("Error: {why}");
        }
    }
    if let Output::Events { .. } = run.output {
        // Usage is the host's latest count, sent after every model call,
        // so it holds for failed and cancelled turns too.
        let outcome = RunOutcome {
            result: &result,
            session_id: &session_id,
            provider: &provider,
            model: &model,
            elapsed_ms,
            stats: &view.stats,
        };
        view.print(&Outgoing::Notification(ServerNotification::ExecSummary(
            exec_summary(outcome),
        )));
    }
    client.shutdown().await;

    match result {
        // A failed turn has said why (above); just fail the exit code.
        Ok(()) => Ok(i32::from(run_failure(&Ok(()), &view.stats).is_some())),
        Err(e) => {
            eprintln!("Error: {e}");
            Ok(1)
        }
    }
}

/// Ctrl+C, listened for from the moment it is made, so a signal that comes
/// before the first wait is kept instead of killing the process.
struct CtrlC {
    #[cfg(unix)]
    signal: tokio::signal::unix::Signal,
}

impl CtrlC {
    fn listen() -> Result<Self> {
        Ok(Self {
            #[cfg(unix)]
            signal: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .context("couldn't listen for Ctrl+C")?,
        })
    }

    async fn recv(&mut self) {
        #[cfg(unix)]
        {
            self.signal.recv().await;
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

// ---------------------------------------------------------------------------
// Run summary: `exec/summary`
// ---------------------------------------------------------------------------

/// Per-run counters, from the turn's notifications.
#[derive(Debug, Clone, Default, PartialEq)]
struct RunStats {
    /// Top-level tool calls by tool name.
    tool_calls: BTreeMap<String, u64>,
    /// Tool calls made by sub-agents, by tool name.
    sub_agent_tool_calls: BTreeMap<String, u64>,
    /// How the turn ended, and why if it failed.
    turn_end: Option<(TurnStatus, Option<String>)>,
    /// Why the last reply doesn't match `--output-schema`, if it doesn't.
    schema_mismatch: Option<String>,
    /// The session's latest token counts (`session/tokenUsage/updated`),
    /// by agent: everything since it was created, a resumed session's
    /// earlier turns and sub-agents included.
    usage: TokenUsage,
    /// The last reply of the session's own, as `codex exec` takes its
    /// final agent message: none unless the turn completed.
    last_reply: Option<String>,
}

/// Everything a summary needs once the turn is over.
struct RunOutcome<'a> {
    result: &'a Result<()>,
    session_id: &'a str,
    provider: &'a str,
    model: &'a str,
    elapsed_ms: u64,
    stats: &'a RunStats,
}

/// What a failed turn says: its error, or that it failed.
fn failed_turn_message(turn: &Turn) -> String {
    turn.error
        .as_ref()
        .map(|e| e.message.clone())
        .unwrap_or_else(|| "the turn failed".to_string())
}

/// Why the run failed, if it did. A turn the engine cut short on a warning
/// (provider server error, image rejection, dropped connection) still
/// returns, but ends `failed`, so it counts too.
fn run_failure(result: &Result<()>, stats: &RunStats) -> Option<String> {
    match (result, &stats.turn_end) {
        (Err(e), _) => Some(format!("{e:#}")),
        (Ok(()), Some((TurnStatus::Failed, error))) => Some(
            error
                .clone()
                .unwrap_or_else(|| "the turn failed".to_string()),
        ),
        (Ok(()), _) => stats.schema_mismatch.clone(),
    }
}

/// `koda exec --json`'s last line.
fn exec_summary(outcome: RunOutcome<'_>) -> ExecSummary {
    let failure = run_failure(outcome.result, outcome.stats);
    let by_agent = &outcome.stats.usage.by_agent;
    let mut usage = Usage::default();
    for agent in by_agent.values() {
        usage.input_tokens += agent.input_tokens;
        usage.cached_input_tokens += agent.cached_input_tokens;
        usage.cache_write_input_tokens += agent.cache_write_input_tokens;
        usage.output_tokens += agent.output_tokens;
        usage.reasoning_output_tokens += agent.reasoning_output_tokens;
        usage.model_calls += agent.model_calls;
        usage.web_search_requests += agent.web_search_requests;
    }
    ExecSummary {
        session_id: outcome.session_id.to_string(),
        success: failure.is_none(),
        error: failure,
        model_provider: outcome.provider.to_string(),
        model: outcome.model.to_string(),
        response: outcome.stats.last_reply.clone().unwrap_or_default(),
        turn_status: outcome.stats.turn_end.as_ref().map(|(status, _)| *status),
        duration_ms: outcome.elapsed_ms,
        usage,
        usage_by_agent: by_agent.clone(),
        tool_calls: outcome.stats.tool_calls.clone(),
        sub_agent_tool_calls: outcome.stats.sub_agent_tool_calls.clone(),
    }
}

// ---------------------------------------------------------------------------
// View — what the run prints, and the counts its summary reports
// ---------------------------------------------------------------------------

/// The objective of a `/goal <objective>` prompt.
fn goal_objective(prompt: &str) -> Option<&str> {
    prompt
        .trim()
        .strip_prefix("/goal ")
        .map(str::trim)
        .filter(|objective| !objective.is_empty())
}

/// Set the session's goal to `objective`, replacing any it has, as the
/// TUI's `/goal <objective>` does; the host then starts its turns.
async fn set_goal(client: &InProcessClient, session_id: &str, objective: &str) -> Result<()> {
    use koda_client::protocol::goal::{SessionGoalClearParams, SessionGoalSetParams};
    client
        .request::<methods::SessionGoalClear>(SessionGoalClearParams {
            session_id: session_id.to_string(),
        })
        .await?;
    client
        .request::<methods::SessionGoalSet>(SessionGoalSetParams {
            session_id: session_id.to_string(),
            objective: Some(objective.to_string()),
            status: Some(GoalStatus::Active),
            token_budget: None,
        })
        .await?;
    Ok(())
}

/// The session's goal's status, if it has one.
async fn goal_status(client: &InProcessClient, session_id: &str) -> Result<Option<GoalStatus>> {
    use koda_client::protocol::goal::SessionGoalGetParams;
    let answer = client
        .request::<methods::SessionGoalGet>(SessionGoalGetParams {
            session_id: session_id.to_string(),
        })
        .await?;
    Ok(answer.goal.map(|goal| goal.status))
}

/// The session's background commands, and whether a turn will start for them.
async fn background(
    client: &InProcessClient,
    session_id: &str,
) -> Result<koda_client::protocol::background::BackgroundTerminalsListResponse> {
    Ok(client
        .request::<methods::SessionBackgroundTerminalsList>(BackgroundTerminalsListParams {
            session_id: session_id.to_string(),
        })
        .await?)
}

/// Lines from the start of a new file shown in its diff.
const NEW_FILE_LINES: usize = 60;

struct View {
    output: Output,
    stats: RunStats,
    /// The plan as last sent, to say how the next one changed it.
    plan: Vec<PlanStep>,
    /// Calls Koda was asked about: one declined after that was refused, not
    /// blocked.
    asked: HashSet<String>,
    /// Characters of the reply, for a footer when the provider reports no
    /// output tokens.
    reply_chars: usize,
    /// `InvokeAgent` calls whose agent isn't named yet: named once it shows
    /// it started (a call of its own, a notice, or an answer), so a call
    /// refused before it started names none.
    agents: std::collections::HashMap<String, (String, &'static str)>,
    /// Whether an error that ends the turn was printed, so a failed turn's
    /// own error isn't printed twice (the TUI's `error_shown`).
    error_shown: bool,
}

impl View {
    fn new(output: Output) -> Self {
        Self {
            output,
            stats: RunStats::default(),
            plan: Vec::new(),
            asked: HashSet::new(),
            reply_chars: 0,
            agents: std::collections::HashMap::new(),
            error_shown: false,
        }
    }

    /// Write one protocol message to stdout. `println!` goes through Rust's
    /// line-buffered stdout, so it reaches a piped reader as soon as it ends.
    fn print(&self, message: &Outgoing) {
        if let Output::Events { deltas: false } = self.output
            && message.is_delta()
        {
            return;
        }
        // For a client that lists sessions; a run has the one, and its
        // turn's start and end say the same.
        if matches!(
            message,
            Outgoing::Notification(ServerNotification::SessionStatusChanged(_))
        ) {
            return;
        }
        match message.to_json() {
            Ok(text) => println!("{text}"),
            Err(e) => tracing::warn!("exec --json: failed to serialize a message: {e}"),
        }
    }

    /// What starting the session said. Text shows its warnings on stderr,
    /// like every other notice: stdout carries only the answer.
    fn before_turn(&mut self, message: &Outgoing) {
        // A resumed session's stored counts come as it opens.
        self.record(message);
        match (self.output, message) {
            (Output::Events { .. }, _) => self.print(message),
            (Output::Text, Outgoing::Notification(ServerNotification::Warning(w))) => {
                eprintln!("\x1b[33m  \u{26a0} {}\x1b[0m", w.message);
            }
            (Output::Text, Outgoing::Notification(ServerNotification::ConfigWarning(w))) => {
                eprintln!("\x1b[33m  \u{26a0} {}\x1b[0m", w.summary);
            }
            (
                Output::Text,
                Outgoing::Notification(ServerNotification::McpServerStatusUpdated(update)),
            ) => mcp_unavailable(update),
            (Output::Text, _) => {}
        }
    }

    /// One protocol message during the turn: counted for the summary, and
    /// printed as JSON or drawn as text. Returns the turn once it has
    /// completed.
    fn protocol(&mut self, message: Outgoing) -> Option<Turn> {
        self.record(&message);
        match self.output {
            Output::Events { .. } => self.print(&message),
            Output::Text => self.render(&message),
        }
        match message {
            Outgoing::Notification(ServerNotification::TurnCompleted(done)) => Some(done.turn),
            _ => None,
        }
    }

    fn record(&mut self, message: &Outgoing) {
        match message {
            Outgoing::Notification(ServerNotification::ItemStarted(started)) => {
                if let Item::ToolCall { tool, .. } = &started.item {
                    let counts = if started.parent_item_id.is_some() {
                        &mut self.stats.sub_agent_tool_calls
                    } else {
                        &mut self.stats.tool_calls
                    };
                    *counts.entry(tool.clone()).or_default() += 1;
                }
            }
            Outgoing::Notification(ServerNotification::SessionTokenUsageUpdated(updated)) => {
                self.stats.usage = updated.token_usage.clone();
            }
            Outgoing::Notification(ServerNotification::ItemCompleted(done))
                if done.parent_item_id.is_none() =>
            {
                if let Item::AgentMessage { text, .. } = &done.item {
                    self.stats.last_reply = Some(text.clone());
                }
            }
            Outgoing::Notification(ServerNotification::TurnCompleted(done)) => {
                // Only a completed turn has a final reply, as in `codex
                // exec`: a failed one's partial reply was discarded.
                if done.turn.status != TurnStatus::Completed {
                    self.stats.last_reply = None;
                }
                self.stats.turn_end = Some((
                    done.turn.status,
                    done.turn.error.as_ref().map(|e| e.message.clone()),
                ));
            }
            _ => {}
        }
    }

    /// The text output. The streamed reply goes to stdout; everything else
    /// to stderr.
    fn render(&mut self, message: &Outgoing) {
        let notification = match message {
            Outgoing::Request(ServerRequest::ToolCallApproval { params, .. }) => {
                self.asked.insert(params.item_id.clone());
                // Koda refuses these itself when nobody is there to ask.
                if params.effect == ToolEffect::Destructive {
                    eprintln!(
                        "\x1b[31m  \u{2717} Rejected destructive action: {} \u{2014} {}\x1b[0m",
                        params.tool, params.detail
                    );
                }
                return;
            }
            Outgoing::Request(ServerRequest::UserInput { params, .. }) => {
                eprintln!(
                    "[koda] AskUser (no interactive session): {}",
                    params.question
                );
                return;
            }
            Outgoing::Request(ServerRequest::LoopCap { .. }) => return,
            // The host declines these itself when nobody is there to ask.
            Outgoing::Request(ServerRequest::McpServerElicitation { params, .. }) => {
                eprintln!(
                    "[koda] {} asked for input (no interactive session): {}",
                    params.server_name, params.message
                );
                return;
            }
            Outgoing::Notification(notification) => notification,
            _ => return,
        };
        match notification {
            ServerNotification::AgentMessageDelta(delta) => {
                self.reply_chars += delta.delta.chars().count();
                print!("{}", delta.delta);
                let _ = std::io::stdout().flush();
            }
            ServerNotification::ItemStarted(started) => {
                if let Some(parent) = &started.parent_item_id {
                    self.name_agent(parent);
                }
                let indent = if started.parent_item_id.is_some() {
                    "    "
                } else {
                    "  "
                };
                match &started.item {
                    Item::Reasoning { .. } => {
                        eprintln!("\x1b[90m  \u{1f4ad} thinking...\x1b[0m");
                    }
                    Item::ToolCall {
                        id,
                        tool,
                        arguments,
                        ..
                    } => {
                        eprintln!("\x1b[36m{indent}\u{26a1} {tool}\x1b[0m");
                        if tool == "InvokeAgent"
                            && let Some(agent) = arguments["agent_name"].as_str()
                        {
                            self.agents.insert(id.clone(), (agent.to_string(), indent));
                        }
                    }
                    item @ Item::ContextCompaction { .. } => {
                        if let Some(line) = protocol_text::compaction_started(item) {
                            notice(line);
                        }
                    }
                    _ => {}
                }
            }
            ServerNotification::ItemCompleted(done) => match &done.item {
                Item::AgentMessage { .. } => println!(),
                item @ Item::ContextCompaction { .. } => {
                    if let Some(line) = protocol_text::compaction_done(item) {
                        notice(line);
                    }
                }
                // A sub-agent's results aren't shown: its answer is.
                Item::ToolCall { id, .. } if done.parent_item_id.is_some() => {
                    self.agents.remove(id);
                }
                Item::ToolCall {
                    id,
                    tool,
                    status,
                    output,
                    detail,
                    changes,
                    ..
                } => match status {
                    ToolCallStatus::Completed | ToolCallStatus::Failed => {
                        if *status == ToolCallStatus::Completed {
                            self.name_agent(id);
                        }
                        self.agents.remove(id);
                        let mark = if *status == ToolCallStatus::Completed {
                            "\x1b[32m  \u{2713}"
                        } else {
                            "\x1b[31m  \u{2717}"
                        };
                        eprintln!("{mark} {tool}\x1b[0m");
                        print_output(output.as_deref().unwrap_or(""));
                    }
                    // Blocked without asking: what it would have done.
                    ToolCallStatus::Declined if !self.asked.contains(id) => {
                        if let Some(detail) = detail {
                            eprintln!("\x1b[33m  \u{1f50d} Would execute: {detail}\x1b[0m");
                        }
                        for change in changes {
                            print_change(change);
                        }
                    }
                    _ => {}
                },
                _ => {}
            },
            ServerNotification::ToolCallOutputDelta(delta) => {
                let line = delta.delta.trim_end_matches('\n');
                match delta.stream {
                    OutputStream::Stderr => eprintln!("  \u{2502}e {line}"),
                    OutputStream::Stdout => eprintln!("  \u{2502} {line}"),
                }
            }
            ServerNotification::ToolCallProgress(progress) => {
                self.name_agent(&progress.item_id);
                eprintln!("\x1b[36m    {}\x1b[0m", progress.message);
            }
            ServerNotification::TurnPlanUpdated(updated) => {
                let (added, changed, removed) =
                    protocol_text::plan_change(&self.plan, &updated.plan);
                eprintln!(
                    "\x1b[2m  [todos] +{added} ~{changed} -{removed} ({} total)\x1b[0m",
                    updated.plan.len()
                );
                self.plan = updated.plan.clone();
            }
            ServerNotification::TurnLoopGuard(guard) => {
                notice((
                    Tone::Warn,
                    protocol_text::loop_guard(guard.action, &guard.detail),
                ));
            }
            ServerNotification::Warning(w) => notice((Tone::Warn, w.message.clone())),
            ServerNotification::McpServerStatusUpdated(update) => mcp_unavailable(update),
            // The goal's state and the evaluator's checks, as the TUI shows
            // them.
            ServerNotification::SessionGoalUpdated(update) => {
                let status = match update.goal.status {
                    GoalStatus::Active => "active",
                    GoalStatus::Paused => "paused",
                    GoalStatus::Blocked => "blocked",
                    GoalStatus::BudgetLimited => "budget used up",
                    GoalStatus::Complete => "complete",
                };
                eprintln!("\x1b[2mGoal ({status}): {}\x1b[0m", update.goal.objective);
            }
            ServerNotification::SessionGoalHeld(held) => notice((
                Tone::Warn,
                format!(
                    "Goal on hold: {} turns in a row called no tool, so Koda stopped.",
                    held.turns
                ),
            )),
            ServerNotification::SessionGoalJudged(judged) => {
                use koda_client::protocol::goal::GoalVerdict;
                let verdict = match judged.verdict {
                    GoalVerdict::Met => "met",
                    GoalVerdict::NotYet => "not met yet",
                    GoalVerdict::Impossible => "impossible",
                };
                let reason = judged.reason.as_deref().unwrap_or_default();
                eprintln!("\x1b[2mGoal check: {verdict}. {reason}\x1b[0m");
            }
            // A retry's notice says what happened and when Koda tries again.
            ServerNotification::Error(e) if e.will_retry => {
                notice((Tone::Warn, e.error.message.clone()));
            }
            ServerNotification::Error(e) => {
                eprintln!("\x1b[31m  \u{2717} {}\x1b[0m", e.error.message);
                self.error_shown = true;
            }
            // A failed turn says why, as Codex's `exec` prints `ERROR:` for
            // one and Koda's TUI shows the turn's error: a turn that ends on
            // an error the host sent no notice for (a rejected key, retries
            // run out) would otherwise exit 1 with nothing said.
            ServerNotification::TurnCompleted(done)
                if done.turn.status == TurnStatus::Failed && !self.error_shown =>
            {
                eprintln!(
                    "\x1b[31m  \u{2717} {}\x1b[0m",
                    failed_turn_message(&done.turn)
                );
                self.error_shown = true;
            }
            ServerNotification::TurnCompleted(done)
                if done.turn.status == TurnStatus::Completed =>
            {
                let tokens = match done.turn.usage.as_ref().map(|u| u.output_tokens) {
                    Some(tokens) if tokens > 0 => tokens,
                    _ => (self.reply_chars / 4) as u64,
                };
                let secs = done.turn.duration_ms.unwrap_or(0) as f64 / 1000.0;
                let rate = if secs > 0.0 {
                    tokens as f64 / secs
                } else {
                    0.0
                };
                eprintln!(
                    "\x1b[90m  {tokens} tokens \u{00b7} {secs:.1}s \u{00b7} {rate:.0} t/s\x1b[0m"
                );
            }
            _ => {}
        }
    }
}

impl View {
    /// Name the agent call `id` started, the first time it shows it ran.
    fn name_agent(&mut self, id: &str) {
        if let Some((agent, indent)) = self.agents.remove(id) {
            eprintln!("\x1b[35m{indent}\u{1f916} {agent}\x1b[0m");
        }
    }
}

/// A notice on stderr.
/// Say that an MCP server didn't start, and why, as `codex exec` warns: the
/// run goes on without its tools, which is otherwise invisible here.
fn mcp_unavailable(update: &McpServerStatusUpdatedNotification) {
    if update.status != McpServerStartupState::Failed {
        return;
    }
    let why = update.error.as_deref().unwrap_or("it failed to start");
    notice((
        Tone::Warn,
        format!("MCP server '{}' unavailable: {why}", update.name),
    ));
}

fn notice((tone, text): (Tone, String)) {
    match tone {
        Tone::Info => eprintln!("\x1b[36m  {text}\x1b[0m"),
        Tone::Warn => eprintln!("\x1b[33m  \u{26a0} {text}\x1b[0m"),
    }
}

/// A tool's output, its middle cut when it is long.
fn print_output(output: &str) {
    use koda_tui::truncate::{Truncated, truncate_for_display};
    match truncate_for_display(output) {
        Truncated::Full(_) => {
            for line in output.lines() {
                eprintln!("  \u{2502} {line}");
            }
        }
        Truncated::Split {
            head,
            tail,
            hidden,
            total,
        } => {
            for line in &head {
                eprintln!("  \u{2502} {line}");
            }
            eprintln!(
                "\x1b[2m{}\x1b[0m",
                koda_tui::truncate::separator(hidden, total)
            );
            for line in &tail {
                eprintln!("  \u{2502} {line}");
            }
        }
    }
}

/// A file change: a change's unified diff, or a new file's first lines.
fn print_change(change: &FileUpdateChange) {
    match change.kind {
        PatchChangeKind::Add => {
            eprintln!("  + {} (new file)", change.path);
            let lines: Vec<&str> = change.diff.lines().collect();
            for line in lines.iter().take(NEW_FILE_LINES) {
                eprintln!("  +{line}");
            }
            if lines.len() > NEW_FILE_LINES {
                eprintln!("  \u{2026} {} more lines", lines.len() - NEW_FILE_LINES);
            }
        }
        PatchChangeKind::Delete => eprintln!("  - {} (deleted)", change.path),
        PatchChangeKind::Update { .. } => {
            eprintln!("  ~ {}", change.path);
            for line in change.diff.lines() {
                eprintln!("  {line}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use koda_client::protocol::item::ItemStartedNotification;
    use koda_client::protocol::turn::{TurnCompletedNotification, TurnError};

    fn tool_start(name: &str, parent: Option<&str>) -> Outgoing {
        Outgoing::Notification(ServerNotification::ItemStarted(ItemStartedNotification {
            session_id: "s1".into(),
            turn_id: "t1".into(),
            parent_item_id: parent.map(str::to_string),
            item: Item::ToolCall {
                id: format!("call-{name}"),
                tool: name.into(),
                arguments: serde_json::json!({}),
                effect: None,
                kind: None,
                status: ToolCallStatus::InProgress,
                output: None,
                detail: None,
                changes: Vec::new(),
                progress: Vec::new(),
                exit_code: None,
                duration_ms: None,
            },
            started_at_ms: 0,
        }))
    }

    fn turn_completed(status: TurnStatus, error: Option<&str>) -> Outgoing {
        Outgoing::Notification(ServerNotification::TurnCompleted(
            TurnCompletedNotification {
                session_id: "s1".into(),
                turn: Turn {
                    id: "t1".into(),
                    status,
                    error: error.map(|message| TurnError {
                        message: message.into(),
                        code: None,
                    }),
                    usage: None,
                    duration_ms: None,
                    started_at: None,
                    completed_at: None,
                    items: Vec::new(),
                },
            },
        ))
    }

    #[test]
    fn stats_split_top_level_and_sub_agent_tool_calls() {
        let mut view = View::new(Output::Events { deltas: false });
        for message in [
            tool_start("Read", None),
            tool_start("Read", None),
            tool_start("Edit", None),
            tool_start("Grep", Some("call-task")),
            turn_completed(TurnStatus::Completed, None),
        ] {
            view.record(&message);
        }

        let stats = view.stats;
        assert_eq!(
            stats.tool_calls,
            BTreeMap::from([("Edit".to_string(), 1), ("Read".to_string(), 2)])
        );
        assert_eq!(
            stats.sub_agent_tool_calls,
            BTreeMap::from([("Grep".to_string(), 1)])
        );
        assert_eq!(stats.turn_end, Some((TurnStatus::Completed, None)));
    }

    #[test]
    fn exec_summary_carries_the_run_and_its_metrics() {
        let main = Usage {
            input_tokens: 120,
            output_tokens: 30,
            model_calls: 2,
            ..Default::default()
        };
        let explore = Usage {
            input_tokens: 80,
            output_tokens: 10,
            model_calls: 1,
            ..Default::default()
        };
        let stats = RunStats {
            tool_calls: BTreeMap::from([("Read".to_string(), 2)]),
            sub_agent_tool_calls: BTreeMap::new(),
            turn_end: Some((TurnStatus::Completed, None)),
            schema_mismatch: None,
            usage: TokenUsage {
                by_agent: BTreeMap::from([
                    ("main".to_string(), main),
                    ("explore".to_string(), explore),
                ]),
                ..Default::default()
            },
            last_reply: Some("done".to_string()),
        };
        let result: Result<()> = Ok(());
        let summary = exec_summary(RunOutcome {
            result: &result,
            session_id: "s1",
            provider: "mock",
            model: "mock-model",
            elapsed_ms: 42,
            stats: &stats,
        });

        assert!(summary.success);
        assert_eq!(summary.error, None);
        assert_eq!(summary.response, "done");
        assert_eq!(summary.session_id, "s1");
        assert_eq!(summary.model, "mock-model");
        assert_eq!(summary.duration_ms, 42);
        assert_eq!(summary.turn_status, Some(TurnStatus::Completed));
        // The whole session's, every agent's added up.
        assert_eq!(summary.usage.input_tokens, 200);
        assert_eq!(summary.usage.model_calls, 3);
        assert_eq!(summary.usage_by_agent["main"].output_tokens, 30);
        assert_eq!(summary.usage_by_agent["explore"].model_calls, 1);
        assert_eq!(summary.tool_calls["Read"], 2);
    }

    #[test]
    fn exec_summary_reports_errors() {
        let result: Result<()> = Err(anyhow::anyhow!("provider exploded"));
        let summary = exec_summary(RunOutcome {
            result: &result,
            session_id: "s1",
            provider: "mock",
            model: "mock-model",
            elapsed_ms: 0,
            stats: &RunStats::default(),
        });
        assert!(!summary.success);
        assert_eq!(summary.error.as_deref(), Some("provider exploded"));
        assert_eq!(summary.turn_status, None);
    }

    /// A turn the engine ended early on a warning returns `Ok` but ends
    /// `failed`; the summary must report failure with that message
    /// (server error, image rejection, dropped connection all take this path).
    #[test]
    fn exec_summary_reports_early_stop_as_failure() {
        for message in [
            "Provider returned a server error: 500",
            "This model rejected the image attachment",
            "Stream failed mid-reply \u{2014} turn discarded (reset)",
        ] {
            let stats = RunStats {
                turn_end: Some((TurnStatus::Failed, Some(message.to_string()))),
                ..Default::default()
            };
            let result: Result<()> = Ok(());
            let summary = exec_summary(RunOutcome {
                result: &result,
                session_id: "s1",
                provider: "mock",
                model: "mock-model",
                elapsed_ms: 0,
                stats: &stats,
            });
            assert!(!summary.success, "{message}");
            assert_eq!(summary.error.as_deref(), Some(message));
            assert_eq!(summary.turn_status, Some(TurnStatus::Failed));
            assert_eq!(run_failure(&result, &stats).as_deref(), Some(message));
        }
    }

    #[test]
    fn cancelled_turn_is_not_a_failure() {
        let stats = RunStats {
            turn_end: Some((TurnStatus::Interrupted, None)),
            ..Default::default()
        };
        assert_eq!(run_failure(&Ok(()), &stats), None);
    }
}
