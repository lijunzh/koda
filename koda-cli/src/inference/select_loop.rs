//! Inner inference loop — `tokio::select!` over input, engine events, the
//! host's messages, the TUI's answers and the draw timer.
//!
//! Koda's host runs the turn (PR 5c); the TUI is its client:
//!
//!   - `turn/start` starts it, or `session/compact/start` for `/compact`,
//!     and `turn/completed` ends this loop.
//!   - The renderer reads the engine's own events, which the client hands
//!     out in process ([`koda_client::StartArgs::raw_events`]) until the TUI
//!     renders items. They arrive in order, and all of a turn's before its
//!     `turn/completed`.
//!   - Dialog answers and steering text leave the key handlers as
//!     [`EngineCommand`]s, as before, and go to the host as answers to its
//!     requests and `turn/steer` ([`super::turn_tracker`]).
//!   - Esc, Ctrl+C and Ctrl+D send `turn/interrupt` ([`super::TurnStop`]).
//!
//! What stays from before:
//!
//!   - Rotates which arm is preferred each iteration (`prefer_input`)
//!     so neither terminal input nor engine events can monopolise the
//!     executor under sustained load (#1137, #1139, regression of #540).
//!   - The `Draw` arm is the *only* place `terminal.draw()` is called;
//!     every other arm calls `frame_requester.schedule_frame()` so the
//!     coalescing scheduler caps redraws at ~120 FPS (#1138).
//!
//! `drain_bounded` and the regression tests for #1137 live here too,
//! since the bounded drain only has one production caller and unit
//! tests are easier to wire when they share the file.

use super::turn_tracker::TurnTracker;
use crate::sink::UiEvent;
use crate::tui_context::{TuiContext, TurnInput, TurnRequest};
use crate::tui_types::TuiState;
use crate::tui_viewport::draw_viewport;

use futures_util::StreamExt;
use koda_client::RawEvent;
use koda_client::events::Outgoing;
use koda_client::protocol::ServerNotification;
use koda_client::protocol::methods;
use koda_client::protocol::session::SessionCompactStartParams;
use koda_client::protocol::turn::{ImageInput, TurnStartParams, TurnSteerParams};
use koda_core::engine::{EngineCommand, EngineEvent};
use koda_core::trust;
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use tokio::sync::mpsc;

/// Outcome of one inner-loop `tokio::select!` poll inside `run_inference_turn`.
///
/// Carrying the raw event out of the select arm (rather than handling it
/// inline) lets us rotate the priority order of the input vs ui arms each
/// iteration without duplicating the (large) handler bodies (#1137, #1139).
enum SelectArm {
    Crossterm(crossterm::event::Event),
    Ui(RawEvent),
    /// The frame scheduler granted us a draw slot (#1138). Render once.
    Draw,
    /// A message from the host; `None` once it has stopped.
    Host(Option<Outgoing>),
    /// An answer or steering text from the key handlers.
    Command(EngineCommand),
}

