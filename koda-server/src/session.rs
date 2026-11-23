//! One session on the host: the task that owns it and runs its turns.
//!
//! The task owns the [`KodaSession`], its engine command channel and the
//! running turn. While a turn runs it keeps reading the client's commands:
//! an interrupt cancels the turn and is answered once it has stopped, a
//! steer goes to the engine as [`EngineCommand::QueueNext`], and an answer
//! to a server request goes back as the engine command it stands for.
//! Codex's turn requests are the model
//! (`app-server/src/request_processors/turn_processor.rs`).
//!
//! [`HostSink`] turns the session's engine events into protocol messages
//! and puts them on the client's queue, holding its lock while it does, so
//! events from a turn and its sub-agents keep the order they happened in.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::AtomicI64;
use std::sync::{Arc, Mutex, PoisonError};

use koda_core::agent::KodaAgent;
use koda_core::config::KodaConfig;
use koda_core::engine::{ApprovalDecision, EngineCommand, EngineEvent, EngineSink};
use koda_core::loop_guard::LoopContinuation;
use koda_core::providers::ImageData;
use koda_core::session::KodaSession;
use koda_protocol::item::{self, ToolCallApprovalResponse, UserInputResponse};
use koda_protocol::rpc::{ErrorObject, RequestId, codes};
use koda_protocol::server::WarningNotification;
use koda_protocol::session::{
    ApprovalPolicy, Session, SessionCompactStartResponse, SessionSettingsUpdateParams,
    SessionSettingsUpdateResponse, SessionSettingsUpdatedNotification, SessionStartedNotification,
};
use koda_protocol::turn::{
    LoopCapDecision, LoopCapResponse, Mention, SkillInput, Turn, TurnError, TurnInterruptParams,
    TurnInterruptResponse, TurnStartParams, TurnStartResponse, TurnStatus, TurnSteerParams,
    TurnSteerResponse,
};
use koda_protocol::{PROTOCOL_VERSION, ServerNotification, ServerRequest};
use koda_state::db::{Database, Role};
use koda_state::session_store::SessionStore;
use tokio::sync::{mpsc, oneshot};

use crate::events::{EventProjector, Outgoing};
use crate::host::{Answer, to_json};
use koda_core::mcp::manager::SharedMcpManager;

/// Open server requests, by id: the session each belongs to. Shared by the
/// host, which routes a client's answer, and every session's sink.
pub(crate) type OpenRequests = Arc<Mutex<HashMap<RequestId, String>>>;

/// What a session's task is asked to do.
pub(crate) enum SessionCommand {
    /// `turn/start`.
    StartTurn {
        params: TurnStartParams,
        reply: oneshot::Sender<Answer>,
    },
    /// `turn/interrupt`.
    Interrupt {
        params: TurnInterruptParams,
        reply: oneshot::Sender<Answer>,
    },
    /// `turn/steer`.
    Steer {
        params: TurnSteerParams,
        reply: oneshot::Sender<Answer>,
    },
    /// `session/compact/start`.
    Compact { reply: oneshot::Sender<Answer> },
    /// `session/rewind`.
    Rewind {
        before_turn_id: String,
        reply: oneshot::Sender<Answer>,
    },
    /// `session/settings/update`.
    UpdateSettings {
        params: SessionSettingsUpdateParams,
        reply: oneshot::Sender<Answer>,
    },
    /// The client's answer to one of this session's server requests.
    Respond { id: RequestId, result: Answer },
    /// `config/mcpServer/reload`: take the stored MCP servers again, now
    /// when idle or once the running turn has ended. `done` is dropped once
    /// the session has taken them, or, during a turn, queued them.
    ReloadMcp { done: oneshot::Sender<()> },
    /// `session/goal/set`, `session/goal/get` or `session/goal/clear`.
    Goal {
        request: GoalRequest,
        reply: oneshot::Sender<Answer>,
    },
    /// Stop the turn, if one runs, and end the task. `done`, if given, is
    /// dropped once the task has ended.
    Shutdown { done: Option<oneshot::Sender<()>> },
    /// Store the session's turns sent so far: `done` is dropped once they
    /// are, so a read of the store that follows sees all of them.
    FlushHistory { done: oneshot::Sender<()> },
    /// `session/name/set`: store the name, after anything queued before
    /// it, so a first prompt sent earlier can't title the session over it;
    /// a later one no longer does.
    Named {
        name: String,
        done: oneshot::Sender<Result<(), String>>,
    },
}

