//! Inner inference loop — `tokio::select!` over input, engine events, the
//! host's messages, the TUI's answers and the draw timer.
//!
//! Koda's host runs the turn (PR 5c); the TUI is its client:
//!
//!   - `turn/start` starts it, or `session/compact/start` for `/compact`,
//!     and `turn/completed` ends this loop.
//!   - The renderer reads the turn's notifications: its items, their
//!     deltas and notices ([`crate::tui_render::TuiRenderer`]). The host's
//!     requests become the dialogs ([`super::ui_handler`]).
//!   - Dialog answers and steering text leave the key handlers as
//!     [`TurnCommand`]s and go to the host as the responses to its requests
//!     and `turn/steer` ([`super::turn_tracker`] keeps the steers).
//!   - Esc, Ctrl+C and Ctrl+D send `turn/interrupt` ([`super::TurnStop`]).
//!
//! What stays from before:
//!
//!   - Rotates which arm is preferred each iteration (`prefer_input`)
//!     so neither terminal input nor the host's messages can monopolise
//!     the executor under sustained load (#1137, #1139, regression of #540).
//!   - The `Draw` arm is the *only* place `terminal.draw()` is called;
//!     every other arm calls `frame_requester.schedule_frame()` so the
//!     coalescing scheduler caps redraws at ~120 FPS (#1138).
//!
//! `drain_bounded` and the regression tests for #1137 live here too,
//! since the bounded drain only has one production caller and unit
//! tests are easier to wire when they share the file.

use super::TurnCommand;
use super::turn_tracker::TurnTracker;
use crate::tui_context::{TuiContext, TurnInput, TurnRequest};
use crate::tui_types::{MenuContent, TuiState};
use crate::tui_viewport::draw_viewport;

use futures_util::StreamExt;
use koda_client::events::Outgoing;
use koda_client::protocol::ServerNotification;
use koda_client::protocol::item::{Item, ToolCallStatus};
use koda_client::protocol::methods;
use koda_client::protocol::session::SessionCompactStartParams;
use koda_client::protocol::turn::{TurnStartParams, TurnSteerParams, UserInput};
use tokio::sync::mpsc;

/// Outcome of one inner-loop `tokio::select!` poll inside `run_inference_turn`.
///
/// Carrying the message out of the select arm (rather than handling it
/// inline) lets us rotate the priority order of the input vs host arms each
/// iteration without duplicating the (large) handler bodies (#1137, #1139).
enum SelectArm {
    Crossterm(crossterm::event::Event),
    /// The frame scheduler granted us a draw slot (#1138). Render once.
    Draw,
    /// A message from the host; `None` once it has stopped. Boxed: a
    /// message is far larger than the other arms.
    Host(Option<Box<Outgoing>>),
    /// An answer or steering text from the key handlers.
    Command(TurnCommand),
}

/// Take up to `cap` items from `next` synchronously, invoking `on_event` for
/// each, and stop early when it has none ready.
///
/// Returns the number of items processed (always `<= cap`).
///
/// This is the kernel of the #1137 fix: the original drain loop was
/// `while let Ok(extra) = rx.try_recv() { ... }` which is unbounded. Under
/// sustained event pressure (sub-agent fan-out, fast streaming) new events
/// arrived between iterations of the loop, so `try_recv()` kept returning
/// `Ok` indefinitely — the parent `tokio::select!` never re-evaluated and
/// terminal input was starved. Bounding the drain forces a yield back to
/// the select after a bounded number of events.
fn drain_bounded<T>(
    mut next: impl FnMut() -> Option<T>,
    cap: usize,
    mut on_event: impl FnMut(T),
) -> usize {
    let mut n = 0;
    for _ in 0..cap {
        match next() {
            Some(item) => {
                on_event(item);
                n += 1;
            }
            None => break,
        }
    }
    n
}