/// Drain up to `cap` items from an `mpsc::UnboundedReceiver` synchronously,
/// invoking `on_event` for each.
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
fn drain_bounded<T, F: FnMut(T)>(
    rx: &mut mpsc::UnboundedReceiver<T>,
    cap: usize,
    mut on_event: F,
) -> usize {
    let mut n = 0;
    for _ in 0..cap {
        match rx.try_recv() {
            Ok(item) => {
                on_event(item);
                n += 1;
            }
            Err(_) => break,
        }
    }
    n
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
        let opened = crate::tui_context::open_host_session(
            &self.client,
            Some(self.session.id.clone()),
            self.session.db.clone(),
            &self.config,
        )
        .await?;
        self.host_session = opened.id.clone();
        self.session = opened;
        // The resumed session's own model, and the TUI's mode for it.
        self.sync_settings(true).await
    }

    /// Start a turn on the host and follow it until `turn/completed`:
    /// render its events, answer its requests from the dialogs, steer and
    /// stop it from the keyboard.
    pub(crate) async fn run_inference_turn(
        &mut self,
        request: TurnRequest,
        ui_rx: &mut mpsc::UnboundedReceiver<RawEvent>,
        cmd_tx: &mpsc::Sender<EngineCommand>,
        cmd_rx: &mut mpsc::Receiver<EngineCommand>,
    ) -> anyhow::Result<()> {
        // The prompt, and what goes back to the input box if it can't start.
        let (prompt, restore) = match request {
            TurnRequest::Prompt(TurnInput {
                message,
                images,
                typed,
                paste_blocks,
            }) => (Some((message, images)), Some((typed, paste_blocks))),
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
        let started = async {
            self.sync_host_session().await?;
            // Messages from before this turn (session notices, resolved
            // requests) are no business of it.
            while self.client.try_next_event().is_some() {}
            let Some((message, images)) = prompt else {
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
                        Some(Outgoing::Notification(ServerNotification::TurnStarted(started)))
                            if started.session_id == self.session.id =>
                        {
                            return anyhow::Ok(started.turn.id);
                        }
                        Some(_) => {}
                        None => anyhow::bail!("Koda's host stopped"),
                    }
                }
            };
            let response = self
                .client
                .request::<methods::TurnStart>(TurnStartParams {
                    session_id: self.session.id.clone(),
                    input: message,
                    images: images
                        .into_iter()
                        .map(|image| ImageInput {
                            media_type: image.media_type,
                            data: image.base64,
                        })
                        .collect(),
                    client_message_id: None,
                    mode: None,
                })
                .await
                .map_err(host_error)?;
            anyhow::Ok(response.turn.id)
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
                if let Some((typed, paste_blocks)) = restore {
                    self.textarea.set_text_clearing_elements(&typed);
                    self.paste_blocks = paste_blocks;
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
        self.renderer.last_turn_stats = None;

        // #1158 (b): the overlay reads the session's sub-agents and the
        // agent's background processes each frame.
        let bg_agents_for_status = self.session.bg_agents.clone();
        let agent_for_status = self.agent.clone();

        // Ctrl+D asked to quit: the turn was told to stop, and Koda exits
        // once it has.
        let mut quit_requested = false;

        // Rotate which select arm is preferred each iteration so neither
        // terminal input nor engine events can monopolise the executor
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

        let host_stopped = loop {
            prefer_input = !prefer_input;

            // Maximum number of engine events to drain in one iteration.
            // The original drain loop was unbounded, which under sustained
            // event pressure (sub-agent fan-out, fast streaming) let the
            // ui_rx arm monopolise the executor — terminal events queued
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
                    Some(raw) = ui_rx.recv() => SelectArm::Ui(raw),
                    Some(command) = cmd_rx.recv() => SelectArm::Command(command),
                    message = self.client.next_event() => SelectArm::Host(message),
                }
            } else {
                tokio::select! {
                    biased;

                    Some(()) = self.draw_rx.recv() => SelectArm::Draw,
                    Some(raw) = ui_rx.recv() => SelectArm::Ui(raw),
                    Some(Ok(ev)) = self.crossterm_events.next() => SelectArm::Crossterm(ev),
                    Some(command) = cmd_rx.recv() => SelectArm::Command(command),
                    message = self.client.next_event() => SelectArm::Host(message),
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

                    let mode = trust::read_trust(&self.shared_mode);
                    let ctx = self.context_pct;
                    let mcp_info = self.agent.mcp_status_bar_info();
                    // #1210: the overlay above the status bar shows what
                    // the sub-agents and background processes do now.
                    let agent_snaps = bg_agents_for_status.snapshot();
                    let process_snaps = agent_for_status.tools.bg_registry.snapshot();
                    let (child_activity_rows, child_activity_total) =
                        self.child_activity.build_rows(&agent_snaps, &process_snaps);
                    let _ = self.terminal.draw(|f| {
                        draw_viewport(
                            f,
                            &self.textarea,
                            &self.config.model,
                            mode,
                            ctx,
                            self.tui_state,
                            &self.prompt_mode,
                            self.inference_start
                                .map(|s| s.elapsed().as_secs())
                                .unwrap_or(0),
                            self.renderer.last_turn_stats.as_ref(),
                            &self.menu,
                            &self.scroll_buffer,
                            self.mouse_selection.as_ref(),
                            mcp_info,
                            &self.project_root,
                            &child_activity_rows,
                            child_activity_total,
                        );
                    });
                    // #1354: while any sub-agent or background process runs,
                    // draw again in a second so the overlay's ages tick.
                    if child_activity_total > 0 {
                        self.frame_requester
                            .schedule_frame_in(std::time::Duration::from_secs(1));
                    }
                }
                SelectArm::Crossterm(ev) => {
                    quit_requested |= super::crossterm_handler::handle_crossterm_event_inline(
                        ev,
                        &stop,
                        cmd_tx,
                        &mut self.scroll_buffer,
                        self.history_area_height as usize,
                        &mut self.menu,
                        &mut self.prompt_mode,
                        &mut self.pending_approval_id,
                        &mut self.textarea,
                        &self.shared_mode,
                        &mut self.completer,
                        &mut self.history,
                        &mut self.history_idx,
                        &mut self.paste_blocks,
                        (&self.input_history, &self.session.id),
                        &bg_agents_for_status,
                        &agent_for_status.tools.bg_registry,
                        &mut self.child_activity,
                    )
                    .await;
                    self.frame_requester.schedule_frame();
                }
                SelectArm::Ui(raw) => {
                    self.absorb_turn_event(raw.event, &mut tracker);
                    // Bounded drain — at most MAX_DRAIN extra events per
                    // iteration so we yield back to the select for input.
                    // The drain still amortises N events into 1 redraw, but
                    // cannot monopolise the executor anymore.
                    let _ = drain_bounded(ui_rx, MAX_DRAIN, |extra| {
                        self.absorb_turn_event(extra.event, &mut tracker);
                    });
                    self.frame_requester.schedule_frame();
                }
                SelectArm::Command(command) => {
                    self.send_turn_command(command, &mut tracker).await;
                }
                SelectArm::Host(None) => break true,
                SelectArm::Host(Some(message)) => {
                    tracker.observe_message(message);
                    // An answer given before its request was read goes now.
                    self.send_ready_answers(&mut tracker).await;
                }
            }
            if tracker.completed().is_some() {
                break false;
            }
        };

        // The turn's events all arrived before its `turn/completed`.
        while let Ok(raw) = ui_rx.try_recv() {
            self.absorb_turn_event(raw.event, &mut tracker);
        }
        let failed = if host_stopped {
            Some("Koda's host stopped".to_string())
        } else {
            tracker
                .completed()
                .and_then(|turn| turn.error.as_ref())
                .filter(|_| !tracker.error_shown())
                .map(|error| error.message.clone())
        };
        if let Some(error) = failed {
            self.scroll_buffer.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("\u{2717} Turn failed: {error}"),
                    Style::default().fg(Color::Red),
                ),
            ]));
        }

        self.post_turn_cleanup(tracker.take_unsent_steers());
        if quit_requested || host_stopped {
            self.should_quit = true;
        }
        Ok(())
    }

    /// Take note of one of the turn's engine events, then show it.
    fn absorb_turn_event(&mut self, event: EngineEvent, tracker: &mut TurnTracker) {
        tracker.observe_event(&event);
        if let EngineEvent::ContextUsage { used, max } = &event {
            self.context_pct = if *max > 0 {
                (used * 100 / max) as u32
            } else {
                0
            };
        }
        if let EngineEvent::ContextWindowChanged { tokens } = &event {
            self.learned_context_window = Some(*tokens);
        }
        super::ui_handler::handle_inference_ui_inline(
            UiEvent::Engine(event),
            &mut self.scroll_buffer,
            &mut self.menu,
            &mut self.prompt_mode,
            &mut self.renderer,
            &mut self.child_activity,
        );
    }

    /// Send what a key handler asked for: steering text as `turn/steer`, an
    /// answer as the response to its request.
    async fn send_turn_command(&mut self, command: EngineCommand, tracker: &mut TurnTracker) {
        let EngineCommand::QueueNext { text } = command else {
            tracker.answer(command);
            // Its request was sent with the event the dialog showed, so it
            // is on the queue already if it isn't read yet.
            while let Some(message) = self.client.try_next_event() {
                tracker.observe_message(message);
            }
            self.send_ready_answers(tracker).await;
            return;
        };
        // Kept until the model receives it. If the turn has just ended the
        // host refuses it, and it comes back to the input box with the rest.
        tracker.steer_sent(text.clone());
        let turn = tracker.turn_id().to_string();
        let steered = self
            .client
            .request::<methods::TurnSteer>(TurnSteerParams {
                session_id: self.session.id.clone(),
                expected_turn_id: turn,
                input: text,
                client_message_id: None,
            })
            .await;
        if let Err(e) = steered {
            tracing::debug!("turn/steer: {e}");
        }
    }

    async fn send_ready_answers(&mut self, tracker: &mut TurnTracker) {
        for (id, response) in tracker.ready_answers() {
            if let Err(e) = self.client.resolve_server_request(id, response).await {
                tracing::warn!("an answer didn't reach Koda's host: {e}");
            }
        }
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
        let n = drain_bounded(&mut rx, 64, |item| seen.push(item));
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
        let n = drain_bounded(&mut rx, 64, |item| seen.push(item));
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
        let n = drain_bounded(&mut rx, 64, |item| seen.push(item));
        assert_eq!(n, 64, "drain must stop at cap, even with 1000 items queued");
        assert_eq!(seen.len(), 64);
        assert_eq!(seen.first(), Some(&0));
        assert_eq!(seen.last(), Some(&63));

        // The remaining 936 items are still in the channel, ready for the
        // next iteration. Verify by draining again with a huge cap.
        let mut more = Vec::new();
        let m = drain_bounded(&mut rx, 10_000, |item| more.push(item));
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
        let n = drain_bounded(&mut rx, 0, |item| seen.push(item));
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
            let n = drain_bounded(&mut rx, 64, |_| {});
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
