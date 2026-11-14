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
use koda_core::db::{Database, Role};
use koda_core::engine::{ApprovalDecision, EngineCommand, EngineEvent, EngineSink};
use koda_core::loop_guard::LoopContinuation;
use koda_core::persistence::Persistence;
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
    LoopCapDecision, LoopCapResponse, Turn, TurnError, TurnInterruptParams, TurnInterruptResponse,
    TurnStartParams, TurnStartResponse, TurnStatus, TurnSteerParams, TurnSteerResponse,
};
use koda_protocol::{PROTOCOL_VERSION, ServerNotification, ServerRequest};
use tokio::sync::{mpsc, oneshot};

use crate::RawEvent;
use crate::events::{EventProjector, Outgoing};
use crate::host::{Answer, protocol_mode, to_json};
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
    /// In process only: what the session's next turns run with. Applied
    /// at once when idle, or once the running turn has ended.
    Configure {
        config: Box<KodaConfig>,
        done: oneshot::Sender<Answer>,
    },
    /// Stop the turn, if one runs, and end the task. `done`, if given, is
    /// dropped once the task has ended.
    Shutdown { done: Option<oneshot::Sender<()>> },
}

impl SessionCommand {
    /// Answer the command's request with `error`, if it has one.
    pub(crate) fn refuse(self, error: ErrorObject) {
        match self {
            Self::StartTurn { reply, .. }
            | Self::Interrupt { reply, .. }
            | Self::Steer { reply, .. }
            | Self::Compact { reply }
            | Self::UpdateSettings { reply, .. }
            | Self::Configure { done: reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::Respond { .. } | Self::ReloadMcp { .. } | Self::Shutdown { .. } => {}
        }
    }
}

/// What every session on a host shares.
#[derive(Clone)]
pub(crate) struct SessionContext {
    pub config: KodaConfig,
    /// `~/.config/koda/config.toml`, read again for each new session.
    pub config_file: koda_config::ConfigFile,
    pub project_root: PathBuf,
    pub db: Database,
    pub events: mpsc::UnboundedSender<Outgoing>,
    pub request_ids: Arc<AtomicI64>,
    pub open_requests: OpenRequests,
    /// Where engine events go as they are, for a front end that asked.
    pub raw: Option<mpsc::UnboundedSender<RawEvent>>,
    /// In process only: the agent every session uses, when the front end
    /// built it. Without one, each session builds its own.
    pub agent: Option<Arc<KodaAgent>>,
}

/// A session, open and ready for a turn.
pub(crate) struct OpenSession {
    pub session: Session,
    pub commands: mpsc::UnboundedSender<SessionCommand>,
    pub handles: SessionHandles,
    pub task: SessionTask,
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
        events,
        request_ids,
        open_requests,
        raw,
        agent,
    } = context;