/// The turn a notification belongs to, for those that name one.
fn turn_of(notification: &ServerNotification) -> Option<&str> {
    Some(match notification {
        ServerNotification::ItemStarted(n) => &n.turn_id,
        ServerNotification::ItemCompleted(n) => &n.turn_id,
        ServerNotification::AgentMessageDelta(n) | ServerNotification::ReasoningTextDelta(n) => {
            &n.turn_id
        }
        ServerNotification::ToolCallOutputDelta(n) => &n.turn_id,
        ServerNotification::ToolCallProgress(n) => &n.turn_id,
        ServerNotification::TurnLoopGuard(n) => &n.turn_id,
        ServerNotification::TurnPlanUpdated(n) => &n.turn_id,
        ServerNotification::SessionTokenUsageUpdated(n) => &n.turn_id,
        ServerNotification::Error(n) => n.turn_id.as_deref()?,
        _ => return None,
    })
}

/// What a sub-agent's call says it does, for the overlay: `Read src/a.rs`.
fn activity_of(tool: &str, arguments: &serde_json::Value) -> String {
    let detail: String = crate::tool_header::detail_spans(tool, arguments)
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    if detail.is_empty() {
        tool.to_string()
    } else {
        format!("{tool} {detail}")
    }
}

impl TuiContext {
    /// Open the session a command switched to, if one did, and close the
    /// one it left.
    async fn sync_host_session(&mut self) -> anyhow::Result<()> {
        if self.session.id == self.host_session {
            return Ok(());
        }
        let left = std::mem::take(&mut self.host_session);
        let _ = self
            .client
            .request::<methods::SessionUnsubscribe>(
                koda_client::protocol::session::SessionUnsubscribeParams { session_id: left },
            )
            .await;
        // Its earlier turns were drawn when `/resume` chose it.
        let (opened, _, _) = crate::tui_context::open_host_session(
            &self.client,
            Some(self.session.id.clone()),
            &crate::tui_context::settings::Named::default(),
        )
        .await?;
        self.host_session = opened.id.clone();
        self.session = opened;
        self.mcp_stale = true;
        // The resumed session's own model, and the TUI's mode for it.
        self.sync_settings(true).await
    }