impl SessionCommand {
    /// Answer the command's request with `error`, if it has one.
    pub(crate) fn refuse(self, error: ErrorObject) {
        match self {
            Self::StartTurn { reply, .. }
            | Self::Interrupt { reply, .. }
            | Self::Steer { reply, .. }
            | Self::Compact { reply }
            | Self::Rewind { reply, .. }
            | Self::Goal { reply, .. }
            | Self::UpdateSettings { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::Respond { .. }
            | Self::ReloadMcp { .. }
            | Self::Shutdown { .. }
            | Self::FlushHistory { .. }
            | Self::Named { .. } => {}
        }
    }
}

/// A request about the session's goal.
pub(crate) enum GoalRequest {
    Set(koda_protocol::goal::SessionGoalSetParams),
    Get,
    Clear,
}

/// What every session on a host shares.
#[derive(Clone)]
pub(crate) struct SessionContext {
    pub config: KodaConfig,
    /// `~/.config/koda/config.toml`, read again for each new session.
    pub config_file: koda_config::ConfigFile,
    pub project_root: PathBuf,
    pub db: Database,
    /// This machine's sandbox, checked once as the host starts.
    pub sandbox: koda_protocol::state::SandboxState,
    pub events: mpsc::UnboundedSender<Outgoing>,
    pub request_ids: Arc<AtomicI64>,
    pub open_requests: OpenRequests,
    /// Set once the first session has checked this run's effort and
    /// thinking budget against its model ([`crate::settings::check_run_reasoning`]).
    pub run_checked: Arc<std::sync::atomic::AtomicBool>,
}

/// A session, open and ready for a turn.
pub(crate) struct OpenSession {
    pub session: Session,
    pub commands: mpsc::UnboundedSender<SessionCommand>,
    pub handles: SessionHandles,
    pub task: SessionTask,
    /// A resumed session's stored token counts, which the host sends the
    /// client that resumed it, as Codex replays a thread's on attach.
    pub restored_usage: Option<koda_protocol::session::TokenUsage>,
}

/// Open a session: `resume` names an earlier one, or `None` starts one.
pub(crate) async fn open(
    context: SessionContext,
    resume: Option<String>,
    policy: ApprovalPolicy,
    mut choice: crate::settings::ModelChoice,
) -> Result<OpenSession, ErrorObject> {
    let SessionContext {
        mut config,
        config_file: context_file,
        project_root,
        db,
        sandbox,
        events,
        request_ids,
        open_requests,
        run_checked,
    } = context;
    // The modes this session may run in, which the host enforces.
    let rules = crate::settings::ModeRules { sandbox };

    let resumed = resume.is_some();
    let loaded = crate::config_file::load(&context_file)?;
    // A new session starts in the file's mode (with `--mode` over it), else
    // the host's.
    if let Some(file_mode) = crate::config_file::mode(&loaded) {
        config.trust = file_mode;
    }
    // A resumed session keeps its own provider, endpoint, model choice and
    // mode unless the client names others, as Codex's `thread/resume` does.
    let mut mode = config.trust;
    if let Some(id) = &resume {
        // By its full id, as Codex's `thread/resume` takes one: a client
        // matches a shorter one over `session/list`.
        let stored = db
            .get_session(id)
            .await
            .map_err(|e| internal(format!("couldn't read the session: {e:#}")))?
            .ok_or_else(|| {
                ErrorObject::new(
                    codes::INVALID_PARAMS,
                    format!("no session '{id}'. Run `koda` and type /sessions to list them."),
                )
            })?;
        let overridden = choice.provider.is_some() || choice.model.is_some();
        if !overridden {
            choice.provider = stored.provider;
            choice.base_url = choice.base_url.or(stored.base_url);
            choice.model = stored.model;
        }
        if let Some(saved) = stored
            .mode
            .as_deref()
            .and_then(koda_core::trust::TrustMode::parse)
        {
            mode = saved;
        }
    }
    // The mode rules, before anything is created: Auto needs the sandbox,
    // and a session a person answers runs in Safe rather than Plan.
    let mode = rules.allow(mode)?;
    // A new session takes the saved choice where the client named nothing,
    // as a Codex thread starts from `config.toml`'s defaults.
    let saved = crate::config_file::values(&loaded);
    if !resumed {
        choice = crate::settings::with_saved(choice, &saved);
    }
    let alias = crate::settings::apply_start(&mut config, &choice, &saved.model_providers).await?;
    crate::settings::detect_model(&mut config).await;
    // The file's effort, window, retries and timeouts, now that the
    // provider is known, before the provider is asked for its window.
    crate::config_file::apply_settings(&mut config, &loaded);
    // The run's effort and thinking budget were asked for the model the run
    // starts with, so its first session refuses one that model doesn't take,
    // as Codex refuses an effort asked for a model. A later session on
    // another model (`/provider`, `/new`) has them corrected at request
    // time, as Codex falls back when a switch lands on a model without it.
    if !run_checked.swap(true, std::sync::atomic::Ordering::Relaxed) {
        crate::settings::check_run_reasoning(&config, &loaded)?;
    }

    // Ask the provider for the model's window first. A guess is said once
    // the session has started.
    let probe = koda_core::providers::create_provider(&config.provider_settings());
    let source = config.query_and_apply_capabilities(probe.as_ref()).await;
    let notice = config.assumed_context_notice(source);

    let id = match resume {
        Some(id) => id,
        None => db
            .create_session(&config.agent_name, &project_root)
            .await
            .map_err(|e| internal(format!("couldn't create the session: {e:#}")))?,
    };
    db.set_session_model(
        &id,
        &config.provider_type.to_string(),
        &config.base_url,
        alias.unwrap_or(&config.model),
    )
    .await
    .map_err(|e| internal(format!("couldn't record the session's model: {e:#}")))?;

    // Every session's agent is the host's, as Codex's server builds each
    // thread's: a front end never hands it one. The prompt's quick reference
    // is for a session a person answers.
    let interactive = policy != ApprovalPolicy::Never;
    let mut agent = KodaAgent::new(&config, project_root, interactive)
        .await
        .map_err(|e| internal(format!("couldn't build the agent: {e:#}")))?;
    crate::builtin_skills::inject_builtin_skills(&mut agent);
    agent.rebuild_system_prompt(&config);
    // Nobody attends the session: its background commands get a time
    // limit, as Claude Code's do in `claude -p`, so a client that waits for
    // them (`koda exec`) isn't held forever by a server left running.
    if !interactive {
        agent
            .tools
            .bg_registry
            .set_time_limit(config.background_time_limit);
    }
    let agent = Arc::new(agent);
    let history = crate::history::HistoryWriter::spawn(db.clone(), id.clone());
    let mut session = KodaSession::new(id.clone(), agent, db, &config, mode).await;
    // A resumed session already has its title.
    session.title_set = resumed;

    // Its MCP servers start in the background; opening waits only for the
    // required ones, and fails when one of them does, as Codex's does.
    let mcp = session.mcp();
    let servers = crate::mcp::configured(&context_file)?;
    let on_status = crate::mcp::status_fn(&id, &events);
    if let Err(e) = session.start_mcp(servers, Some(on_status)).await {
        if let Some(mcp) = &mcp {
            mcp.write().await.shutdown().await;
        }
        if !resumed {
            let _ = session.db.delete_session(&id).await;
        }
        return Err(internal(format!("{e:#}")));
    }

    let stored = session
        .db
        .get_session(&id)
        .await
        .map_err(|e| internal(format!("couldn't read the session: {e:#}")))?
        .ok_or_else(|| internal(format!("session {id} isn't stored")))?;
    let described = Session {
        model: Some(config.model.clone()),
        provider: Some(config.provider_type.meta().name.to_string()),
        context_window: Some(config.max_context_tokens as u64),
        mode: Some(mode),
        tools: Some(
            session
                .agent
                .turn_tool_defs(&config)
                .iter()
                .map(|tool| tool.name.clone())
                .collect(),
        ),
        instruction_sources: Some(
            koda_core::memory::instruction_sources(&session.agent.project_root)
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
        ),
        ..describe(&stored)
    };
    let _ = events.send(Outgoing::Notification(ServerNotification::SessionStarted(
        SessionStartedNotification {
            session: described.clone(),
            koda_version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: PROTOCOL_VERSION,
        },
    )));
    // A Claude model pinned while its line has moved on.
    let pinned_older = (config.provider_type == koda_core::config::ProviderType::Anthropic)
        .then(|| koda_core::model_alias::pinned_older_notice(&config.model))
        .flatten();
    for message in notice.into_iter().chain(pinned_older) {
        let _ = events.send(Outgoing::Notification(ServerNotification::Warning(
            WarningNotification {
                session_id: Some(id.clone()),
                message,
            },
        )));
    }

    // A resumed session's counts go on from what its stored calls used,
    // as Codex restores a thread's. Input tokens count cached ones, by the
    // provider's rule, as live calls do.
    let mut projector =
        EventProjector::with_request_ids(&id, request_ids).with_agent(Arc::clone(&session.agent));
    let restored_usage = if resumed {
        let stored = session
            .db
            .session_usage_by_agent(&id)
            .await
            .map_err(|e| internal(format!("couldn't read the session's usage: {e:#}")))?;
        (!stored.is_empty()).then(|| {
            let by_agent = stored
                .iter()
                .map(|(agent, usage)| {
                    let call = koda_core::providers::TokenUsage {
                        prompt_tokens: usage.prompt_tokens,
                        completion_tokens: usage.completion_tokens,
                        cache_read_tokens: usage.cache_read_tokens,
                        cache_creation_tokens: usage.cache_creation_tokens,
                        thinking_tokens: usage.thinking_tokens,
                        stop_reason: String::new(),
                    };
                    let totals = koda_core::engine::UsageTotals {
                        api_calls: usage.api_calls,
                        ..koda_core::engine::UsageTotals::of_call(probe.as_ref(), &call)
                    };
                    (agent.clone(), crate::events::usage_of(&totals))
                })
                .collect();
            projector.restore_usage(by_agent, Some(config.max_context_tokens as u64))
        })
    } else {
        None
    };

    let (commands_tx, commands) = mpsc::unbounded_channel();
    let (engine_tx, engine_rx) = mpsc::channel(32);
    let sink = Arc::new(HostSink {
        session_id: id.clone(),
        events,
        open_requests,
        history: Some(history),
        // Koda answers itself only when nobody is there to ask.
        answers: (policy == ApprovalPolicy::Never).then(|| engine_tx.clone()),
        notices: engine_tx.clone(),
        state: Mutex::new(SinkState {
            projector,
            prompt: None,
            steers: VecDeque::new(),
            kinds: HashMap::new(),
            pending_end: None,
            learned_window: None,
            goal_turn: crate::goal::TurnActivity::default(),
            goal_budget: None,
            elicitations: HashMap::new(),
        }),
    });
    // The session's MCP servers ask the client for input through the sink.
    if let Some(mcp) = session.mcp() {
        mcp.read().await.set_elicitor(Arc::new(HostElicitor {
            sink: Arc::downgrade(&sink),
        }));
    }
    // Sub-agents' status goes out as it changes, not only between the
    // turn's model calls.
    session.attach_event_sink(Arc::clone(&sink) as Arc<dyn EngineSink>);
    Ok(OpenSession {
        session: described,
        commands: commands_tx,
        handles: SessionHandles {
            project_root: session.agent.project_root.clone(),
            agent: Arc::clone(&session.agent),
            mcp: mcp.clone(),
        },
        task: SessionTask {
            session,
            config,
            config_file: context_file,
            alias: alias.map(str::to_string),
            pending_settings: Vec::new(),
            rules,
            mcp,
            reload_mcp: false,
            sink,
            engine_tx,
            engine_rx,
            commands,
            waiting: None,
            strikes: crate::goal::Strikes::default(),
            held: false,
            folder_instructions: std::collections::HashSet::new(),
            check: None,
            checkin: crate::goal::CheckIn::default(),
        },
        restored_usage,
    })
}

/// The task that owns one session.
pub(crate) struct SessionTask {
    session: KodaSession,
    config: KodaConfig,
    /// The config file, for an MCP reload.
    config_file: koda_config::ConfigFile,
    /// The alias the model was chosen by, if any.
    alias: Option<String>,
    /// `session/settings/update`s that arrived during the turn, applied
    /// once it has ended.
    pending_settings: Vec<SessionSettingsUpdateParams>,
    /// The session's MCP servers. Kept here, not read from the agent, which
    /// a front end may share with a later session.
    mcp: Option<SharedMcpManager>,
    /// A `config/mcpServer/reload` arrived during the turn.
    reload_mcp: bool,
    /// The modes the session may run in.
    rules: crate::settings::ModeRules,
    sink: Arc<HostSink>,
    engine_tx: mpsc::Sender<EngineCommand>,
    engine_rx: mpsc::Receiver<EngineCommand>,
    commands: mpsc::UnboundedReceiver<SessionCommand>,
    /// A command read while deciding whether to go on with the goal: it
    /// comes first.
    waiting: Option<SessionCommand>,
    /// The counts that block the session's goal.
    strikes: crate::goal::Strikes,
    /// The last turn was interrupted: the goal waits for the user, as Claude
    /// Code's does, until a turn they start ends or they change the goal.
    held: bool,
    /// The subfolders' instruction files already sent with a mentioned file,
    /// each sent once while the session is open.
    folder_instructions: std::collections::HashSet<std::path::PathBuf>,
    /// What the goal evaluator said remains after the last turn, for the
    /// next turn on the goal.
    check: Option<String>,
    /// The goal waiting on background commands, and when to check in.
    checkin: crate::goal::CheckIn,
}

impl SessionTask {
    /// Read commands until told to stop or the host is gone, then stop the
    /// session's MCP servers.
    pub(crate) async fn run(mut self) {
        // Every line logged for this session, by the host or the engine,
        // carries its id: the log database files it under the session.
        let span = tracing::info_span!("session", session_id = %self.session.id);
        tracing::Instrument::instrument(self.serve(), span).await;
        if let Some(mcp) = &self.mcp {
            mcp.write().await.shutdown().await;
        }
    }

