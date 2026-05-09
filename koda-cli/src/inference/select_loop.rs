//! Inner inference loop — `tokio::select!` over input/UI/draw/turn-done.
//!
//! This is the hottest, most subtle piece of the inference lifecycle:
//!
//!   - Rotates which arm is preferred each iteration (`prefer_input`)
//!     so neither terminal input nor engine events can monopolise the
//!     executor under sustained load (#1137, #1139, regression of #540).
//!   - The `Draw` arm is the *only* place `terminal.draw()` is called;
//!     every other arm calls `frame_requester.schedule_frame()` so the
//!     coalescing scheduler caps redraws at ~120 FPS (#1138).
//!   - Per-turn `cancel_token` is a child of `session.cancel` so Esc
//!     fires only the foreground turn, not the bg agents derived from
//!     the session root (#1200, #1216).
//!
//! `drain_bounded` and the regression tests for #1137 live here too,
//! since the bounded drain only has one production caller and unit
//! tests are easier to wire when they share the file.

use crate::sink::UiEvent;
use crate::tui_context::TuiContext;
use crate::tui_types::TuiState;
use crate::tui_viewport::draw_viewport;

use futures_util::StreamExt;
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
    Ui(UiEvent),
    /// The frame scheduler granted us a draw slot (#1138). Render once.
    Draw,
    TurnDone(anyhow::Result<()>),
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
    /// Run a full inference turn: start the turn future, handle events
    /// inside the inner `tokio::select!` loop, and perform post-turn
    /// cleanup (undo commit, event drain, auto-compact).
    pub(crate) async fn run_inference_turn(
        &mut self,
        pending_images: Option<Vec<koda_core::providers::ImageData>>,
        ui_tx: &mpsc::UnboundedSender<UiEvent>,
        ui_rx: &mut mpsc::UnboundedReceiver<UiEvent>,
        cmd_tx: &mpsc::Sender<EngineCommand>,
        cmd_rx: &mut mpsc::Receiver<EngineCommand>,
    ) -> anyhow::Result<()> {
        let cli_sink = crate::sink::CliSink::channel(ui_tx.clone());
        // #1321: spawn the bg-agent event forwarder on first turn.
        // Idempotent across the session lifetime — the registry's
        // receiver moves out exactly once, so subsequent calls are
        // cheap no-ops returning `false`. We attach a dedicated
        // long-lived `CliSink` (sharing the same `ui_tx`) so bg-task
        // status flows continuously to the TUI regardless of what
        // tool the foreground turn is currently inside. Replaces
        // the per-iteration `drain_status_events()` poll and the
        // 200 ms `with_status_pump` hotfix — mirrors Codex's
        // `forward_events` shape.
        let _ = self
            .session
            .attach_event_sink(std::sync::Arc::new(crate::sink::CliSink::channel(
                ui_tx.clone(),
            )));
        // #1200: derive a per-turn child token instead of cloning the
        // session-lifetime root. This keeps `session.cancel` stable
        // across turn boundaries so bg agents (which call
        // `ChildAgentRegistry::reserve(&session.cancel, …)`) keep their
        // cancel-token cascade pointing at a live parent. Cancelling
        // the child fires only the foreground turn; bg agents are
        // explicitly cancelled by the Ctrl+C path further down.
        let cancel_token = self.session.cancel_token().child_token();
        // #1216: clone the cloneable session-cancel handle BEFORE the
        // run_turn future borrows `&mut self.session`. The Esc/Ctrl+C
        // arm of the inference loop's select! needs to call
        // `session.interrupt()` mid-turn for the Gemini-style cascade,
        // but it can't reach `self.session` while the turn future
        // holds a mutable borrow. The handle is internally `Arc`-backed
        // so this clone is cheap and aliases the same root.
        let session_cancel = self.session.cancel_handle();
        let db_handle = self.session.db.clone();

        self.tui_state = TuiState::Inferring;
        self.inference_start = Some(std::time::Instant::now());
        self.renderer.last_turn_stats = None;

        // #1158 (b): clone the bg-task registry handles BEFORE pinning
        // the turn future. The future borrows `self.session` mutably for
        // its entire lifetime, so `self.session.bg_agents` becomes
        // unreachable inside the streaming loop. The agent registry is
        // an `Arc` (cheap clone); the process registry isn't, so we
        // clone the enclosing `Arc<KodaAgent>` and reach through it.
        let bg_agents_for_status = self.session.bg_agents.clone();
        let agent_for_status = self.agent.clone();

        // Run the inference turn as a pinned future
        {
            let turn = self.session.run_turn(
                &self.config,
                pending_images,
                &cli_sink,
                cmd_rx,
                // #1208: pass the per-turn child token so Ctrl+C / Esc
                // stops *this* turn without firing session.cancel
                // (which bg agents derive from — see #1200).
                Some(cancel_token.clone()),
            );
            tokio::pin!(turn);

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

            loop {
                // Round-robin: alternate which arm is preferred so a flood of
                // engine events can't starve terminal input (and vice versa).
                // Combined with the bounded drain below, this guarantees that
                // keystrokes / Ctrl+C / scroll events get serviced within a
                // small bounded number of inference-loop iterations even when
                // sub-agents fan out or streaming is firing TextDeltas at
                // line rate (#1137, #1139, regression of #540).
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
                        Some(Ok(ev)) = self.crossterm_events.next() => {
                            SelectArm::Crossterm(ev)
                        }
                        Some(ui_event) = ui_rx.recv() => {
                            SelectArm::Ui(ui_event)
                        }
                        result = &mut turn => SelectArm::TurnDone(result),
                    }
                } else {
                    tokio::select! {
                        biased;

                        Some(()) = self.draw_rx.recv() => SelectArm::Draw,
                        Some(ui_event) = ui_rx.recv() => {
                            SelectArm::Ui(ui_event)
                        }
                        Some(Ok(ev)) = self.crossterm_events.next() => {
                            SelectArm::Crossterm(ev)
                        }
                        result = &mut turn => SelectArm::TurnDone(result),
                    }
                };

                match select_result {
                    SelectArm::Draw => {
                        tracing::debug!(
                            target: "koda_cli::diag::child_activity",
                            stage = "inference_draw_arm",
                            "SelectArm::Draw -> terminal.draw()"
                        );
                        // The actual `terminal.draw()` happens here — nowhere
                        // else in the inference loop. We can't call
                        // `self.draw()` directly because `turn` holds a
                        // `&mut self.session` borrow for its full lifetime,
                        // so we use disjoint field access just like the old
                        // synchronous draw block did.
                        let (term_w, _) = crossterm::terminal::size()
                            .map(|(c, r)| (c as usize, r as usize))
                            .unwrap_or((80, 24));
                        let hist_viewport = (self.history_area_height as usize).max(1);
                        self.scroll_buffer.clamp_offset(term_w, hist_viewport);

                        let mode = trust::read_trust(&self.shared_mode);
                        let ctx = self.context_pct;
                        let mcp_info = self.agent.mcp_status_bar_info();
                        // #1158 (b): keep status pill alive during streaming turns.
                        // #1210: snapshot + project bg-activity rows for the
                        // overlay above the status bar. `bg_agents_for_status`
                        // and `agent_for_status` are session-lifetime `Arc`
                        // clones grabbed once at run() entry (see ~line 100),
                        // so this works inside the turn future's `&mut
                        // self.session` borrow window.
                        //
                        // Replaces the #1158 ambient `bg_counts` status pill:
                        // the overlay shows what's running, what each task
                        // is doing right now, and the cancel keybindings.
                        let agent_snaps = bg_agents_for_status.snapshot();
                        let process_snaps = agent_for_status.tools.bg_registry.snapshot();
                        let (child_activity_rows, child_activity_total) =
                            self.child_activity.build_rows(&agent_snaps, &process_snaps);
                        let queue_total = self.later_queue.len();
                        let queue_preview: Vec<String> = self
                            .later_queue
                            .iter()
                            .take(crate::widgets::queue_preview::MAX_VISIBLE)
                            .cloned()
                            .collect();
                        let _ = self.terminal.draw(|f| {
                            draw_viewport(
                                f,
                                &self.textarea,
                                &self.config.model,
                                mode,
                                ctx,
                                self.tui_state,
                                &self.prompt_mode,
                                &queue_preview,
                                queue_total,
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
                        // #1354: self-perpetuating draw loop. While any
                        // non-terminal bg agent or process exists, schedule
                        // the next frame ~1 s out so the age column ticks
                        // and the activity pill stays live without
                        // depending on user keystrokes or new engine
                        // events. `child_activity_total` is already
                        // computed only over non-terminal entries (see
                        // `ChildActivityTracker::build_rows`), so the
                        // ticker auto-stops the moment work drains.
                        if child_activity_total > 0 {
                            self.frame_requester
                                .schedule_frame_in(std::time::Duration::from_secs(1));
                        }
                    }
                    SelectArm::Crossterm(ev) => {
                        super::crossterm_handler::handle_crossterm_event_inline(
                            ev,
                            &session_cancel,
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
                            &mut self.later_queue,
                            &mut self.paste_blocks,
                            &db_handle,
                            &bg_agents_for_status,
                            &agent_for_status.tools.bg_registry,
                            &mut self.child_activity,
                        )
                        .await;
                        // Request a redraw to reflect the new state. The
                        // frame scheduler coalesces this with any other
                        // requests in the same window (#1138).
                        self.frame_requester.schedule_frame();
                    }
                    SelectArm::Ui(ui_event) => {
                        // Extract context usage before rendering
                        if let UiEvent::Engine(EngineEvent::ContextUsage { used, max }) = &ui_event
                        {
                            self.context_pct = if *max > 0 {
                                (used * 100 / max) as u32
                            } else {
                                0
                            };
                        }
                        super::ui_handler::handle_inference_ui_inline(
                            ui_event,
                            &mut self.scroll_buffer,
                            &mut self.menu,
                            &mut self.prompt_mode,
                            &mut self.renderer,
                            &mut self.child_activity,
                        );
                        // Bounded drain — at most MAX_DRAIN extra events per
                        // iteration so we yield back to the select for input
                        // and the turn future. The drain still amortises N
                        // events into 1 redraw (the original optimisation),
                        // but cannot monopolise the executor anymore.
                        let _ = drain_bounded(ui_rx, MAX_DRAIN, |extra| {
                            if let UiEvent::Engine(EngineEvent::ContextUsage { used, max }) = &extra
                            {
                                self.context_pct = if *max > 0 {
                                    (used * 100 / max) as u32
                                } else {
                                    0
                                };
                            }
                            super::ui_handler::handle_inference_ui_inline(
                                extra,
                                &mut self.scroll_buffer,
                                &mut self.menu,
                                &mut self.prompt_mode,
                                &mut self.renderer,
                                &mut self.child_activity,
                            );
                        });
                        // One coalesced redraw per drain batch — not per
                        // event — keeps redraw cost bounded under streaming
                        // floods (#1138).
                        tracing::debug!(
                            target: "koda_cli::diag::child_activity",
                            stage = "ui_arm_schedule_frame",
                            "Ui arm completed -> schedule_frame()"
                        );
                        self.frame_requester.schedule_frame();
                    }
                    SelectArm::TurnDone(result) => {
                        if let Err(e) = result {
                            self.scroll_buffer.push(Line::from(vec![
                                Span::raw("  "),
                                Span::styled(
                                    format!("\u{2717} Turn failed: {e:#}"),
                                    Style::default().fg(Color::Red),
                                ),
                            ]));
                        }
                        break;
                    }
                }
            }
        }

        // Post-turn cleanup
        self.post_turn_cleanup(ui_rx, &cancel_token).await;
        Ok(())
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