    /// Start a turn on the host and follow it until `turn/completed`:
    /// render its events, answer its requests from the dialogs, steer and
    /// stop it from the keyboard.
    pub(crate) async fn run_inference_turn(
        &mut self,
        request: TurnRequest,
        cmd_tx: &mpsc::Sender<TurnCommand>,
        cmd_rx: &mut mpsc::Receiver<TurnCommand>,
    ) -> anyhow::Result<()> {
        // The prompt, and what goes back to the input box if it can't start.
        let mut watch = None;
        let (prompt, restore) = match request {
            TurnRequest::Watch { turn_id, waiting } => {
                watch = Some((turn_id, waiting));
                (None, None)
            }
            TurnRequest::Prompt(TurnInput {
                message,
                images,
                mentions,
                typed,
                pastes,
                pasted_images,
            }) => (
                Some((message, images, mentions)),
                Some((typed, pastes, pasted_images)),
            ),
            TurnRequest::Compact => (None, None),
        };
        // The host's own words say what went wrong.
        let host_error = |e: koda_client::RequestError| match e {
            koda_client::RequestError::Server { error, .. } => anyhow::anyhow!(error.message),
            other => anyhow::anyhow!(other),
        };

        // The session a command switched to. What it runs with, the model and
        // the mode, is the host's: `/model`, `/provider` and Shift+Tab change
        // it with `session/settings/update` (`tui_context::settings`).
        let watched = watch.as_ref().map(|(turn_id, _)| turn_id.clone());
        // What a goal turn the prompt joined sent before the prompt did.
        let mut joined: Vec<Outgoing> = Vec::new();
        let started =
            async {
                // A turn already running is followed, not started.
                if let Some(turn_id) = watched {
                    return anyhow::Ok(turn_id);
                }
                self.sync_host_session().await?;
                // Messages from before this turn (session notices, resolved
                // requests) are no business of it; a main session's, while in a
                // side conversation, are kept for the return. A turn Koda has
                // just started on the session's goal is kept, with what it sent:
                // the prompt joins it.
                let mut running: Option<(String, Vec<Outgoing>)> = None;
                while let Some(message) = self.client.try_next_event() {
                    if self.note_main_session(&message) {
                        continue;
                    }
                    self.note_session(&message);
                    match &message {
                        Outgoing::Notification(ServerNotification::TurnStarted(started))
                            if started.session_id == self.session.id =>
                        {
                            running = Some((started.turn.id.clone(), Vec::new()));
                        }
                        Outgoing::Notification(ServerNotification::TurnCompleted(done))
                            if running.as_ref().is_some_and(|(id, _)| *id == done.turn.id) =>
                        {
                            running = None;
                        }
                        _ => {
                            if let Some((_, sent)) = &mut running {
                                sent.push(message);
                            }
                        }
                    }
                }
                let Some((message, images, mentions)) = prompt else {
                    self.client
                        .request::<methods::SessionCompactStart>(SessionCompactStartParams {
                            session_id: self.session.id.clone(),
                        })
                        .await
                        .map_err(host_error)?;
                    // The compaction's turn is named by its `turn/started`,
                    // which comes before anything else of it.
                    loop {
                        match self.client.next_event().await {
                            Some(Outgoing::Notification(ServerNotification::TurnStarted(
                                started,
                            ))) if started.session_id == self.session.id => {
                                return anyhow::Ok(started.turn.id);
                            }
                            Some(_) => {}
                            None => anyhow::bail!("Koda's host stopped"),
                        }
                    }
                };
                let message = self.with_review_results(message);
                let input = self.side_boundary(message);
                // Text alone can join a running turn.
                let joinable = images.is_empty() && mentions.is_empty();
                if joinable && let Some((turn_id, sent)) = running {
                    self.steer_goal_turn(&turn_id, input).await?;
                    joined = sent;
                    return anyhow::Ok(turn_id);
                }
                let response =
                    self.client
                        .request::<methods::TurnStart>(TurnStartParams {
                            session_id: self.session.id.clone(),
                            // The text, then what it mentioned and pasted, the
                            // order the model is sent them in.
                            input: std::iter::once(UserInput::text(input.clone()))
                                .chain(mentions.into_iter().map(UserInput::mention))
                                .chain(images.into_iter().map(|image| {
                                    UserInput::image(&image.media_type, &image.base64)
                                }))
                                .collect(),
                            client_user_message_id: None,
                            mode: None,
                            output_schema: None,
                        })
                        .await;
                match response {
                    Ok(response) => anyhow::Ok(response.turn.id),
                    // Koda started a turn on the goal as the prompt went out:
                    // the prompt joins it once it says which.
                    Err(koda_client::RequestError::Server { error, .. })
                        if joinable && error.message.contains("a turn is already running") =>
                    {
                        let (turn_id, sent) = self.next_turn_started().await?;
                        self.steer_goal_turn(&turn_id, input).await?;
                        joined = sent;
                        anyhow::Ok(turn_id)
                    }
                    Err(e) => Err(host_error(e)),
                }
            }
            .await;
        let turn_id = match started {
            Ok(turn_id) => turn_id,
            // Nothing was sent: the text goes back to the input box.
            Err(e) => {
                let mut why = format!("{e:#}");
                if let Some(first) = why.get_mut(..1) {
                    first.make_ascii_uppercase();
                }
                crate::tui_output::err_msg(&mut self.scroll_buffer, why);
                if let Some((typed, pastes, pasted_images)) = restore {
                    self.pastes.restore(&mut self.textarea, &typed, pastes);
                    // The markers come back as markers, with their images.
                    for (marker, _) in &pasted_images {
                        if let Some(at) = typed.find(marker.as_str()) {
                            self.textarea.add_element_range(at..at + marker.len());
                        }
                    }
                    self.pasted_images = pasted_images;
                }
                return Ok(());
            }
        };

        let stop = super::TurnStop {
            requests: self.client.request_handle(),
            session_id: self.session.id.clone(),
            turn_id: turn_id.clone(),
        };
        let mut tracker = TurnTracker::new(turn_id);

        self.tui_state = TuiState::Inferring;
        self.inference_start = Some(std::time::Instant::now());
        self.renderer.start_turn();
        // What a followed turn asked and sent while the TUI was away.
        if let Some((_, waiting)) = watch {
            for message in waiting {
                self.absorb_host_message(message, &mut tracker);
            }
        }
        for message in joined {
            self.absorb_host_message(message, &mut tracker);
        }

        // Ctrl+D asked to quit: the turn was told to stop, and Koda exits
        // once it has.
        let mut quit_requested = false;

        // Rotate which select arm is preferred each iteration so neither
        // terminal input nor the host's messages can monopolise the executor
        // under sustained load (#1137, #1139). `biased` alone only chooses
        // priority *within* one select point — it does not prevent one arm
        // from winning N times in a row when both are constantly ready.
        let mut prefer_input = true;

        // Schedule the initial frame so the user sees state immediately
        // when the turn starts. From here on, every event-handling arm
        // calls `schedule_frame()` to request the next coalesced redraw
        // (#1138). The Draw arm of the select is the *only* place that
        // actually calls `terminal.draw()` — the rate limiter inside the
        // frame scheduler caps that at ~120 FPS.
        self.frame_requester.schedule_frame();

        let host_stopped = tracker.completed().is_none()
            && loop {
                prefer_input = !prefer_input;
                self.show_next_request();

                // Maximum number of host messages to drain in one iteration.
                // The original drain loop was unbounded, which under sustained
                // event pressure (sub-agent fan-out, fast streaming) let the
                // event arm monopolise the executor — terminal events queued
                // up in the OS buffer until the crossterm parser mis-framed
                // partial mouse-report sequences as individual key events,
                // and Ctrl+C took seconds to propagate (#1137).
                const MAX_DRAIN: usize = 64;

                // The Draw arm is *always* polled first within each iteration.
                // It only fires when the frame scheduler has emitted a
                // notification (rate-limited to ~120 FPS), so it cannot
                // starve the other arms in practice.
                let select_result = if prefer_input {
                    tokio::select! {
                        biased;

                        Some(()) = self.draw_rx.recv() => SelectArm::Draw,
                        Some(Ok(ev)) = self.crossterm_events.next() => SelectArm::Crossterm(ev),
                        Some(command) = cmd_rx.recv() => SelectArm::Command(command),
                        message = self.client.next_event() => SelectArm::Host(message.map(Box::new)),
                    }
                } else {
                    tokio::select! {
                        biased;

                        Some(()) = self.draw_rx.recv() => SelectArm::Draw,
                        message = self.client.next_event() => SelectArm::Host(message.map(Box::new)),
                        Some(Ok(ev)) = self.crossterm_events.next() => SelectArm::Crossterm(ev),
                        Some(command) = cmd_rx.recv() => SelectArm::Command(command),
                    }
                };

                match select_result {
                    SelectArm::Draw => {
                        tracing::debug!(
                            target: "koda_cli::diag::child_activity",
                            stage = "inference_draw_arm",
                            "SelectArm::Draw -> terminal.draw()"
                        );
                        let (term_w, _) = crossterm::terminal::size()
                            .map(|(c, r)| (c as usize, r as usize))
                            .unwrap_or((80, 24));
                        let hist_viewport = (self.history_area_height as usize).max(1);
                        self.scroll_buffer.clamp_offset(term_w, hist_viewport);

                        let mode = self.mode;
                        let ctx = self.context_left;
                        let mcp_info = self.mcp.status_bar();
                        // #1210: the overlay above the status bar shows what
                        // the sub-agents and background processes do now. The
                        // turn may start a process: the host is asked about
                        // once a second.
                        self.refresh_background(true).await;
                        let (child_activity_rows, child_activity_total) =
                            self.child_activity.build_rows();
                        let child_activity_hint = self.child_activity.hint();
                        // The working area: what Koda does now, and the
                        // plan. A question for the user takes the status
                        // line's place.
                        let waiting = match &self.menu {
                            MenuContent::Approval { .. } => Some("Waiting for your approval"),
                            MenuContent::AskUser { .. } | MenuContent::LoopCap { .. } => {
                                Some("Waiting for your answer")
                            }
                            _ => None,
                        };
                        let working = self.renderer.working.lines(
                            waiting,
                            self.plan_shown,
                            std::time::Instant::now(),
                        );
                        let mut history_rect = None;
                        let _ = self.terminal.draw(|f| {
                            history_rect = Some(draw_viewport(
                                f,
                                &self.textarea,
                                &self.settings.model,
                                mode,
                                self.sandbox.available,
                                ctx,
                                self.tui_state,
                                &self.prompt_mode,
                                self.inference_start
                                    .map(|s| s.elapsed().as_secs())
                                    .unwrap_or(0),
                                &self.menu,
                                &self.scroll_buffer,
                                self.mouse_selection.as_ref(),
                                mcp_info,
                                &self.project_root,
                                &child_activity_rows,
                                child_activity_total,
                                child_activity_hint,
                                self.side.is_some(),
                                &working,
                            ));
                        });
                        // The working area takes rows from the history
                        // panel: clicks and scrolling go by its new size.
                        if let Some(rect) = history_rect {
                            self.history_area_y = rect.y;
                            self.history_area_height = rect.height;
                        }
                        // Draw again in a second so the status line's time
                        // and the overlay's ages (#1354) tick.
                        self.frame_requester
                            .schedule_frame_in(std::time::Duration::from_secs(1));
                    }
                    SelectArm::Crossterm(ev) => {
                        quit_requested |= super::crossterm_handler::handle_crossterm_event_inline(
                            ev,
                            &stop,
                            cmd_tx,
                            &mut self.scroll_buffer,
                            (
                                self.history_area_y,
                                (self.history_area_height as usize).max(1),
                            ),
                            &mut self.menu,
                            &mut self.prompt_mode,
                            &mut self.pending_approval_id,
                            &mut self.textarea,
                            (&mut self.mode, &self.sandbox),
                            &mut self.completer,
                            (&mut self.history, &mut self.history_pastes),
                            &mut self.history_idx,
                            &mut self.pastes,
                            (&self.input_history, &self.session.id),
                            &mut self.child_activity,
                            &mut self.plan_shown,
                        )
                        .await;
                        self.frame_requester.schedule_frame();
                    }
                    SelectArm::Command(TurnCommand::Side { question }) => {
                        // The turn goes on without the TUI, which opens a side
                        // conversation and follows the turn again on the way
                        // back.
                        self.side_from_turn = Some((question, tracker.turn_id().to_string()));
                        break false;
                    }
                    SelectArm::Command(command) => {
                        self.send_turn_command(command, &mut tracker).await;
                    }
                    SelectArm::Host(None) => break true,
                    SelectArm::Host(Some(message)) => {
                        self.absorb_host_message(*message, &mut tracker);
                        // Bounded drain — at most MAX_DRAIN more messages per
                        // iteration so we yield back to the select for input.
                        // The drain still amortises N messages into 1 redraw,
                        // but cannot monopolise the executor anymore.
                        let _ = drain_bounded(
                            || {
                                // Nothing of the turn comes after its end.
                                if tracker.completed().is_some() {
                                    return None;
                                }
                                let message = self.client.try_next_event()?;
                                self.absorb_host_message(message, &mut tracker);
                                Some(())
                            },
                            MAX_DRAIN,
                            |()| {},
                        );
                        self.frame_requester.schedule_frame();
                    }
                }
                if tracker.completed().is_some() {
                    break false;
                }
            };

        // A thought not drawn yet goes in; the lookup row closes.
        self.renderer.end_turn(&mut self.scroll_buffer);
        // Its sub-agents ended with it; what it left running in the
        // background stays listed.
        self.child_activity.clear_agents();
        let requests = self.client.request_handle();
        crate::tui_context::background::list_background(
            &requests,
            &self.session.id,
            &mut self.child_activity,
        )
        .await;
        // The shared server can be reached again: the idle loop reconnects,
        // and says so. A host in this process is gone for good.
        let host_gone = host_stopped && self.backend == crate::backend::Backend::InProcess;
        let failed = if host_gone {
            Some("Koda's host stopped".to_string())
        } else if host_stopped {
            None
        } else {
            tracker
                .completed()
                .and_then(|turn| turn.error.as_ref())
                .filter(|_| !self.renderer.error_shown())
                .map(|error| error.message.clone())
        };
        if let Some(error) = failed {
            self.scroll_buffer
                .push(crate::tui_render::turn_failed_line(&error));
        }

        // The turn's end, as Codex's TUI marks it: how long it worked.
        if let Some(ms) = tracker.completed().and_then(|turn| turn.duration_ms) {
            self.scroll_buffer
                .push(crate::tui_render::worked_for_line(ms));
        }

        // Tell a user who looked away that Koda is waiting again, as Codex
        // does: the reply's start, or why the turn failed. Not for a turn
        // the user stopped, nor when Koda is quitting.
        if !quit_requested && !host_stopped {
            let status = tracker.completed().map(|turn| turn.status);
            let message = match status {
                Some(koda_client::protocol::turn::TurnStatus::Completed) => {
                    Some(tracker.last_reply().unwrap_or_default().to_string())
                }
                Some(koda_client::protocol::turn::TurnStatus::Failed) => {
                    let reason = tracker
                        .completed()
                        .and_then(|turn| turn.error.as_ref())
                        .map(|error| error.message.clone())
                        .unwrap_or_default();
                    Some(format!("Turn failed: {reason}"))
                }
                _ => None,
            };
            if let Some(message) = message {
                crate::notifications::notify(crate::notifications::Notification::TurnComplete {
                    message,
                });
            }
        }

        self.last_turn = tracker
            .completed()
            .map(|turn| (turn.status, tracker.last_reply().map(str::to_string)));
        let unsent = tracker.take_unsent_steers();
        // A Plan turn that ended with a plan: implement it, or keep
        // planning, as Codex's TUI asks once the turn is done and nothing
        // the user wrote waits to go.
        let offer_plan = self.mode == koda_client::protocol::state::TrustMode::Plan
            && tracker.proposed_plan()
            && unsent.is_empty()
            && self.queued.is_empty()
            && !quit_requested
            && tracker.completed().is_some_and(|turn| {
                turn.status == koda_client::protocol::turn::TurnStatus::Completed
            });
        let completed = tracker
            .completed()
            .is_some_and(|turn| turn.status == koda_client::protocol::turn::TurnStatus::Completed);
        self.post_turn_cleanup(unsent, completed && !quit_requested && !host_stopped);
        if offer_plan {
            self.offer_to_implement_plan();
        }
        if quit_requested || host_gone {
            self.should_quit = true;
        }
        self.turn_cut = host_stopped && !host_gone;
        Ok(())
    }