    async fn serve(&mut self) {
        loop {
            let command = match self.waiting.take() {
                Some(command) => command,
                None => {
                    // While the model's background commands run, look in on
                    // them: an exit starts a turn that tells the model.
                    let watching = self.background().any_running()
                        || self.background().has_exits()
                        || self.background().has_monitor_notices();
                    tokio::select! {
                        command = self.commands.recv() => match command {
                            Some(command) => command,
                            None => return,
                        },
                        () = tokio::time::sleep(WATCH_EVERY), if watching => {
                            if let Some(next) = self.wake().await
                                && !self.follow(next).await
                            {
                                return;
                            }
                            continue;
                        }
                    }
                }
            };
            let next = match command {
                SessionCommand::StartTurn { params, reply } => self.turn(params, reply).await,
                SessionCommand::Compact { reply } => self.compact(reply).await,
                SessionCommand::Rewind {
                    before_turn_id,
                    reply,
                } => {
                    let _ =
                        reply.send(self.rewind(&before_turn_id).await.and_then(|()| {
                            to_json(koda_protocol::session::SessionRewindResponse {})
                        }));
                    Next::Idle
                }
                SessionCommand::Interrupt { reply, .. } => {
                    let _ = reply.send(Err(invalid("no active turn to interrupt")));
                    Next::Idle
                }
                SessionCommand::Steer { reply, .. } => {
                    let _ = reply.send(Err(invalid("no active turn to steer")));
                    Next::Idle
                }
                SessionCommand::Respond { id, result } => {
                    // Only an MCP server's request outlives a turn.
                    if self.sink.answer(&id, result).is_some() {
                        tracing::debug!("an answer to request {id} arrived after its turn");
                    }
                    Next::Idle
                }
                SessionCommand::ReloadMcp { done } => {
                    self.reload_mcp_now().await;
                    drop(done);
                    Next::Idle
                }
                SessionCommand::FlushHistory { done } => {
                    self.sink.flush_history().await;
                    drop(done);
                    Next::Idle
                }
                SessionCommand::Named { name, done } => {
                    self.session.title_set = true;
                    let stored = self
                        .session
                        .db
                        .set_session_title(&self.session.id, &name)
                        .await;
                    let _ = done.send(stored.map_err(|e| format!("{e:#}")));
                    Next::Idle
                }
                SessionCommand::UpdateSettings { params, reply } => {
                    match self.update_settings(params).await {
                        Ok(()) => {
                            let _ = reply.send(to_json(SessionSettingsUpdateResponse {}));
                            self.report_settings();
                        }
                        Err(error) => {
                            let _ = reply.send(Err(error));
                        }
                    }
                    Next::Idle
                }
                SessionCommand::Goal { request, reply } => {
                    self.goal_request(request, reply).await;
                    Next::Idle
                }
                SessionCommand::Shutdown { .. } => return,
            };
            // A command answered between a goal's turns, such as a client
            // asking for the goal or the turn list, goes first; then the
            // goal goes on, as it would have without the command.
            if !self.follow(next).await {
                return;
            }
        }
    }

    /// Answer a goal request while no turn runs.
    async fn goal_request(&mut self, request: GoalRequest, reply: oneshot::Sender<Answer>) {
        if !matches!(request, GoalRequest::Get) {
            self.strikes.reset();
            self.held = false;
            self.check = None;
        }
        let set = goal_request(
            &self.session.db,
            &self.session.id,
            &self.sink,
            request,
            reply,
            self.config.max_goal_token_budget,
        )
        .await;
        let Some(set) = set else {
            return;
        };
        // A session started with a goal is named by it, as by a first prompt.
        if !self.session.title_set {
            self.session.title_set = true;
            let title: String = set.goal.objective.chars().take(50).collect();
            let _ = self
                .session
                .db
                .set_session_title(&self.session.id, &title)
                .await;
        }
    }

    /// The model's background commands.
    fn background(&self) -> &koda_core::tools::bg_process::BgRegistry {
        &self.session.agent.tools.bg_registry
    }

    /// A pending client command, read ahead so it goes first; `false` when
    /// the host is gone.
    fn peek_command(&mut self) -> bool {
        if self.waiting.is_none() {
            match self.commands.try_recv() {
                Ok(command) => self.waiting = Some(command),
                Err(mpsc::error::TryRecvError::Disconnected) => return false,
                Err(mpsc::error::TryRecvError::Empty) => {}
            }
        }
        true
    }

    /// While idle: a turn that tells the model its background commands
    /// exited, as Claude Code starts one when a background task finishes;
    /// or, on a goal they've kept waiting, a check-in on them.
    async fn wake(&mut self) -> Option<Next> {
        if !self.peek_command() || self.waiting.is_some() {
            return None;
        }
        if self.background().has_exits() || self.background().has_monitor_notices() {
            let turn_id = uuid::Uuid::new_v4().to_string();
            return Some(self.run_turn(turn_id, Work::Wake).await);
        }
        self.check_in().await
    }

    /// Check in on background commands that have kept the goal waiting
    /// past its check-in time, as Claude Code does: the goal's turn, with
    /// the running commands listed first.
    async fn check_in(&mut self) -> Option<Next> {
        if !self.goal_may_run() {
            return None;
        }
        let interval = self.config.goal_checkin?;
        let waited = self.checkin.due(interval)?;
        let goal = self.active_goal().await?;
        let running: Vec<_> = self
            .background()
            .snapshot()
            .into_iter()
            .filter(|process| {
                process.spawner.is_none()
                    && process.status == koda_core::tools::bg_process::BgProcessStatus::Running
            })
            .collect();
        if running.is_empty() {
            return None;
        }
        let last = self.checkin.checked_in(interval);
        let message = format!(
            "{}\n\n{}",
            crate::goal::checkin_message(&running, waited, last),
            self.continuation(&goal)
        );
        self.start_goal_turn(&message).await
    }

    /// Whether Koda may start a turn on the goal by itself now.
    fn goal_may_run(&self) -> bool {
        self.config.goals && !self.held && self.session.mode != koda_core::trust::TrustMode::Plan
    }

    /// The session's goal, if it is active.
    async fn active_goal(&self) -> Option<koda_core::goal::Goal> {
        match self.session.db.get_goal(&self.session.id).await {
            Ok(Some(goal)) if goal.status == koda_core::goal::GoalStatus::Active => Some(goal),
            Ok(_) => None,
            Err(e) => {
                tracing::warn!("couldn't read the session's goal: {e:#}");
                None
            }
        }
    }

    /// What starts a turn Koda runs on the goal by itself, by its judge.
    fn continuation(&self, goal: &koda_core::goal::Goal) -> String {
        match self.config.goal_judge {
            koda_config::GoalJudge::Model => koda_core::goal::continuation_message(goal),
            koda_config::GoalJudge::Evaluator => {
                koda_core::goal::evaluated_continuation_message(goal, self.check.as_deref())
            }
        }
    }

    /// Save `message`, unseen by clients, and run the goal's turn on it.
    async fn start_goal_turn(&mut self, message: &str) -> Option<Next> {
        if let Err(e) = self
            .session
            .db
            .insert_message(
                &self.session.id,
                &Role::User,
                Some(message),
                None,
                None,
                None,
            )
            .await
        {
            tracing::warn!("couldn't start a turn on the goal: {e:#}");
            return None;
        }
        let turn_id = uuid::Uuid::new_v4().to_string();
        Some(self.run_turn(turn_id, Work::Goal).await)
    }

    /// Start a turn on the session's goal, if it is active, nothing waits
    /// and the user didn't just interrupt: Codex's continuation when a
    /// thread goes idle.
    async fn continue_goal(&mut self) -> Option<Next> {
        // A client's command goes first; with the host gone, nothing starts.
        if !self.goal_may_run() || !self.peek_command() || self.waiting.is_some() {
            return None;
        }
        let goal = self.active_goal().await?;
        // A background command still running holds the goal, as Claude
        // Code's waits for background work: its exit starts a turn, and the
        // goal goes on after it. A check-in catches one that hangs.
        if self.background().any_running() {
            self.checkin.wait();
            return None;
        }
        self.checkin.done_waiting();
        let message = self.continuation(&goal);
        self.start_goal_turn(&message).await
    }

    /// Run the compactions that turns ended asking for. Returns whether the
    /// task goes on.
    async fn follow(&mut self, mut next: Next) -> bool {
        loop {
            self.apply_pending_settings().await;
            if std::mem::take(&mut self.reload_mcp) {
                self.reload_mcp_now().await;
            }
            next = match next {
                Next::Idle => match self.continue_goal().await {
                    Some(next) => next,
                    None => return true,
                },
                Next::Stop => return false,
                Next::Compact(reply) => self.compact(reply).await,
            };
        }
    }

    /// Apply a `session/settings/update`: check it, then change the model
    /// (probing its window), the effort and the mode, and record them with
    /// the session.
    async fn update_settings(
        &mut self,
        params: SessionSettingsUpdateParams,
    ) -> Result<(), ErrorObject> {
        crate::settings::check_update(
            &self.config,
            params.model.as_deref(),
            params.effort.as_deref(),
        )?;
        let mut next = self.config.clone();
        let mut alias = self.alias.clone();
        let model_changed = match params.model.as_deref() {
            Some(model) => {
                let (id, by) = crate::settings::resolve_update(&next, model).await?;
                next.model = id.clone();
                next.model_settings.model = id;
                next.recalculate_model_derived();
                alias = by.map(str::to_string);
                true
            }
            None => false,
        };
        if let Some(effort) = params.effort {
            next.model_settings.reasoning_effort = Some(effort);
        }
        let id = self.session.id.clone();
        if model_changed {
            let probe = koda_core::providers::create_provider(&next.provider_settings());
            next.query_and_apply_capabilities(probe.as_ref()).await;
            self.session
                .db
                .set_session_model(
                    &id,
                    &next.provider_type.to_string(),
                    &next.base_url,
                    alias.as_deref().unwrap_or(&next.model),
                )
                .await
                .map_err(|e| internal(format!("couldn't record the model: {e:#}")))?;
            self.session.update_provider(&next);
        }
        if let Some(asked) = params.mode {
            let mode = self.rules.allow(asked)?;
            self.session.mode = mode;
            self.session
                .db
                .set_session_mode(&id, mode.as_str())
                .await
                .map_err(|e| internal(format!("couldn't record the mode: {e:#}")))?;
        }
        self.config = next;
        self.alias = alias;
        Ok(())
    }

    /// Bring the session's MCP servers in line with the stored ones.
    async fn reload_mcp_now(&mut self) {
        let Some(mcp) = &self.mcp else {
            return;
        };
        match crate::mcp::configured(&self.config_file) {
            Ok(servers) => koda_core::mcp::McpManager::reconcile(mcp, servers).await,
            Err(error) => tracing::warn!("MCP reload: {}", error.message),
        }
    }

