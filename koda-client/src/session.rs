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
use koda_core::session::KodaSession;
use koda_protocol::item::{self, ToolCallApprovalResponse, UserInputResponse};
use koda_protocol::rpc::{ErrorObject, RequestId, codes};
use koda_protocol::session::{Session, SessionStartedNotification};
use koda_protocol::turn::{
    LoopCapDecision, LoopCapResponse, Turn, TurnInterruptParams, TurnInterruptResponse,
    TurnStartParams, TurnStartResponse, TurnStatus, TurnSteerParams, TurnSteerResponse,
};
use koda_protocol::{PROTOCOL_VERSION, ServerNotification, ServerRequest};
use tokio::sync::{mpsc, oneshot};

use crate::events::{EventProjector, Outgoing};
use crate::host::{Answer, protocol_mode, to_json};

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
    /// The client's answer to one of this session's server requests.
    Respond { id: RequestId, result: Answer },
    /// Stop the turn, if one runs, and end the task.
    Shutdown,
}

impl SessionCommand {
    /// Answer the command's request with `error`, if it has one.
    pub(crate) fn refuse(self, error: ErrorObject) {
        match self {
            Self::StartTurn { reply, .. }
            | Self::Interrupt { reply, .. }
            | Self::Steer { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::Respond { .. } | Self::Shutdown => {}
        }
    }
}

/// What every session on a host shares.
#[derive(Clone)]
pub(crate) struct SessionContext {
    pub config: KodaConfig,
    pub project_root: PathBuf,
    pub db: Database,
    pub events: mpsc::UnboundedSender<Outgoing>,
    pub request_ids: Arc<AtomicI64>,
    pub open_requests: OpenRequests,
}

/// A session, open and ready for a turn.
pub(crate) struct OpenSession {
    pub session: Session,
    pub commands: mpsc::UnboundedSender<SessionCommand>,
    pub task: SessionTask,
}

/// Open a session: `resume` names an earlier one, or `None` starts one.
pub(crate) async fn open(
    context: SessionContext,
    resume: Option<String>,
) -> Result<OpenSession, ErrorObject> {
    let SessionContext {
        mut config,
        project_root,
        db,
        events,
        request_ids,
        open_requests,
    } = context;

    // As `koda exec` does: ask the provider for the model's window first.
    let probe = koda_core::providers::create_provider(&config);
    let source = config.query_and_apply_capabilities(probe.as_ref()).await;
    if let Some(notice) = config.assumed_context_notice(source) {
        tracing::warn!("{notice}");
    }

    let resumed = resume.is_some();
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

    let mut agent = KodaAgent::new(&config, project_root, &[])
        .await
        .map_err(|e| internal(format!("couldn't build the agent: {e:#}")))?;
    crate::builtin_skills::inject_builtin_skills(&mut agent);
    agent.rebuild_system_prompt(&config, &[]);
    let agent = Arc::new(agent);
    let mut session = KodaSession::new(id.clone(), agent, db, &config, config.trust).await;
    // A resumed session already has its title.
    session.title_set = resumed;

    let described = Session {
        id: id.clone(),
        model: config.model.clone(),
        provider: config.provider_type.meta().name.to_string(),
        context_window: config.max_context_tokens as u64,
        mode: protocol_mode(config.trust),
        tools: session
            .agent
            .turn_tool_defs(&config)
            .iter()
            .map(|tool| tool.name.clone())
            .collect(),
    };
    let _ = events.send(Outgoing::Notification(ServerNotification::SessionStarted(
        SessionStartedNotification {
            session: described.clone(),
            koda_version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: PROTOCOL_VERSION,
        },
    )));

    let (commands_tx, commands) = mpsc::unbounded_channel();
    let (engine_tx, engine_rx) = mpsc::channel(32);
    let sink = Arc::new(HostSink {
        session_id: id.clone(),
        events,
        open_requests,
        state: Mutex::new(SinkState {
            projector: EventProjector::with_request_ids(&id, request_ids),
            prompt: None,
            steers: VecDeque::new(),
            kinds: HashMap::new(),
        }),
    });
    Ok(OpenSession {
        session: described,
        commands: commands_tx,
        task: SessionTask {
            session,
            config,
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
    sink: Arc<HostSink>,
    engine_tx: mpsc::Sender<EngineCommand>,
    engine_rx: mpsc::Receiver<EngineCommand>,
    commands: mpsc::UnboundedReceiver<SessionCommand>,
}

impl SessionTask {
    /// Read commands until told to stop or the host is gone.
    pub(crate) async fn run(mut self) {
        while let Some(command) = self.commands.recv().await {
            match command {
                SessionCommand::StartTurn { params, reply } => {
                    if !self.turn(params, reply).await {
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
                SessionCommand::Shutdown => return,
            }
        }
    }

    /// Run one turn. Returns whether the task goes on.
    async fn turn(&mut self, params: TurnStartParams, reply: oneshot::Sender<Answer>) -> bool {
        let TurnStartParams {
            input,
            client_message_id,
            mode,
            ..
        } = params;
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
                "couldn't save the message, so it wasn't sent: {e:#}"
            ))));
            return true;
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

        let cancel = self.session.cancel_token().child_token();
        let mut interrupts = Vec::new();
        let mut keep_running = true;
        let Self {
            session,
            config,
            sink,
            engine_tx,
            engine_rx,
            commands,
        } = self;
        let result = {
            let turn = session.run_turn_as(
                turn_id.clone(),
                config,
                None,
                sink.as_ref(),
                engine_rx,
                Some(cancel.clone()),
            );
            tokio::pin!(turn);
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
                        Some(SessionCommand::Respond { id, result }) => {
                            if let Some(command) = sink.answer(&id, result)
                                && engine_tx.try_send(command).is_err()
                            {
                                tracing::warn!("the turn's input is full; an answer was lost");
                            }
                        }
                        // Told to stop, or the host is gone: stop the turn and
                        // let it wind down. It bounds that itself.
                        Some(SessionCommand::Shutdown) | None => {
                            cancel.cancel();
                            keep_running = false;
                        }
                    },
                }
            }
        };
        if let Err(e) = result {
            tracing::warn!("turn {turn_id} ended with an error: {e:#}");
        }
        // Steering text the turn didn't deliver: the client sees no item for
        // it. Left in the channel, it would go out with the next prompt.
        let _ = session.take_queued_steers(engine_rx);
        sink.end_turn();
        for reply in interrupts {
            let _ = reply.send(to_json(TurnInterruptResponse {}));
        }
        keep_running
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
}

/// The session's [`EngineSink`]: engine events become protocol messages on
/// the client's queue.
pub(crate) struct HostSink {
    session_id: String,
    events: mpsc::UnboundedSender<Outgoing>,
    open_requests: OpenRequests,
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

    fn queue_steer(&self, client_message_id: Option<String>) {
        self.lock().steers.push_back(client_message_id);
    }

    fn end_turn(&self) {
        let mut state = self.lock();
        state.prompt = None;
        state.steers.clear();
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
            // Requests still open when the turn ends need no answer.
            (None, event @ EngineEvent::TurnEnd { .. }) => {
                let mut out = state.projector.resolve_all();
                out.extend(state.projector.project(None, event));
                out
            }
            (parent, event) => state.projector.project(parent, event),
        };
        self.send(&mut state, out);
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

    /// The client answers approvals.
    fn can_ask(&self) -> bool {
        true
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