    /// Take note of one of the host's messages, then show it: a request as
    /// its dialog, a notification of this turn's through the renderer.
    /// Send `input` to a turn Koda started on the session's goal, as text
    /// typed during it is sent.
    async fn steer_goal_turn(&mut self, turn_id: &str, input: String) -> anyhow::Result<()> {
        self.client
            .request::<methods::TurnSteer>(TurnSteerParams {
                session_id: self.session.id.clone(),
                expected_turn_id: turn_id.to_string(),
                input: vec![UserInput::text(input)],
                client_user_message_id: None,
            })
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(())
    }

    /// The session's next `turn/started`, with what came after it, waiting
    /// a few seconds at most.
    async fn next_turn_started(&mut self) -> anyhow::Result<(String, Vec<Outgoing>)> {
        let wait = async {
            loop {
                match self.client.next_event().await {
                    Some(Outgoing::Notification(ServerNotification::TurnStarted(started)))
                        if started.session_id == self.session.id =>
                    {
                        return Ok(started.turn.id);
                    }
                    Some(message) => {
                        self.note_main_session(&message);
                        self.note_session(&message);
                    }
                    None => anyhow::bail!("Koda's host stopped"),
                }
            }
        };
        let turn_id = tokio::time::timeout(std::time::Duration::from_secs(5), wait)
            .await
            .map_err(|_| anyhow::anyhow!("a turn is already running"))??;
        let mut sent = Vec::new();
        while let Some(message) = self.client.try_next_event() {
            sent.push(message);
        }
        Ok((turn_id, sent))
    }