    /// Apply the updates that arrived during the turn. They were checked
    /// when they arrived; one that fails now is logged.
    async fn apply_pending_settings(&mut self) {
        let pending = std::mem::take(&mut self.pending_settings);
        let any = !pending.is_empty();
        for params in pending {
            if let Err(error) = self.update_settings(params).await {
                tracing::warn!("a settings update after the turn failed: {}", error.message);
            }
        }
        if any {
            self.report_settings();
        }
    }

    /// Send `session/settings/updated` with the settings as they are now.
    fn report_settings(&self) {
        let settings =
            crate::settings::report(&self.config, self.alias.as_deref(), self.session.mode);
        self.sink.notify(ServerNotification::SessionSettingsUpdated(
            SessionSettingsUpdatedNotification {
                session_id: self.session.id.clone(),
                settings,
            },
        ));
    }

    /// Compact the session as a turn of its own, answering `reply` first.
    /// `session/rewind`: the turn's starting point, as it stored it, and
    /// everything from there taken out of the history.
    async fn rewind(&mut self, before_turn_id: &str) -> Result<(), ErrorObject> {
        use koda_state::session_store::{MessageBoundary, REWIND_KIND, SessionStore};
        // What the client was sent is stored first, so none of it lands
        // after the rewind.
        self.sink.flush_history().await;
        let id = &self.session.id;
        let records = self
            .session
            .db
            .load_turn_records(id)
            .await
            .map_err(|e| internal(format!("couldn't read the session's turns: {e:#}")))?;
        let boundary = records
            .iter()
            .find(|r| r.turn_id == before_turn_id && r.kind == REWIND_KIND)
            .and_then(|r| serde_json::from_str::<MessageBoundary>(&r.payload).ok());
        let Some(boundary) = boundary else {
            let known = records.iter().any(|r| r.turn_id == before_turn_id);
            return Err(invalid(if known {
                format!(
                    "turn {before_turn_id} is from before Koda recorded where turns start, so it can't be rewound"
                )
            } else {
                format!("no turn {before_turn_id} in this session")
            }));
        };
        self.session
            .db
            .rewind(id, before_turn_id, boundary)
            .await
            .map_err(|e| invalid(format!("{e:#}")))
    }

    /// Store where the session's messages stand as turn `turn_id` starts, so
    /// it can be rewound to before it (`session/rewind`). `boundary` is the
    /// point taken before the prompt's message was stored, for a prompt.
    async fn mark_turn_start(
        &self,
        turn_id: &str,
        boundary: Option<koda_state::session_store::MessageBoundary>,
    ) {
        use koda_state::session_store::{REWIND_KIND, SessionStore, TurnRecord};
        let boundary = match boundary {
            Some(boundary) => boundary,
            None => match self.session.db.message_boundary(&self.session.id).await {
                Ok(boundary) => boundary,
                Err(e) => {
                    tracing::warn!("couldn't note where turn {turn_id} starts: {e:#}");
                    return;
                }
            },
        };
        let record = TurnRecord {
            turn_id: turn_id.to_string(),
            kind: REWIND_KIND.to_string(),
            payload: serde_json::to_string(&boundary).unwrap_or_default(),
        };
        if let Err(e) = self
            .session
            .db
            .append_turn_records(&self.session.id, &[record])
            .await
        {
            tracing::warn!("couldn't note where turn {turn_id} starts: {e:#}");
        }
    }

    async fn compact(&mut self, reply: oneshot::Sender<Answer>) -> Next {
        let _ = reply.send(to_json(SessionCompactStartResponse {}));
        let turn_id = uuid::Uuid::new_v4().to_string();
        self.run_turn(turn_id, Work::Compact).await
    }

    /// Start a turn on `params`, answering `reply` once it has started.
    async fn turn(&mut self, params: TurnStartParams, reply: oneshot::Sender<Answer>) -> Next {
        let TurnStartParams {
            input,
            images,
            mentions,
            skills,
            client_message_id,
            mode,
            output_schema,
            ..
        } = params;
        if let Some(why) = output_schema
            .as_ref()
            .and_then(koda_core::output_schema::invalid_schema)
        {
            let _ = reply.send(Err(ErrorObject::new(
                codes::INVALID_PARAMS,
                format!("outputSchema isn't a JSON Schema Koda can check: {why}"),
            )));
            return Next::Idle;
        }
        let mut images: Vec<ImageData> = images
            .into_iter()
            .map(|image| ImageData {
                media_type: image.media_type,
                base64: image.data,
            })
            .collect();
        // Mentioned files are read here, on the host's machine, under the
        // Read tool's rules; the model gets them after the text, and the
        // item keeps the text as written.
        let message = self.with_mentions(&input, &mentions, &mut images).await;
        let message = self.with_skills(message, &input, &skills);
        let message = match &output_schema {
            Some(schema) => format!(
                "{message}\n\n{}",
                koda_core::output_schema::instruction(schema)
            ),
            None => message,
        };
        self.session.output_schema = output_schema;
        let images = (!images.is_empty()).then_some(images);
        if let Some(mode) = mode {
            self.session.mode = core_mode(mode);
        }

        // The turn reads the conversation back from the database, so a
        // message that wasn't saved would be sent as nothing.
        let session_id = self.session.id.clone();
        // Where the history stands before the prompt, for `session/rewind`.
        let boundary = {
            use koda_state::session_store::SessionStore;
            self.session.db.message_boundary(&session_id).await.ok()
        };
        if let Err(e) = self
            .session
            .db
            .insert_message(&session_id, &Role::User, Some(&message), None, None, None)
            .await
        {
            let _ = reply.send(Err(internal(format!(
                "couldn't save your message, so it wasn't sent: {e:#}"
            ))));
            return Next::Idle;
        }
        if !self.session.title_set {
            self.session.title_set = true;
            let title: String = input.chars().take(50).collect();
            let _ = self.session.db.set_session_title(&session_id, &title).await;
        }

        // Answered before the turn's first notification is produced.
        let turn_id = uuid::Uuid::new_v4().to_string();
        let _ = reply.send(to_json(TurnStartResponse {
            turn: Turn {
                id: turn_id.clone(),
                status: TurnStatus::InProgress,
                error: None,
                usage: None,
                duration_ms: None,
                items: Vec::new(),
            },
        }));
        self.sink
            .begin_turn(input, client_message_id.into_iter().collect(), mentions);
        self.mark_turn_start(&turn_id, boundary).await;
        self.run_turn(turn_id, Work::Prompt(images)).await
    }

    /// `input` as the model is sent it: with each mentioned file's contents
    /// after it, or a directory's listing, or why it wasn't read, unless the
    /// config's `file_mentions` is off, when the model gets the paths to
    /// read itself, as Codex leaves them. Mentioned images join `images`.
    async fn with_mentions(
        &mut self,
        input: &str,
        mentions: &[Mention],
        images: &mut Vec<ImageData>,
    ) -> String {
        use koda_core::tools::mention::{Mentioned, mention_context};
        if mentions.is_empty() {
            return input.to_string();
        }
        if !self.config.file_mentions {
            let paths: Vec<&str> = mentions.iter().map(|m| m.path.as_str()).collect();
            return format!(
                "{input}\n\n(Mentioned, not attached; read them if you need them: {})",
                paths.join(", ")
            );
        }
        let mut read = Vec::new();
        let mut resources = Vec::new();
        for mention in mentions {
            if let Some((server, uri)) = self.resource_mention(&mention.path).await {
                resources.push(self.read_resource(server, uri, images).await);
                continue;
            }
            match self.session.agent.tools.read_mention(&mention.path).await {
                Mentioned::Image(image) => images.push(image),
                other => read.push((mention.path.clone(), other)),
            }
        }
        let mut message = match mention_context(&read) {
            Some(context) => format!("{input}\n\n{context}"),
            None => input.to_string(),
        };
        for resource in resources {
            message.push_str("\n\n");
            message.push_str(&resource);
        }
        for instructions in self.folder_instructions_for(mentions) {
            message.push_str("\n\n");
            message.push_str(&instructions);
        }
        message
    }

    /// An `@server:uri` mention's server and URI, as Claude Code names an
    /// MCP resource: one of the session's connected servers, a colon, and a
    /// URI with a scheme. Anything else is a path.
    async fn resource_mention<'a>(&self, path: &'a str) -> Option<(&'a str, &'a str)> {
        let (server, uri) = path.split_once(':')?;
        if !uri.contains("://") {
            return None;
        }
        let mcp = self.mcp.as_ref()?;
        mcp.read()
            .await
            .is_connected(server)
            .then_some((server, uri))
    }

    /// Read MCP resource `uri` from `server` for the model, as Claude Code
    /// attaches a mentioned resource: its text, marked as data, as MCP
    /// results are; an image goes with the turn's images.
    async fn read_resource(&self, server: &str, uri: &str, images: &mut Vec<ImageData>) -> String {
        use koda_core::mcp::client::ResourceContent;
        /// The most of a resource's text sent, as Read's own cap keeps a
        /// mention from filling the window.
        const MAX_CHARS: usize = 100_000;
        let open = format!("<resource server=\"{server}\" uri=\"{uri}\">");
        let Some(mcp) = self.mcp.as_ref() else {
            return format!("{open}\nNo MCP servers.\n</resource>");
        };
        let contents = match mcp.read().await.read_resource(server, uri).await {
            Ok(contents) => contents,
            Err(e) => return format!("{open}\nNot read: {e:#}\n</resource>"),
        };
        let mut parts = Vec::new();
        for content in contents {
            match content {
                ResourceContent::Text { text, .. } => {
                    let total = text.chars().count();
                    let mut text: String = text.chars().take(MAX_CHARS).collect();
                    if total > MAX_CHARS {
                        text.push_str(&format!("\n[Cut at {MAX_CHARS} of {total} characters.]"));
                    }
                    parts.push(koda_core::untrusted::wrap(
                        &format!("{server}:{uri}"),
                        &text,
                    ));
                }
                ResourceContent::Blob {
                    mime_type: Some(mime),
                    blob,
                } if mime.starts_with("image/") => {
                    images.push(ImageData {
                        media_type: mime.clone(),
                        base64: blob,
                    });
                    parts.push(format!("An image ({mime}), attached."));
                }
                ResourceContent::Blob { mime_type, blob } => parts.push(format!(
                    "Binary content ({}, about {} bytes), not attached.",
                    mime_type.as_deref().unwrap_or("unknown type"),
                    blob.len() / 4 * 3
                )),
            }
        }
        if parts.is_empty() {
            parts.push("It's empty.".to_string());
        }
        format!("{open}\n{}\n</resource>", parts.join("\n"))
    }