    let resumed = resume.is_some();
    // A resumed session keeps its own provider, endpoint, model choice and
    // mode unless the client names others, as Codex's `thread/resume` does.
    let mut mode = config.trust;
    if let Some(id) = &resume {
        let id = db
            .resolve_session_id(id)
            .await
            .map_err(|e| ErrorObject::new(codes::INVALID_PARAMS, format!("{e:#}")))?;
        let stored = db
            .get_session(&id)
            .await
            .map_err(|e| internal(format!("couldn't read the session: {e:#}")))?;
        if let Some(stored) = stored {
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
    }
    // A new session takes the saved choice where the client named nothing,
    // as a Codex thread starts from `config.toml`'s defaults.
    let saved = crate::config_file::values(&crate::config_file::load(&context_file)?);
    if !resumed {
        choice = crate::settings::with_saved(choice, &saved);
    }
    let alias = crate::settings::apply_start(&mut config, &choice, &saved.model_providers).await?;
    crate::settings::detect_model(&mut config).await;

    // Ask the provider for the model's window first. A guess is said once
    // the session has started.
    let probe = koda_core::providers::create_provider(&config);
    let source = config.query_and_apply_capabilities(probe.as_ref()).await;
    let notice = config.assumed_context_notice(source);

    let id = match resume {
        Some(id) => db
            .resolve_session_id(&id)
            .await
            .map_err(|e| ErrorObject::new(codes::INVALID_PARAMS, format!("{e:#}")))?,
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

    let agent = match agent {
        Some(agent) => agent,
        None => {
            let mut agent = KodaAgent::new(&config, project_root, &[])
                .await
                .map_err(|e| internal(format!("couldn't build the agent: {e:#}")))?;
            crate::builtin_skills::inject_builtin_skills(&mut agent);
            agent.rebuild_system_prompt(&config, &[]);
            Arc::new(agent)
        }
    };
    let mut session = KodaSession::new(id.clone(), agent, db, &config, mode).await;
    // A resumed session already has its title.
    session.title_set = resumed;

    // Its MCP servers start in the background; opening waits only for the
    // required ones, and fails when one of them does, as Codex's does.
    let mcp = session.mcp();
    let servers = crate::mcp::configured(&context_file, &config)?;
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
        mode: Some(protocol_mode(mode)),
        tools: Some(
            session
                .agent
                .turn_tool_defs(&config)
                .iter()
                .map(|tool| tool.name.clone())
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
    if let Some(message) = notice {
        let _ = events.send(Outgoing::Notification(ServerNotification::Warning(
            WarningNotification {
                session_id: Some(id.clone()),
                message,
            },
        )));
    }

    let (commands_tx, commands) = mpsc::unbounded_channel();
    let (engine_tx, engine_rx) = mpsc::channel(32);
    let sink = Arc::new(HostSink {
        session_id: id.clone(),
        events,
        open_requests,
        raw,
        // Koda answers itself only when nobody is there to ask.
        answers: (policy == ApprovalPolicy::Never).then(|| engine_tx.clone()),
        state: Mutex::new(SinkState {
            projector: EventProjector::with_request_ids(&id, request_ids),
            prompt: None,
            steers: VecDeque::new(),
            kinds: HashMap::new(),
            pending_end: None,
            learned_window: None,
        }),
    });
    // Sub-agents' status goes out as it changes, not only between the
    // turn's model calls.
    session.attach_event_sink(Arc::clone(&sink) as Arc<dyn EngineSink>);
    Ok(OpenSession {
        session: described,
        commands: commands_tx,
        handles: SessionHandles {
            bg_agents: Arc::clone(&session.bg_agents),
            mcp: mcp.clone(),
        },
        task: SessionTask {
            session,
            config,
            config_file: context_file,
            alias: alias.map(str::to_string),
            pending_settings: Vec::new(),
            mcp,
            reload_mcp: false,
            sink,
            engine_tx,
            engine_rx,
            commands,
        },
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
    sink: Arc<HostSink>,
    engine_tx: mpsc::Sender<EngineCommand>,
    engine_rx: mpsc::Receiver<EngineCommand>,
    commands: mpsc::UnboundedReceiver<SessionCommand>,
}

impl SessionTask {
    /// Read commands until told to stop or the host is gone, then stop the
    /// session's MCP servers.
    pub(crate) async fn run(mut self) {
        self.serve().await;
        if let Some(mcp) = &self.mcp {
            mcp.write().await.shutdown().await;
        }
    }

    async fn serve(&mut self) {
        while let Some(command) = self.commands.recv().await {
            match command {
                SessionCommand::StartTurn { params, reply } => {
                    let next = self.turn(params, reply).await;
                    if !self.follow(next).await {
                        return;
                    }
                }
                SessionCommand::Compact { reply } => {
                    let next = self.compact(reply).await;
                    if !self.follow(next).await {
                        return;
                    }
                }
                SessionCommand::Interrupt { reply, .. } => {
                    let _ = reply.send(Err(invalid("no active turn to interrupt")));
                }
                SessionCommand::Steer { reply, .. } => {
                    let _ = reply.send(Err(invalid("no active turn to steer")));
                }
                SessionCommand::Respond { id, .. } => {
                    tracing::debug!("an answer to request {id} arrived after its turn");
                }
                SessionCommand::ReloadMcp { done } => {
                    self.reload_mcp_now().await;
                    drop(done);
                }
                SessionCommand::Configure { config, done } => {
                    self.configure(*config);
                    let _ = done.send(to_json(serde_json::Value::Null));
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
                }
                SessionCommand::Shutdown { .. } => return,
            }
        }
    }

    fn configure(&mut self, config: KodaConfig) {
        self.session.update_provider(&config);
        self.config = config;
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
                Next::Idle => return true,
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
            let probe = koda_core::providers::create_provider(&next);
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
        if let Some(mode) = params.mode {
            let mode = crate::settings::mode_of(mode);
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
        match crate::mcp::configured(&self.config_file, &self.config) {
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
            client_message_id,
            mode,
            ..
        } = params;
        let images = (!images.is_empty()).then(|| {
            images
                .into_iter()
                .map(|image| ImageData {
                    media_type: image.media_type,
                    base64: image.data,
                })
                .collect()
        });
        if let Some(mode) = mode {
            self.session.mode = core_mode(mode);
        }

        // The turn reads the conversation back from the database, so a
        // message that wasn't saved would be sent as nothing.
        let session_id = self.session.id.clone();
        if let Err(e) = self
            .session
            .db
            .insert_message(&session_id, &Role::User, Some(&input), None, None, None)
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
            },
        }));
        self.sink
            .begin_turn(input, client_message_id.into_iter().collect());
        self.run_turn(turn_id, Work::Prompt(images)).await
    }

    /// Run a turn to its end, reading the client's commands meanwhile.
    async fn run_turn(&mut self, turn_id: String, work: Work) -> Next {
        let steerable = matches!(work, Work::Prompt(_));
        let cancel = self.session.cancel_token().child_token();
        // `session/compact/start` during the turn: it stops, and the
        // compaction runs once it has.
        let mut compact_next: Option<oneshot::Sender<Answer>> = None;
        let mut interrupts = Vec::new();
        let mut configured: Option<(KodaConfig, Vec<oneshot::Sender<Answer>>)> = None;
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
            ..
        } = self;
        let result = {
            // The turn's error, if it returned one, as text.
            let mut turn: std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>,
            > = match work {
                Work::Prompt(images) => {
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
                        Some(SessionCommand::Respond { id, result }) => {
                            if let Some(command) = sink.answer(&id, result)
                                && engine_tx.try_send(command).is_err()
                            {
                                tracing::warn!("the turn's input is full; an answer was lost");
                            }
                        }
                        // Checked now, applied once the turn has ended.
                        Some(SessionCommand::UpdateSettings { params, reply }) => {
                            match crate::settings::check_update(
                                config,
                                params.model.as_deref(),
                                params.effort.as_deref(),
                            ) {
                                Ok(_) => {
                                    pending_settings.push(params);
                                    let _ = reply.send(to_json(SessionSettingsUpdateResponse {}));
                                }
                                Err(error) => {
                                    let _ = reply.send(Err(error));
                                }
                            }
                        }
                        // The running turn keeps its servers.
                        Some(SessionCommand::ReloadMcp { done }) => {
                            *reload_mcp = true;
                            drop(done);
                        }
                        // The running turn keeps what it started with.
                        Some(SessionCommand::Configure { config: next, done }) => {
                            let mut waiting = configured.take().map(|(_, d)| d).unwrap_or_default();
                            waiting.push(done);
                            configured = Some((*next, waiting));
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
        sink.end_turn(error);
        for reply in interrupts {
            let _ = reply.send(to_json(TurnInterruptResponse {}));
        }
        // A window the server stated in an overflow error holds for the
        // session's next turns too.
        if let Some(tokens) = sink.take_learned_window() {
            config.max_context_tokens = tokens;
            config.model_settings.max_context_tokens = tokens;
        }
        if let Some((next, waiting)) = configured {
            session.update_provider(&next);
            *config = next;
            for done in waiting {
                let _ = done.send(to_json(serde_json::Value::Null));
            }
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
    /// Compact the history (`session/compact/start`).
    Compact,
}

/// What the task does once a turn has ended.
enum Next {
    /// Wait for the client.
    Idle,
    /// Compact, as the client asked during the turn.
    Compact(oneshot::Sender<Answer>),
    /// End: told to stop, or the host is gone.
    Stop,
}

/// In process only: a session's parts a front end still reaches directly
/// while it moves onto the protocol.
#[derive(Clone)]
pub struct SessionHandles {
    /// The session's sub-agents, for showing what they do and stopping them.
    pub bg_agents: Arc<koda_core::child_agent::ChildAgentRegistry>,
    /// The session's MCP servers, for `mcpServerStatus/list`.
    pub mcp: Option<SharedMcpManager>,
}

/// A stored session as the protocol describes it, without what an open one
/// runs with.
pub(crate) fn describe(stored: &koda_core::persistence::SessionInfo) -> Session {
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
        mode: None,
        tools: None,
    }
}

/// What kind of server request is open, to read the client's answer.
#[derive(Debug, Clone, Copy)]
enum RequestKind {
    Approval,
    Question,
    LoopCap,
}

struct SinkState {
    projector: EventProjector,
    /// The prompt of the turn about to start, with its client id.
    prompt: Option<(String, Vec<String>)>,
    /// Client ids of steering texts sent and not yet delivered, oldest
    /// first; `None` where the client gave none.
    steers: VecDeque<Option<String>>,
    kinds: HashMap<RequestId, RequestKind>,
    /// The turn's end, held until the turn has returned, so an error it
    /// returned with goes on `turn/completed`.
    pending_end: Option<EngineEvent>,
    /// A context window the server stated mid-turn.
    learned_window: Option<usize>,
}

/// The session's [`EngineSink`]: engine events become protocol messages on
/// the client's queue.
pub(crate) struct HostSink {
    session_id: String,
    events: mpsc::UnboundedSender<Outgoing>,
    open_requests: OpenRequests,
    raw: Option<mpsc::UnboundedSender<RawEvent>>,
    /// Set under [`ApprovalPolicy::Never`]: where Koda's own answers go.
    answers: Option<mpsc::Sender<EngineCommand>>,
    state: Mutex<SinkState>,
}

impl HostSink {
    fn lock(&self) -> std::sync::MutexGuard<'_, SinkState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn begin_turn(&self, prompt: String, client_message_ids: Vec<String>) {
        let mut state = self.lock();
        state.prompt = Some((prompt, client_message_ids));
        state.steers.clear();
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
    fn end_turn(&self, error: Option<String>) {
        let mut state = self.lock();
        state.prompt = None;
        state.steers.clear();
        let Some(end) = state.pending_end.take() else {
            return;
        };
        // Requests still open when the turn ends need no answer.
        let mut out = state.projector.resolve_all();
        out.extend(state.projector.project(None, end));
        if let Some(message) = error {
            for message_out in &mut out {
                if let Outgoing::Notification(ServerNotification::TurnCompleted(done)) = message_out
                    && done.turn.error.is_none()
                {
                    done.turn.error = Some(TurnError {
                        message: message.clone(),
                    });
                }
            }
        }
        self.send(&mut state, out);
    }

    /// The engine command the client's answer to `id` stands for, once
    /// `serverRequest/resolved` is out. `None` for a request that isn't
    /// open, such as one whose turn has ended.
    fn answer(&self, id: &RequestId, result: Answer) -> Option<EngineCommand> {
        let mut state = self.lock();
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
        if let Some(raw) = &self.raw {
            let _ = raw.send(RawEvent {
                session_id: self.session_id.clone(),
                parent: parent.map(str::to_string),
                event: event.clone(),
            });
        }
        let own_answer = self.answers.as_ref().and_then(|_| policy_answer(&event));
        let out = match (parent, event) {
            (None, EngineEvent::SteerDelivered { texts }) => {
                let ids = (0..texts.len())
                    .filter_map(|_| state.steers.pop_front().flatten())
                    .collect();
                state.projector.user_message(texts.join("\n\n"), ids)
            }
            (None, event @ EngineEvent::TurnStart { .. }) => {
                let mut out = state.projector.project(None, event);
                if let Some((text, ids)) = state.prompt.take() {
                    out.extend(state.projector.user_message(text, ids));
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

    /// Put `out` on the client's queue, keeping the open-request tables in
    /// step. Called with the lock held, so messages keep their order.
    fn send(&self, state: &mut SinkState, out: Vec<Outgoing>) {
        for message in out {
            match &message {
                Outgoing::Request(request) => {
                    let kind = match request {
                        ServerRequest::ToolCallApproval { .. } => RequestKind::Approval,
                        ServerRequest::UserInput { .. } => RequestKind::Question,
                        ServerRequest::LoopCap { .. } => RequestKind::LoopCap,
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
            raw: None,
            answers,
            state: Mutex::new(SinkState {
                projector: EventProjector::with_request_ids("s1", Arc::default()),
                prompt: None,
                steers: VecDeque::new(),
                kinds: HashMap::new(),
                pending_end: None,
                learned_window: None,
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
    #[test]
    fn a_turn_that_returns_an_error_says_so_on_turn_completed() {
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
        ));
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