    fn absorb_host_message(&mut self, message: Outgoing, tracker: &mut TurnTracker) {
        if self.note_main_session(&message) {
            return;
        }
        if self.mcp.observe(&self.session.id, &message) {
            self.mcp_stale = true;
        }
        self.note_session(&message);
        tracker.observe_message(&message);
        if self.take_request(&message) {
            return;
        }
        if self.note_update(&message) {
            return;
        }
        let notification = match &message {
            Outgoing::Notification(notification) => notification,
            _ => return,
        };
        match notification {
            // A warning names only its session.
            ServerNotification::Warning(w)
                if w.session_id.as_deref().is_none_or(|s| s == self.session.id) => {}
            n if turn_of(n) == Some(tracker.turn_id()) => {}
            _ => return,
        }
        self.observe_tokens(notification);
        self.observe_agents(notification);
        self.renderer.render(notification, &mut self.scroll_buffer);
    }

    /// The status bar's context meter, from `session/tokenUsage/updated`:
    /// the last call's tokens against the window, as Codex reads it.
    fn observe_tokens(&mut self, notification: &ServerNotification) {
        let ServerNotification::SessionTokenUsageUpdated(updated) = notification else {
            return;
        };
        let usage = &updated.token_usage;
        let Some(window) = usage.model_context_window else {
            return;
        };
        self.context_left = crate::tui_render::context_left(usage.last.total_tokens, window);
        // A window the host learned, as after an overflow; adopted once the
        // turn has ended.
        let window = window as usize;
        if window != self.settings.context_window as usize {
            self.learned_context_window = Some(window);
        }
    }