    /// The instruction files of the subfolders holding the mentioned paths
    /// that this session hasn't sent yet, as Claude Code adds the `CLAUDE.md`
    /// of an `@` file's folder and the folders above it: root first, each
    /// once.
    fn folder_instructions_for(&mut self, mentions: &[Mention]) -> Vec<String> {
        let root = self.session.agent.project_root.clone();
        let shown_root = root.canonicalize().unwrap_or_else(|_| root.clone());
        let mut found = Vec::new();
        for mention in mentions {
            let path = std::path::Path::new(&mention.path);
            for file in koda_core::memory::nested_instruction_files(&root, path) {
                if !self.folder_instructions.insert(file.clone()) {
                    continue;
                }
                let Ok(content) = std::fs::read_to_string(&file) else {
                    continue;
                };
                let shown = file.strip_prefix(&shown_root).unwrap_or(&file);
                let folder = shown.parent().unwrap_or(shown);
                found.push(koda_core::prompt::folder_instructions_context(
                    &shown.display().to_string(),
                    &folder.display().to_string(),
                    &content,
                ));
            }
        }
        found
    }

    /// `message` with the instructions of each skill the user named after it,
    /// as Codex sends a `$name` skill: those picked (`skills`), then each
    /// `$name` in `input` that names one. A model-only skill
    /// (`user_invocable: false`) isn't the user's to load, and a `$name` that
    /// names no skill stays text.
    fn with_skills(&self, message: String, input: &str, picked: &[SkillInput]) -> String {
        let registry = &self.session.agent.tools.skill_registry;
        let mut names: Vec<&str> = picked.iter().map(|skill| skill.name.as_str()).collect();
        for name in koda_core::skills::mentioned_names(input) {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        let blocks: Vec<String> = names
            .into_iter()
            .filter_map(|name| registry.get(name))
            .filter(|skill| skill.meta.user_invocable)
            .map(koda_core::skills::mention_block)
            .collect();
        if blocks.is_empty() {
            return message;
        }
        format!("{message}\n\n{}", blocks.join("\n\n"))
    }

    /// Run a turn to its end, reading the client's commands meanwhile.
    async fn run_turn(&mut self, turn_id: String, work: Work) -> Next {
        // A prompt's turn marked its start before its message was stored.
        if !matches!(work, Work::Prompt(_)) {
            self.mark_turn_start(&turn_id, None).await;
        }
        let steerable = !matches!(work, Work::Compact);
        // The goal as the turn starts, whose turns are counted toward it.
        let automatic = matches!(work, Work::Goal);
        // Only a turn the user started lifts an interrupt's hold on the goal.
        let users = matches!(work, Work::Prompt(_));
        let goal_before = if steerable && self.config.goals {
            self.session
                .db
                .get_goal(&self.session.id)
                .await
                .inspect_err(|e| tracing::warn!("couldn't read the session's goal: {e:#}"))
                .ok()
                .flatten()
        } else {
            None
        };
        let budget = goal_before
            .as_ref()
            .filter(|goal| goal.status == koda_core::goal::GoalStatus::Active)
            .and_then(crate::goal::BudgetWatch::new);
        self.sink.begin_goal_turn(budget);
        let cancel = self.session.cancel_token().child_token();
        // `session/compact/start` during the turn: it stops, and the
        // compaction runs once it has.
        let mut compact_next: Option<oneshot::Sender<Answer>> = None;
        let mut interrupts = Vec::new();
        // Told when the turn has stopped: dropped as this returns.
        let mut closing: Vec<oneshot::Sender<()>> = Vec::new();
        let mut keep_running = true;
        let Self {
            session,
            config,
            sink,
            engine_tx,
            engine_rx,
            commands,
            pending_settings,
            reload_mcp,
            rules,
            strikes,
            held,
            check,
            checkin,
            ..
        } = self;
        // For `session/name/set` during the turn, which holds the session.
        let (db, session_id) = (session.db.clone(), session.id.clone());
        let result = {
            // The turn's error, if it returned one, as text.
            let mut turn: std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>,
            > = match work {
                Work::Prompt(_) | Work::Goal | Work::Wake => {
                    let images = match work {
                        Work::Prompt(images) => images,
                        _ => None,
                    };
                    let run = session.run_turn_as(
                        turn_id.clone(),
                        config,
                        images,
                        sink.as_ref(),
                        engine_rx,
                        Some(cancel.clone()),
                    );
                    Box::pin(async move { run.await.map_err(|e| format!("{e:#}")) })
                }
                Work::Compact => {
                    let run = session.compact_as(
                        turn_id.clone(),
                        config,
                        sink.as_ref(),
                        Some(cancel.clone()),
                    );
                    Box::pin(async move { run.await.map_err(|e| format!("{e:#}")) })
                }
            };
            loop {
                tokio::select! {
                    result = &mut turn => break result,
                    command = commands.recv(), if keep_running => match command {
                        Some(SessionCommand::StartTurn { reply, .. }) => {
                            let _ = reply.send(Err(invalid("a turn is already running")));
                        }
                        Some(SessionCommand::Interrupt { params, reply }) => {
                            if params.turn_id == turn_id {
                                cancel.cancel();
                                interrupts.push(reply);
                            } else {
                                let _ = reply.send(Err(invalid(format!(
                                    "expected active turn id {} but found {turn_id}",
                                    params.turn_id
                                ))));
                            }
                        }
                        Some(SessionCommand::Steer { params, reply }) => {
                            if !steerable {
                                let _ = reply.send(Err(invalid("cannot steer a compact turn")));
                                continue;
                            }
                            if params.expected_turn_id != turn_id {
                                let _ = reply.send(Err(invalid(format!(
                                    "expected active turn id {} but found {turn_id}",
                                    params.expected_turn_id
                                ))));
                                continue;
                            }
                            // `try_send`: the turn isn't polled while this arm
                            // runs, so waiting for room could wait forever.
                            let queued = engine_tx
                                .try_send(EngineCommand::QueueNext { text: params.input });
                            if queued.is_ok() {
                                sink.queue_steer(params.client_message_id);
                            }
                            let _ = reply.send(match queued {
                                Ok(()) => to_json(TurnSteerResponse {
                                    turn_id: turn_id.clone(),
                                }),
                                Err(_) => Err(internal("the turn's input is full; try again")),
                            });
                        }
                        // Replaces the running turn, as Codex's
                        // `thread/compact/start` does.
                        Some(SessionCommand::Compact { reply }) => {
                            if !steerable || compact_next.is_some() {
                                let _ = reply.send(Err(invalid("a compaction is already running")));
                            } else {
                                cancel.cancel();
                                compact_next = Some(reply);
                            }
                        }
                        Some(SessionCommand::Rewind { reply, .. }) => {
                            let _ = reply.send(Err(invalid(
                                "a turn is running; stop it before rewinding",
                            )));
                        }
                        Some(SessionCommand::Respond { id, result }) => {
                            if let Some(command) = sink.answer(&id, result)
                                && engine_tx.try_send(command).is_err()
                            {
                                tracing::warn!("the turn's input is full; an answer was lost");
                            }
                        }
                        // Checked now, applied once the turn has ended.
                        Some(SessionCommand::UpdateSettings { params, reply }) => {
                            let checked = crate::settings::check_update(
                                config,
                                params.model.as_deref(),
                                params.effort.as_deref(),
                            )
                            .and_then(|_| match params.mode {
                                Some(mode) => rules
                                    .allow(mode)
                                    .map(|_| ()),
                                None => Ok(()),
                            });
                            match checked {
                                Ok(_) => {
                                    pending_settings.push(params);
                                    let _ = reply.send(to_json(SessionSettingsUpdateResponse {}));
                                }
                                Err(error) => {
                                    let _ = reply.send(Err(error));
                                }
                            }
                        }
                        // Answered now. A new objective is told to the turn
                        // at its next step; a pause or clear lets it end.
                        Some(SessionCommand::Goal { request, reply }) => {
                            if !matches!(request, GoalRequest::Get) {
                                strikes.reset();
                            }
                            if let Some(set) =
                                goal_request(&db, &session_id, sink, request, reply, config.max_goal_token_budget).await
                                && set.objective_changed
                                && set.goal.status == koda_core::goal::GoalStatus::Active
                            {
                                let text = koda_core::goal::objective_updated_message(&set.goal);
                                if engine_tx.try_send(EngineCommand::Notice { text }).is_err() {
                                    tracing::warn!("the turn's input is full; the new objective waits for the next turn");
                                }
                            }
                        }
                        // The running turn keeps its servers.
                        Some(SessionCommand::ReloadMcp { done }) => {
                            *reload_mcp = true;
                            drop(done);
                        }
                        // The writer runs on a task of its own, so the turn
                        // only pauses while its queued records are stored.
                        Some(SessionCommand::FlushHistory { done }) => {
                            sink.flush_history().await;
                            drop(done);
                        }
                        // This turn's prompt has titled the session already.
                        Some(SessionCommand::Named { name, done }) => {
                            let stored = db.set_session_title(&session_id, &name).await;
                            let _ = done.send(stored.map_err(|e| format!("{e:#}")));
                        }
                        // Told to stop, or the host is gone: stop the turn and
                        // let it wind down. It bounds that itself.
                        Some(SessionCommand::Shutdown { done }) => {
                            cancel.cancel();
                            keep_running = false;
                            closing.extend(done);
                        }
                        None => {
                            cancel.cancel();
                            keep_running = false;
                        }
                    },
                }
            }
        };
        let error = result.err().inspect(|e| {
            tracing::warn!("turn {turn_id} ended with an error: {e}");
        });
        // Steering text the turn didn't deliver: the client sees no item for
        // it. Left in the channel, it would go out with the next prompt.
        let _ = session.take_queued_steers(engine_rx);
        sink.end_turn(error).await;
        // What the turn did counts toward the session's goal.
        if steerable && config.goals {
            let activity = sink.take_goal_turn();
            *check = activity.check.clone();
            match crate::goal::after_turn(
                &db,
                &session_id,
                goal_before.as_ref(),
                automatic,
                &activity,
                strikes,
            )
            .await
            {
                Ok(Some(goal)) => sink.goal_updated(Some(turn_id.clone()), &goal),
                Ok(None) => {}
                Err(e) => tracing::warn!("couldn't count the turn toward the goal: {e:#}"),
            }
            // Turns that only answer, as Claude Code stops a goal loop that
            // makes no tool call: the goal stays, and waits for the user.
            if let Some(turns) = strikes.take_stalled() {
                *held = true;
                sink.notify(ServerNotification::SessionGoalHeld(
                    koda_protocol::goal::SessionGoalHeldNotification {
                        session_id: session_id.clone(),
                        turn_id: turn_id.clone(),
                        turns: u32::from(turns),
                    },
                ));
            }
        }
        // An interrupt hands the session back to the user: the goal stays
        // as it is, and goes on after the next turn they start. A turn Koda
        // started for a background exit leaves the hold as it was.
        if !interrupts.is_empty() {
            *held = true;
        } else if users {
            *held = false;
            checkin.user_turn();
        }
        for reply in interrupts {
            let _ = reply.send(to_json(TurnInterruptResponse {}));
        }
        // A window the server stated in an overflow error holds for the
        // session's next turns too.
        if let Some(tokens) = sink.take_learned_window() {
            config.max_context_tokens = tokens;
            config.model_settings.max_context_tokens = tokens;
        }
        match compact_next {
            _ if !keep_running => Next::Stop,
            Some(reply) => Next::Compact(reply),
            None => Next::Idle,
        }
    }
}

/// What a turn does.
enum Work {
    /// Answer the prompt just saved, with these images.
    Prompt(Option<Vec<ImageData>>),
    /// Go on with the session's goal, as the message just saved says.
    Goal,
    /// Tell the model background commands it started have exited: the
    /// turn's first step does.
    Wake,
    /// Compact the history (`session/compact/start`).
    Compact,
}

/// How often an idle session looks in on its running background commands.
const WATCH_EVERY: std::time::Duration = std::time::Duration::from_millis(500);

/// What the task does once a turn has ended.
enum Next {
    /// Wait for the client.
    Idle,
    /// Compact, as the client asked during the turn.
    Compact(oneshot::Sender<Answer>),
    /// End: told to stop, or the host is gone.
    Stop,
}

/// An open session's parts the host's requests reach: its notes, its
/// background processes and agents, its MCP servers.
#[derive(Clone)]
pub(crate) struct SessionHandles {
    /// The project the session works in, whose notes `session/memory/*`
    /// read and write.
    pub project_root: PathBuf,
    /// The agent the session runs: its background processes for
    /// `session/backgroundTerminals/*` and its sub-agents for `agent/list`.
    pub agent: Arc<koda_core::agent::KodaAgent>,
    /// The session's MCP servers, for `mcpServerStatus/list`.
    pub mcp: Option<SharedMcpManager>,
}

/// A stored session as the protocol describes it, without what an open one
/// runs with: its mode is the one it last ran in, which resuming it keeps.
pub(crate) fn describe(stored: &koda_state::session_store::SessionInfo) -> Session {
    Session {
        id: stored.id.clone(),
        name: stored.title.clone(),
        preview: stored.preview.clone().unwrap_or_default(),
        created_at: stored.created_unix,
        updated_at: stored.updated_unix,
        cwd: stored.project_root.clone(),
        model: None,
        provider: None,
        context_window: None,
        mode: stored
            .mode
            .as_deref()
            .and_then(koda_core::trust::TrustMode::parse),
        tools: None,
        instruction_sources: None,
        turns: Vec::new(),
    }
}

/// What kind of server request is open, to read the client's answer.
#[derive(Debug, Clone, Copy)]
enum RequestKind {
    Approval,
    Question,
    LoopCap,
    Elicitation,
}

struct SinkState {
    projector: EventProjector,
    /// The prompt of the turn about to start, with its client id and the
    /// paths it mentioned.
    prompt: Option<(String, Vec<String>, Vec<Mention>)>,
    /// Client ids of steering texts sent and not yet delivered, oldest
    /// first; `None` where the client gave none.
    steers: VecDeque<Option<String>>,
    kinds: HashMap<RequestId, RequestKind>,
    /// The turn's end, held until the turn has returned, so an error it
    /// returned with goes on `turn/completed`.
    pending_end: Option<EngineEvent>,
    /// A context window the server stated mid-turn.
    learned_window: Option<usize>,
    /// What the running turn did, for the session's goal.
    goal_turn: crate::goal::TurnActivity,
    /// The goal's budget, watched while the turn spends it.
    goal_budget: Option<crate::goal::BudgetWatch>,
    /// MCP servers' requests for input waiting on the client, each with
    /// where its answer goes.
    elicitations: HashMap<RequestId, oneshot::Sender<Answer>>,
}

/// The session's [`EngineSink`]: engine events become protocol messages on
/// the client's queue.
pub(crate) struct HostSink {
    session_id: String,
    events: mpsc::UnboundedSender<Outgoing>,
    open_requests: OpenRequests,
    /// Where the session's turns are stored as they are sent, for resuming
    /// it. `None` in tests that need no database.
    history: Option<crate::history::HistoryWriter>,
    /// Set under [`ApprovalPolicy::Never`]: where Koda's own answers go.
    answers: Option<mpsc::Sender<EngineCommand>>,
    /// Where Koda's notices to the running turn go.
    notices: mpsc::Sender<EngineCommand>,
    state: Mutex<SinkState>,
}

impl HostSink {
    fn lock(&self) -> std::sync::MutexGuard<'_, SinkState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn begin_turn(&self, prompt: String, client_message_ids: Vec<String>, mentions: Vec<Mention>) {
        let mut state = self.lock();
        state.prompt = Some((prompt, client_message_ids, mentions));
        state.steers.clear();
    }

    /// A turn is starting: count what it does afresh, and watch `budget`.
    fn begin_goal_turn(&self, budget: Option<crate::goal::BudgetWatch>) {
        let mut state = self.lock();
        state.goal_turn = crate::goal::TurnActivity::default();
        state.goal_budget = budget;
    }

    /// What the turn that ended did.
    fn take_goal_turn(&self) -> crate::goal::TurnActivity {
        let mut state = self.lock();
        state.goal_budget = None;
        std::mem::take(&mut state.goal_turn)
    }

    /// Send `session/goal/updated`.
    fn goal_updated(&self, turn_id: Option<String>, goal: &koda_core::goal::Goal) {
        self.notify(ServerNotification::SessionGoalUpdated(
            koda_protocol::goal::SessionGoalUpdatedNotification {
                session_id: self.session_id.clone(),
                turn_id,
                goal: crate::goal::to_wire(goal),
            },
        ));
    }

    /// Wait until the turns sent so far are stored.
    async fn flush_history(&self) {
        if let Some(history) = &self.history {
            history.flush().await;
        }
    }

    /// The window the server stated during the turn, if it did.
    fn take_learned_window(&self) -> Option<usize> {
        self.lock().learned_window.take()
    }

    /// Send a notification of the session's own, in order with its events.
    fn notify(&self, notification: ServerNotification) {
        let mut state = self.lock();
        self.send(&mut state, vec![Outgoing::Notification(notification)]);
    }

    fn queue_steer(&self, client_message_id: Option<String>) {
        self.lock().steers.push_back(client_message_id);
    }

    /// The turn has returned: send its `turn/completed`, with `error` on it
    /// when the turn returned one and its end didn't already say why.
    /// Send `turn/completed`, once the turn is stored: a client that reads
    /// the session when the turn ends gets all of it.
    async fn end_turn(&self, error: Option<String>) {
        let out = {
            let mut state = self.lock();
            state.prompt = None;
            state.steers.clear();
            let Some(end) = state.pending_end.take() else {
                return;
            };
            // Requests still open when the turn ends need no answer. An
            // MCP server still waiting for input is told it was cancelled.
            let mut out = state.projector.resolve_all();
            let waiting: Vec<RequestId> = state.elicitations.drain().map(|(id, _)| id).collect();
            for id in waiting {
                state.kinds.remove(&id);
                out.push(self.resolved_notice(&id));
            }
            out.extend(state.projector.project(None, end));
            if let Some(message) = error {
                for message_out in &mut out {
                    if let Outgoing::Notification(ServerNotification::TurnCompleted(done)) =
                        message_out
                        && done.turn.error.is_none()
                    {
                        done.turn.error = Some(TurnError {
                            message: message.clone(),
                            code: None,
                        });
                    }
                }
            }
            out
        };
        if let Some(history) = &self.history {
            history.record(&out);
            history.flush().await;
        }
        let mut state = self.lock();
        self.deliver(&mut state, out);
    }