    /// The overlay's sub-agent rows, from the turn's items.
    fn observe_agents(&mut self, notification: &ServerNotification) {
        match notification {
            ServerNotification::ItemStarted(started) => {
                if let Item::ToolCall {
                    id,
                    tool,
                    arguments,
                    ..
                } = &started.item
                {
                    if tool == "InvokeAgent" {
                        let name = arguments["agent_name"].as_str().unwrap_or("agent");
                        self.child_activity.agent_started(id, name);
                    }
                    if let Some(parent) = &started.parent_item_id {
                        self.child_activity
                            .agent_activity(parent, activity_of(tool, arguments));
                    }
                }
            }
            ServerNotification::ItemCompleted(done) => {
                if let Item::ToolCall {
                    id, tool, status, ..
                } = &done.item
                {
                    if tool == "InvokeAgent" {
                        self.child_activity.agent_ended(id);
                    }
                    // A failure shows without waiting for the end.
                    if let Some(parent) = &done.parent_item_id
                        && *status == ToolCallStatus::Failed
                    {
                        self.child_activity
                            .agent_activity(parent, format!("{tool} \u{2717}"));
                    }
                }
            }
            ServerNotification::ToolCallProgress(progress) => {
                self.child_activity
                    .agent_activity(&progress.item_id, progress.message.trim().to_string());
            }
            _ => {}
        }
    }