    /// The engine command the client's answer to `id` stands for, once
    /// `serverRequest/resolved` is out. `None` for a request that isn't
    /// open, such as one whose turn has ended.
    fn answer(&self, id: &RequestId, result: Answer) -> Option<EngineCommand> {
        let mut state = self.lock();
        if let Some(reply) = state.elicitations.remove(id) {
            state.kinds.remove(id);
            let resolved = self.resolved_notice(id);
            self.send(&mut state, vec![resolved]);
            let _ = reply.send(result);
            return None;
        }
        let kind = state.kinds.remove(id)?;
        let engine_id = state.projector.engine_request(id)?;
        let command = engine_command(kind, engine_id.clone(), result);
        if let Some(resolved) = state.projector.resolved(&engine_id) {
            self.send(&mut state, vec![resolved]);
        }
        Some(command)
    }

    fn handle(&self, parent: Option<&str>, event: EngineEvent) {
        let mut state = self.lock();
        let own_answer = self.answers.as_ref().and_then(|_| policy_answer(&event));
        if parent.is_none() {
            state.goal_turn.observe(&event);
        }
        // Every call, sub-agents' too, spends the goal's budget; reaching
        // it, the turn is told to wrap up.
        if let EngineEvent::ModelUsage { usage } = &event
            && let Some(text) = state
                .goal_budget
                .as_mut()
                .and_then(|watch| watch.charge(usage))
            && self
                .notices
                .try_send(EngineCommand::Notice { text })
                .is_err()
        {
            tracing::warn!("the turn's input is full; the goal's budget notice was lost");
        }
        let out = match (parent, event) {
            (None, EngineEvent::SteerDelivered { texts }) => {
                let ids = (0..texts.len())
                    .filter_map(|_| state.steers.pop_front().flatten())
                    .collect();
                state
                    .projector
                    .user_message(texts.join("\n\n"), ids, Vec::new())
            }
            (None, event @ EngineEvent::TurnStart { .. }) => {
                let mut out = state.projector.project(None, event);
                if let Some((text, ids, mentions)) = state.prompt.take() {
                    out.extend(state.projector.user_message(text, ids, mentions));
                }
                out
            }
            // Sent once the turn has returned ([`Self::end_turn`]).
            (None, event @ EngineEvent::ContextWindowChanged { tokens }) => {
                state.learned_window = Some(tokens);
                state.projector.project(None, event)
            }
            (None, event @ EngineEvent::TurnEnd { .. }) => {
                state.pending_end = Some(event);
                Vec::new()
            }
            (parent, event) => state.projector.project(parent, event),
        };
        self.send(&mut state, out);
        // Nobody is there to ask: answer at once and say so, as a server
        // does when another client answers.
        if let (Some((engine_id, command)), Some(answers)) = (own_answer, &self.answers) {
            if answers.try_send(command).is_err() {
                tracing::warn!("the turn's input is full; Koda's own answer was lost");
            }
            if let Some(resolved) = state.projector.resolved(&engine_id) {
                self.send(&mut state, vec![resolved]);
            }
        }
    }

    /// Store what `out` adds to the session's turns, and put it on the
    /// client's queue. Called with the lock held, so messages keep their
    /// order.
    fn send(&self, state: &mut SinkState, out: Vec<Outgoing>) {
        if let Some(history) = &self.history {
            history.record(&out);
        }
        self.deliver(state, out);
    }

    /// Put `out` on the client's queue, keeping the open-request tables in
    /// step.
    fn deliver(&self, state: &mut SinkState, out: Vec<Outgoing>) {
        for message in out {
            match &message {
                Outgoing::Request(request) => {
                    let kind = match request {
                        ServerRequest::ToolCallApproval { .. } => RequestKind::Approval,
                        ServerRequest::UserInput { .. } => RequestKind::Question,
                        ServerRequest::LoopCap { .. } => RequestKind::LoopCap,
                        ServerRequest::McpServerElicitation { .. } => RequestKind::Elicitation,
                    };
                    state.kinds.insert(request.id().clone(), kind);
                    self.open_requests
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(request.id().clone(), self.session_id.clone());
                }
                Outgoing::Notification(ServerNotification::ServerRequestResolved(resolved)) => {
                    state.kinds.remove(&resolved.request_id);
                    self.open_requests
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&resolved.request_id);
                }
                Outgoing::Notification(_) => {}
            }
            // A client that has gone just stops reading.
            let _ = self.events.send(message);
        }
    }
}

impl HostSink {
    /// `serverRequest/resolved` for one of the host's own requests.
    fn resolved_notice(&self, id: &RequestId) -> Outgoing {
        Outgoing::Notification(ServerNotification::ServerRequestResolved(
            koda_protocol::server::ServerRequestResolvedNotification {
                session_id: self.session_id.clone(),
                request_id: id.clone(),
            },
        ))
    }

    /// Ask the client for an MCP server's request for input. `None` when
    /// nobody is there to ask (`koda exec`), which declines it.
    fn elicit(
        &self,
        request: koda_core::mcp::elicitation::ElicitationRequest,
    ) -> Option<oneshot::Receiver<Answer>> {
        if self.answers.is_some() {
            return None;
        }
        let (reply, answer) = oneshot::channel();
        let mut state = self.lock();
        let id = state.projector.new_request_id();
        state.elicitations.insert(id.clone(), reply);
        let params = koda_protocol::mcp::McpServerElicitationRequestParams {
            session_id: self.session_id.clone(),
            server_name: request.server,
            message: request.message,
            requested_schema: request.requested_schema,
            url: request.url,
        };
        self.send(
            &mut state,
            vec![Outgoing::Request(ServerRequest::McpServerElicitation {
                id,
                params,
            })],
        );
        Some(answer)
    }
}

/// The session's answer to its MCP servers' requests for input: the
/// client's, through the sink.
struct HostElicitor {
    sink: std::sync::Weak<HostSink>,
}

#[async_trait::async_trait]
impl koda_core::mcp::elicitation::Elicitor for HostElicitor {
    async fn elicit(
        &self,
        request: koda_core::mcp::elicitation::ElicitationRequest,
    ) -> koda_core::mcp::elicitation::ElicitationAnswer {
        use koda_core::mcp::elicitation::ElicitationAnswer;
        use koda_protocol::mcp::{McpServerElicitationAction, McpServerElicitationRequestResponse};
        let Some(answer) = self.sink.upgrade().and_then(|sink| sink.elicit(request)) else {
            return ElicitationAnswer::Decline;
        };
        // The turn ended, or the client refused or sent no answer we read.
        let Ok(Ok(value)) = answer.await else {
            return ElicitationAnswer::Cancel;
        };
        match serde_json::from_value::<McpServerElicitationRequestResponse>(value) {
            Ok(response) => match response.action {
                McpServerElicitationAction::Accept => ElicitationAnswer::Accept(response.content),
                McpServerElicitationAction::Decline => ElicitationAnswer::Decline,
                McpServerElicitationAction::Cancel => ElicitationAnswer::Cancel,
            },
            Err(e) => {
                tracing::warn!("an elicitation's answer didn't decode: {e}");
                ElicitationAnswer::Cancel
            }
        }
    }
}

impl EngineSink for HostSink {
    fn emit(&self, event: EngineEvent) {
        self.handle(None, event);
    }

    fn emit_child(&self, parent_id: &str, event: EngineEvent) {
        self.handle(Some(parent_id), event);
    }

    /// Whether a person can answer an approval: not when Koda answers its
    /// own, so a tool doesn't point the model at a way out it can't take.
    fn can_ask(&self) -> bool {
        self.answers.is_none()
    }
}

/// The engine command for the client's answer to a request of `kind`. An
/// error, or an answer that doesn't decode, counts as no: the approval is
/// refused, the question gets an empty answer, the loop stops.
fn engine_command(kind: RequestKind, id: String, result: Answer) -> EngineCommand {
    fn decode<T: serde::de::DeserializeOwned>(result: Answer) -> Option<T> {
        match result {
            Ok(value) => serde_json::from_value::<T>(value)
                .inspect_err(|e| tracing::warn!("a server request's answer didn't decode: {e}"))
                .ok(),
            Err(error) => {
                tracing::debug!("the client refused a server request: {error}");
                None
            }
        }
    }
    match kind {
        // Answered in `HostSink::answer`, never turned into a command.
        RequestKind::Elicitation => EngineCommand::Notice {
            text: String::new(),
        },
        RequestKind::Approval => EngineCommand::ApprovalResponse {
            id,
            decision: match decode::<ToolCallApprovalResponse>(result).map(|r| r.decision) {
                Some(item::ApprovalDecision::Approve) => ApprovalDecision::Approve,
                Some(item::ApprovalDecision::ApproveForSession) => {
                    ApprovalDecision::ApproveForSession
                }
                Some(item::ApprovalDecision::RejectWithFeedback { feedback }) => {
                    ApprovalDecision::RejectWithFeedback { feedback }
                }
                Some(item::ApprovalDecision::Reject) | None => ApprovalDecision::Reject,
            },
        },
        RequestKind::Question => EngineCommand::AskUserResponse {
            id,
            answer: decode::<UserInputResponse>(result)
                .map(|r| r.answer)
                .unwrap_or_default(),
        },
        RequestKind::LoopCap => EngineCommand::LoopDecision {
            id,
            action: match decode::<LoopCapResponse>(result).map(|r| r.decision) {
                Some(LoopCapDecision::Continue50) => LoopContinuation::Continue50,
                Some(LoopCapDecision::Continue200) => LoopContinuation::Continue200,
                Some(LoopCapDecision::Stop) | None => LoopContinuation::Stop,
            },
        },
    }
}

/// Koda's own answer to a request when nobody is there to ask
/// ([`ApprovalPolicy::Never`]), with the engine's id for it: approve what the
/// mode let through to an approval, refuse destructive actions and anything
/// outside the sandbox, answer a question with nothing, go on at the loop
/// cap. What `koda exec` has always done.
fn policy_answer(event: &EngineEvent) -> Option<(String, EngineCommand)> {
    let command = match event {
        EngineEvent::ApprovalRequest {
            id,
            effect,
            tool_name,
            detail,
            ..
        } => {
            let decision = if *effect == koda_core::tools::ToolEffect::Destructive
                && detail.starts_with(koda_core::tools::OUTSIDE_SANDBOX_TAG)
            {
                ApprovalDecision::RejectAuto {
                    reason: format!(
                        "running '{tool_name}' outside the sandbox auto-rejected (headless \
                         mode has no human to approve it). Do without it, or tell the \
                         operator which command to run themselves."
                    ),
                }
            } else if *effect == koda_core::tools::ToolEffect::Destructive {
                // #1022 B15: `RejectAuto`, so the model can tell a structural
                // block from a person's no. With "User rejected this action."
                // it kept asking a user who wasn't there.
                ApprovalDecision::RejectAuto {
                    reason: format!(
                        "destructive action '{tool_name}' auto-rejected (headless mode \
                         refuses destructive ops by policy; no human is available to \
                         approve). Adapt your plan: avoid this kind of action for the \
                         rest of this session, or ask the operator to re-run \
                         interactively."
                    ),
                }
            } else {
                ApprovalDecision::Approve
            };
            EngineCommand::ApprovalResponse {
                id: id.clone(),
                decision,
            }
        }
        EngineEvent::AskUserRequest { id, .. } => EngineCommand::AskUserResponse {
            id: id.clone(),
            answer: String::new(),
        },
        EngineEvent::LoopCapReached { id, .. } => EngineCommand::LoopDecision {
            id: id.clone(),
            action: LoopContinuation::Continue200,
        },
        _ => return None,
    };
    let id = match &command {
        EngineCommand::ApprovalResponse { id, .. }
        | EngineCommand::AskUserResponse { id, .. }
        | EngineCommand::LoopDecision { id, .. } => id.clone(),
        _ => unreachable!("only answers are built above"),
    };
    Some((id, command))
}

/// Answer a goal request, sending what changed to the session's clients.
/// The goal, when the request set it.
async fn goal_request(
    db: &Database,
    session_id: &str,
    sink: &HostSink,
    request: GoalRequest,
    reply: oneshot::Sender<Answer>,
    max_budget: Option<i64>,
) -> Option<crate::goal::GoalSet> {
    use koda_protocol::goal::{
        SessionGoalClearResponse, SessionGoalClearedNotification, SessionGoalGetResponse,
        SessionGoalSetResponse,
    };
    let read = |e: anyhow::Error| internal(format!("couldn't read the goal: {e:#}"));
    match request {
        GoalRequest::Get => {
            let answer = db
                .get_goal(session_id)
                .await
                .map_err(read)
                .and_then(|goal| {
                    to_json(SessionGoalGetResponse {
                        goal: goal.as_ref().map(crate::goal::to_wire),
                    })
                });
            let _ = reply.send(answer);
            None
        }
        GoalRequest::Clear => {
            match db.delete_goal(session_id).await {
                Ok(cleared) => {
                    let _ = reply.send(to_json(SessionGoalClearResponse {
                        cleared: cleared.is_some(),
                    }));
                    if cleared.is_some() {
                        sink.notify(ServerNotification::SessionGoalCleared(
                            SessionGoalClearedNotification {
                                session_id: session_id.to_string(),
                            },
                        ));
                    }
                }
                Err(e) => {
                    let _ = reply.send(Err(internal(format!("couldn't clear the goal: {e:#}"))));
                }
            }
            None
        }
        GoalRequest::Set(params) => {
            match crate::goal::set(db, session_id, params, max_budget).await {
                Ok(set) => {
                    let _ = reply.send(to_json(SessionGoalSetResponse {
                        goal: crate::goal::to_wire(&set.goal),
                    }));
                    sink.goal_updated(None, &set.goal);
                    Some(set)
                }
                Err(error) => {
                    let _ = reply.send(Err(error));
                    None
                }
            }
        }
    }
}

fn core_mode(mode: koda_protocol::state::TrustMode) -> koda_core::trust::TrustMode {
    use koda_core::trust::TrustMode as C;
    use koda_protocol::state::TrustMode as P;
    match mode {
        P::Plan => C::Plan,
        P::Safe => C::Safe,
        P::Auto => C::Auto,
    }
}

pub(crate) fn invalid(message: impl Into<String>) -> ErrorObject {
    ErrorObject::new(codes::INVALID_REQUEST, message)
}

pub(crate) fn internal(message: impl Into<String>) -> ErrorObject {
    ErrorObject::new(codes::INTERNAL_ERROR, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use koda_core::engine::TurnEndReason;
    use koda_core::tools::{OUTSIDE_SANDBOX_TAG, ToolEffect};

    fn sink(
        answers: Option<mpsc::Sender<EngineCommand>>,
    ) -> (HostSink, mpsc::UnboundedReceiver<Outgoing>) {
        let (events, rx) = mpsc::unbounded_channel();
        let sink = HostSink {
            session_id: "s1".to_string(),
            events,
            open_requests: OpenRequests::default(),
            history: None,
            answers,
            notices: mpsc::channel(8).0,
            state: Mutex::new(SinkState {
                projector: EventProjector::with_request_ids("s1", Arc::default()),
                prompt: None,
                steers: VecDeque::new(),
                kinds: HashMap::new(),
                pending_end: None,
                learned_window: None,
                goal_turn: crate::goal::TurnActivity::default(),
                goal_budget: None,
                elicitations: HashMap::new(),
            }),
        };
        (sink, rx)
    }

    fn approval(id: &str, effect: ToolEffect, detail: &str) -> EngineEvent {
        EngineEvent::ApprovalRequest {
            id: id.into(),
            tool_call_id: format!("call-{id}"),
            tool_name: "Bash".into(),
            detail: detail.into(),
            preview: None,
            effect,
            allow_for_session: false,
        }
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<Outgoing>) -> Vec<Outgoing> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    /// What `koda exec` has always answered, now answered by the host.
    #[test]
    fn with_nobody_to_ask_koda_answers_by_its_policy() {
        let (tx, mut commands) = mpsc::channel(8);
        let (sink, mut events) = sink(Some(tx));
        assert!(!sink.can_ask(), "nobody can approve a way out");

        sink.emit(approval("a1", ToolEffect::LocalMutation, "write a.txt"));
        sink.emit(approval("a2", ToolEffect::Destructive, "rm -rf build"));
        let outside = format!("{OUTSIDE_SANDBOX_TAG} cargo test");
        sink.emit(approval("a3", ToolEffect::Destructive, &outside));
        sink.emit(EngineEvent::AskUserRequest {
            id: "q1".into(),
            tool_call_id: "call-q1".into(),
            question: "which file?".into(),
            options: vec![],
        });
        sink.emit(EngineEvent::LoopCapReached {
            id: "c1".into(),
            cap: 200,
            recent_tools: vec![],
        });

        let decision = |command| match command {
            Ok(EngineCommand::ApprovalResponse { decision, .. }) => decision,
            other => panic!("expected an approval, got {other:?}"),
        };
        assert_eq!(decision(commands.try_recv()), ApprovalDecision::Approve);
        assert!(matches!(
            decision(commands.try_recv()),
            ApprovalDecision::RejectAuto { reason } if reason.contains("destructive action")
        ));
        assert!(matches!(
            decision(commands.try_recv()),
            ApprovalDecision::RejectAuto { reason } if reason.contains("outside the sandbox")
        ));
        assert!(matches!(
            commands.try_recv(),
            Ok(EngineCommand::AskUserResponse { ref id, ref answer }) if id == "q1" && answer.is_empty()
        ));
        assert!(matches!(
            commands.try_recv(),
            Ok(EngineCommand::LoopDecision { ref id, action: LoopContinuation::Continue200 })
                if id == "c1"
        ));
        assert!(commands.try_recv().is_err(), "no extra commands");

        // Each request is still sent, and resolved straight after.
        let sent: Vec<_> = drain(&mut events)
            .into_iter()
            .map(|m| match m {
                Outgoing::Request(r) => r.method(),
                Outgoing::Notification(n) => n.method(),
            })
            .collect();
        let pairs = ["item/toolCall/requestApproval", "serverRequest/resolved"];
        assert_eq!(sent[..2], pairs);
        assert_eq!(sent.len(), 10, "{sent:?}");
        assert!(sink.open_requests.lock().unwrap().is_empty());
    }

    #[test]
    fn with_a_client_to_ask_the_request_waits() {
        let (sink, mut events) = sink(None);
        assert!(sink.can_ask());
        sink.emit(approval("a1", ToolEffect::LocalMutation, "write a.txt"));
        let sent = drain(&mut events);
        assert!(
            matches!(sent.as_slice(), [Outgoing::Request(_)]),
            "{sent:?}"
        );
        assert_eq!(sink.open_requests.lock().unwrap().len(), 1);
    }

    /// A turn dropped where it stood after a cancel ends `interrupted`, and
    /// says on `turn/completed` that it didn't stop cleanly.
    #[tokio::test]
    async fn a_turn_that_returns_an_error_says_so_on_turn_completed() {
        let (sink, mut events) = sink(None);
        sink.emit(EngineEvent::TurnStart {
            turn_id: "t1".into(),
        });
        sink.emit(EngineEvent::TurnEnd {
            turn_id: "t1".into(),
            reason: TurnEndReason::Cancelled,
        });
        // Held until the turn has returned.
        assert!(drain(&mut events).iter().all(|m| !matches!(
            m,
            Outgoing::Notification(ServerNotification::TurnCompleted(_))
        )));
        sink.end_turn(Some(
            "the turn did not stop within 5s of being cancelled".into(),
        ))
        .await;
        let sent = drain(&mut events);
        let [Outgoing::Notification(ServerNotification::TurnCompleted(done))] = sent.as_slice()
        else {
            panic!("expected turn/completed, got {sent:?}");
        };
        assert_eq!(done.turn.status, TurnStatus::Interrupted);
        assert_eq!(
            done.turn.error.as_ref().unwrap().message,
            "the turn did not stop within 5s of being cancelled"
        );
    }
}