    /// Send what a key handler asked for: an answer as the response to its
    /// request, steering text as `turn/steer`.
    async fn send_turn_command(&mut self, command: TurnCommand, tracker: &mut TurnTracker) {
        let text = match command {
            TurnCommand::Answer { request, response } => {
                if let Err(e) = self.client.resolve_server_request(request, response).await {
                    tracing::warn!("an answer didn't reach Koda's host: {e}");
                }
                return;
            }
            TurnCommand::Steer { text } => text,
            TurnCommand::Queue { text } => {
                let message = crate::queued::QueuedMessage::take(
                    text,
                    &mut self.pastes.list,
                    &mut self.pasted_images,
                );
                self.queued.push(message);
                return;
            }
            TurnCommand::Unqueue => {
                self.unqueue_last();
                return;
            }
            // The loop takes it before it gets here.
            TurnCommand::Side { .. } => return,
        };
        // Kept until the model receives it. If the turn has just ended the
        // host refuses it, and it comes back to the input box with the rest.
        let client_user_message_id = tracker.steer_sent(text.clone());
        let turn = tracker.turn_id().to_string();
        let steered = self
            .client
            .request::<methods::TurnSteer>(TurnSteerParams {
                session_id: self.session.id.clone(),
                expected_turn_id: turn,
                input: vec![UserInput::text(text)],
                client_user_message_id: Some(client_user_message_id),
            })
            .await;
        if let Err(e) = steered {
            tracing::debug!("turn/steer: {e}");
        }
    }
}

#[cfg(test)]
mod turn_tests {
    use super::turn_of;
    use koda_client::protocol::ServerNotification;
    use koda_client::protocol::turn::TurnPlanUpdatedNotification;

    /// The plan is the turn's: the working area's panel draws it.
    #[test]
    fn a_plan_update_belongs_to_its_turn() {
        let updated = ServerNotification::TurnPlanUpdated(TurnPlanUpdatedNotification {
            session_id: "s".into(),
            turn_id: "t1".into(),
            plan: Vec::new(),
        });
        assert_eq!(turn_of(&updated), Some("t1"));
    }
}

#[cfg(test)]
mod drain_tests {
    //! Regression tests for the bounded drain that fixes #1137.
    //!
    //! The original bug was an unbounded `while let Ok(extra) = rx.try_recv()`
    //! drain inside the inference loop's `tokio::select!` arm. Under sustained
    //! event pressure the loop never returned to the select, starving terminal
    //! input + Ctrl+C. The fix is to bound how many events one iteration
    //! processes before yielding back to the select.

    use super::drain_bounded;
    use tokio::sync::mpsc;

    #[test]
    fn drain_returns_zero_on_empty_channel() {
        let (_tx, mut rx) = mpsc::unbounded_channel::<u32>();
        let mut seen = Vec::new();
        let n = drain_bounded(|| rx.try_recv().ok(), 64, |item| seen.push(item));
        assert_eq!(n, 0);
        assert!(seen.is_empty());
    }

    #[test]
    fn drain_processes_all_when_below_cap() {
        let (tx, mut rx) = mpsc::unbounded_channel::<u32>();
        for i in 0..10 {
            tx.send(i).unwrap();
        }
        let mut seen = Vec::new();
        let n = drain_bounded(|| rx.try_recv().ok(), 64, |item| seen.push(item));
        assert_eq!(n, 10);
        assert_eq!(seen, (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn drain_stops_at_cap_and_leaves_remainder() {
        // The core regression guard for #1137: a flood larger than `cap`
        // must NOT drain the channel completely in one call. The remaining
        // items have to wait for the next iteration of the parent select,
        // which is what guarantees terminal input gets serviced.
        let (tx, mut rx) = mpsc::unbounded_channel::<u32>();
        for i in 0..1_000 {
            tx.send(i).unwrap();
        }
        let mut seen = Vec::new();
        let n = drain_bounded(|| rx.try_recv().ok(), 64, |item| seen.push(item));
        assert_eq!(n, 64, "drain must stop at cap, even with 1000 items queued");
        assert_eq!(seen.len(), 64);
        assert_eq!(seen.first(), Some(&0));
        assert_eq!(seen.last(), Some(&63));

        // The remaining 936 items are still in the channel, ready for the
        // next iteration. Verify by draining again with a huge cap.
        let mut more = Vec::new();
        let m = drain_bounded(|| rx.try_recv().ok(), 10_000, |item| more.push(item));
        assert_eq!(m, 1_000 - 64);
        assert_eq!(more.first(), Some(&64));
        assert_eq!(more.last(), Some(&999));
    }

    #[test]
    fn drain_with_cap_zero_processes_nothing() {
        // Defensive: a cap of zero must not loop forever or process anything.
        let (tx, mut rx) = mpsc::unbounded_channel::<u32>();
        for i in 0..10 {
            tx.send(i).unwrap();
        }
        let mut seen = Vec::new();
        let n = drain_bounded(|| rx.try_recv().ok(), 0, |item| seen.push(item));
        assert_eq!(n, 0);
        assert!(seen.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sustained_flood_is_drained_across_multiple_iterations() {
        // Simulate the actual #1137 scenario: a producer task floods the
        // channel between drain calls. The drain must still terminate each
        // call within the cap, eventually catching up across N iterations.
        let (tx, mut rx) = mpsc::unbounded_channel::<u32>();
        let total: u32 = 500;
        for i in 0..total {
            tx.send(i).unwrap();
        }
        // Producer keeps adding items between iterations, mimicking a
        // sub-agent fan-out firing engine events while we drain.
        let _producer_keepalive = tx;

        let mut total_seen = 0;
        let mut iterations = 0;
        loop {
            let n = drain_bounded(|| rx.try_recv().ok(), 64, |_| {});
            total_seen += n;
            iterations += 1;
            if n == 0 {
                break;
            }
            // Safety: bound iterations so the test fails loud rather than
            // hanging if the drain ever stops making progress.
            assert!(iterations < 100, "drain should converge within 100 iters");
        }
        assert_eq!(total_seen as u32, total);
        // At cap=64 and total=500 we expect ~8 iterations.
        assert!(
            iterations >= 8,
            "expected >=8 iters at cap 64 for 500 items"
        );
    }
}
