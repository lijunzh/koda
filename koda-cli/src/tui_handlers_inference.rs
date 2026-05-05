//! Inference turn lifecycle — inner event loop + post-turn cleanup.
//!
//! Extracted from `TuiContext::run_event_loop()` (Step 3a, #447).
//! Handles: running the turn future, approval/loop-cap hotkeys,
//! engine event rendering, feedback input, post-turn compaction.

use crate::input;
use crate::scroll_buffer::ScrollBuffer;
use crate::sink::UiEvent;
use crate::tui_context::TuiContext;
use crate::tui_types::{MenuContent, PromptMode, TuiState};
use crate::tui_viewport::draw_viewport;

use crossterm::event::{Event, KeyCode, KeyModifiers};
use futures_util::StreamExt;
use koda_core::engine::{ApprovalDecision, EngineCommand, EngineEvent};
use koda_core::persistence::Persistence;
use koda_core::trust::{self, TrustMode};
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
    Crossterm(Event),
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
                    }
                    SelectArm::Crossterm(ev) => {
                        handle_crossterm_event_inline(
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
                        handle_inference_ui_inline(
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
                            handle_inference_ui_inline(
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

    // ── Post-turn cleanup ──────────────────────────────────────

    async fn post_turn_cleanup(
        &mut self,
        ui_rx: &mut mpsc::UnboundedReceiver<UiEvent>,
        turn_cancel: &tokio_util::sync::CancellationToken,
    ) {
        // If the turn was cancelled, clear the later_queue so deferred
        // messages don’t immediately fire a new turn that may block on a
        // single-slot local server (LM Studio, ollama) (#825).
        //
        // #1200: check the per-turn child token, not `session.cancel`.
        // The session root is now stable across turn boundaries so bg
        // agents derived from it keep their cancel cascade live; the
        // child is the one that actually fires on Ctrl+C.
        if turn_cancel.is_cancelled() && !self.later_queue.is_empty() {
            let n = self.later_queue.len();
            self.later_queue.clear();
            self.scroll_buffer.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("\u{1f6ab} Cleared {n} deferred message(s)"),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }

        self.tui_state = TuiState::Idle;
        self.inference_start = None;
        // #1200: do NOT replace `session.cancel`. The session root is
        // session-lifetime stable now; per-turn cancellation is on a
        // child token that's dropped when the turn future ends. This
        // fixes the cascade-broken-across-turns bug where bg agents
        // reserved during turn N were orphaned the moment turn N
        // finished and `session.cancel` was swapped out from under them.

        // Commit undo snapshots for this turn
        if let Ok(mut undo) = self.agent.tools.undo.lock() {
            undo.commit_turn();
        }

        // Drain remaining UI events
        while let Ok(UiEvent::Engine(e)) = ui_rx.try_recv() {
            if let EngineEvent::ContextUsage { used, max } = &e {
                self.context_pct = if *max > 0 {
                    (used * 100 / max) as u32
                } else {
                    0
                };
            }
            // Tap activity events into the bg-activity tracker even
            // during post-turn drain — a fan-out agent emitting a
            // ToolStart milliseconds before the turn future resolves
            // would otherwise vanish from the overlay forever (#1210).
            if let EngineEvent::ChildAgentActivity { task_id, kind, .. } = &e {
                self.child_activity.record_activity(*task_id, kind);
            }
            self.renderer.render_to_buffer(e, &mut self.scroll_buffer);
        }

        // Auto-compact
        self.maybe_auto_compact().await;
    }

    async fn maybe_auto_compact(&mut self) {
        let ctx_pct = self.context_pct as usize;
        if ctx_pct < koda_core::inference_helpers::AUTO_COMPACT_THRESHOLD {
            return;
        }

        let pending = self
            .session
            .db
            .has_pending_tool_calls(&self.session.id)
            .await
            .unwrap_or(false);

        if pending {
            if !self.silent_compact_deferred {
                self.scroll_buffer.push(
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(
                            format!(
                                "\u{1f43b} Context at {ctx_pct}% \u{2014} deferring compact (tool calls pending)"
                            ),
                            Style::default().fg(Color::Yellow),
                        ),
                    ]),
                );
                self.silent_compact_deferred = true;
            }
            return;
        }

        self.silent_compact_deferred = false;
        self.scroll_buffer.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("\u{1f43b} Context at {ctx_pct}% \u{2014} auto-compacting..."),
                Style::default().fg(Color::Cyan),
            ),
        ]));

        match koda_core::compact::compact_session(
            &self.session.db,
            &self.session.id,
            self.config.max_context_tokens,
            &self.config.model_settings,
            &self.provider,
        )
        .await
        {
            Ok(Ok(result)) => {
                self.scroll_buffer.push(Line::styled(
                    format!(
                        "  \u{2713} Compacted {} messages \u{2192} ~{} tokens",
                        result.deleted, result.summary_tokens
                    ),
                    Style::default().fg(Color::Green),
                ));
            }
            Ok(Err(_skip)) => {} // silently skip
            Err(e) => {
                self.scroll_buffer.push(Line::styled(
                    format!("  \u{2717} Auto-compact failed: {e:#}"),
                    Style::default().fg(Color::Red),
                ));
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Free functions that take individual fields to avoid &mut self borrow
// conflicts with the pinned `turn` future.
// ─────────────────────────────────────────────────────────────────────

// ─────────────────────────────────────────────────────────────────────

/// Route a crossterm event during inference (field-level borrows).
///
/// Extracted from the inline `tokio::select!` arm so the select body
/// stays small and readable.
#[allow(clippy::too_many_arguments)]
async fn handle_crossterm_event_inline(
    ev: Event,
    session_cancel: &koda_core::session::SessionCancel,
    cmd_tx: &mpsc::Sender<EngineCommand>,
    scroll_buffer: &mut ScrollBuffer,
    hist_h: usize,
    menu: &mut MenuContent,
    prompt_mode: &mut PromptMode,
    pending_approval_id: &mut Option<String>,
    textarea: &mut crate::composer::textarea::TextArea,
    shared_mode: &koda_core::trust::SharedTrustMode,
    completer: &mut crate::completer::InputCompleter,
    history: &mut Vec<String>,
    history_idx: &mut Option<usize>,
    later_queue: &mut std::collections::VecDeque<String>,
    paste_blocks: &mut Vec<input::PasteBlock>,
    db: &koda_core::db::Database,
    bg_agents: &koda_core::child_agent::ChildAgentRegistry,
    bg_processes: &koda_core::tools::bg_process::BgRegistry,
    child_activity: &mut crate::child_activity::ChildActivityTracker,
) {
    use crossterm::event::MouseEventKind;
    match ev {
        Event::Resize(_, _) => {
            let (w, _) = crossterm::terminal::size()
                .map(|(c, r)| (c as usize, r as usize))
                .unwrap_or((80, 24));
            // Use hist_h (history panel height), not full terminal height.
            scroll_buffer.clamp_offset(w, hist_h);
        }
        Event::Mouse(mouse) => {
            let (w, _) = crossterm::terminal::size()
                .map(|(c, r)| (c as usize, r as usize))
                .unwrap_or((80, 24));
            match mouse.kind {
                MouseEventKind::ScrollUp => scroll_buffer.scroll_up(3, w, hist_h),
                MouseEventKind::ScrollDown => scroll_buffer.scroll_down(3),
                _ => {}
            }
        }
        Event::Paste(text) => {
            let char_count = text.chars().count();
            if char_count < input::PASTE_BLOCK_THRESHOLD {
                textarea.insert_str(&text);
            } else {
                paste_blocks.push(input::PasteBlock {
                    content: text,
                    char_count,
                });
            }
        }
        Event::Key(key) => {
            handle_inference_key_inline(
                key,
                session_cancel,
                cmd_tx,
                scroll_buffer,
                menu,
                prompt_mode,
                pending_approval_id,
                textarea,
                shared_mode,
                completer,
                history,
                history_idx,
                later_queue,
                db,
                bg_agents,
                bg_processes,
                child_activity,
            )
            .await;
        }
        _ => {}
    }
}

/// Handle a key event during inference (field-level borrows).
#[allow(clippy::too_many_arguments)]
async fn handle_inference_key_inline(
    key: crossterm::event::KeyEvent,
    session_cancel: &koda_core::session::SessionCancel,
    cmd_tx: &mpsc::Sender<EngineCommand>,
    scroll_buffer: &mut ScrollBuffer,
    menu: &mut MenuContent,
    prompt_mode: &mut PromptMode,
    pending_approval_id: &mut Option<String>,
    textarea: &mut crate::composer::textarea::TextArea,
    shared_mode: &koda_core::trust::SharedTrustMode,
    completer: &mut crate::completer::InputCompleter,
    history: &mut Vec<String>,
    history_idx: &mut Option<usize>,
    later_queue: &mut std::collections::VecDeque<String>,
    db: &koda_core::db::Database,
    bg_agents: &koda_core::child_agent::ChildAgentRegistry,
    bg_processes: &koda_core::tools::bg_process::BgRegistry,
    child_activity: &mut crate::child_activity::ChildActivityTracker,
) {
    // Vim insert-mode Escape (PR 3 of #1178). Same rationale as in
    // `tui_context::events::handle_key`: route bare Esc to the textarea
    // for the INSERT → NORMAL transition before the inference-loop's
    // "Esc cancels inference" handler grabs it. The textarea decides via
    // `should_handle_vim_insert_escape` so this file does not need to
    // know whether vim is enabled.
    if textarea.should_handle_vim_insert_escape(key) {
        textarea.input(key);
        return;
    }

    // Approval hotkeys
    if let MenuContent::Approval { id, .. } = menu {
        let approval_id = id.clone();
        let decision = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Some(ApprovalDecision::Approve),
            KeyCode::Char('n') | KeyCode::Char('N') => Some(ApprovalDecision::Reject),
            KeyCode::Char('a') | KeyCode::Char('A') => {
                // Sandbox-aware Auto switch (#860 hard refusal). If the
                // kernel sandbox is unavailable we skip the trust flip
                // but still honor the approval as a one-shot — graceful
                // degradation rather than blocking the user mid-prompt.
                // The status-bar sandbox indicator (🛡/⚠) plus the
                // sandbox state in `koda --version` make the underlying
                // state visible; the warn-level log line here aids
                // debugging when users wonder why pressing 'a' didn't
                // persist.
                if let Err(msg) = trust::set_trust_checked(
                    shared_mode,
                    TrustMode::Auto,
                    koda_core::sandbox::is_available(),
                ) {
                    tracing::warn!(
                        "Approve+Always pressed but Auto refused: {msg} \u{2014} approving one-shot only"
                    );
                }
                Some(ApprovalDecision::Approve)
            }
            KeyCode::Char('f') | KeyCode::Char('F') => {
                *prompt_mode = PromptMode::WizardInput {
                    label: "Feedback".into(),
                    mask: false,
                };
                *menu = MenuContent::WizardTrail(vec![(
                    "Action".into(),
                    "Rejected with feedback".into(),
                )]);
                *pending_approval_id = Some(approval_id.clone());
                textarea.set_text_clearing_elements("");
                None
            }
            KeyCode::Esc => Some(ApprovalDecision::Reject),
            _ => None,
        };
        if let Some(d) = decision {
            *menu = MenuContent::None;
            let _ = cmd_tx
                .send(EngineCommand::ApprovalResponse {
                    id: approval_id,
                    decision: d,
                })
                .await;
        }
        return;
    }

    // Loop cap hotkeys
    if matches!(menu, MenuContent::LoopCap) {
        let action = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                Some(koda_core::loop_guard::LoopContinuation::Continue200)
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                Some(koda_core::loop_guard::LoopContinuation::Stop)
            }
            _ => None,
        };
        if let Some(a) = action {
            *menu = MenuContent::None;
            let _ = cmd_tx.send(EngineCommand::LoopDecision { action: a }).await;
        }
        return;
    }

    // AskUser: freeform text input during inference.
    // id is embedded in MenuContent::AskUser — no separate pending field needed.
    if matches!(menu, MenuContent::AskUser { .. })
        && matches!(prompt_mode, PromptMode::WizardInput { .. })
    {
        match (key.code, key.modifiers) {
            (KeyCode::Enter, m) if m.contains(KeyModifiers::ALT) => {
                textarea.insert_str("\n");
            }
            (KeyCode::Enter, KeyModifiers::NONE) => {
                let answer = textarea.text().to_string();
                textarea.set_text_clearing_elements("");
                *prompt_mode = PromptMode::Chat;
                if let MenuContent::AskUser { id, .. } = std::mem::replace(menu, MenuContent::None)
                {
                    let _ = cmd_tx
                        .send(EngineCommand::AskUserResponse { id, answer })
                        .await;
                }
            }
            (KeyCode::Esc, _) => {
                textarea.set_text_clearing_elements("");
                *prompt_mode = PromptMode::Chat;
                if let MenuContent::AskUser { id, .. } = std::mem::replace(menu, MenuContent::None)
                {
                    let _ = cmd_tx
                        .send(EngineCommand::AskUserResponse {
                            id,
                            answer: String::new(),
                        })
                        .await;
                }
            }
            _ => {
                textarea.input(key);
            }
        }
        return;
    }

    // Feedback text input during inference
    if matches!(prompt_mode, PromptMode::WizardInput { .. }) && pending_approval_id.is_some() {
        match (key.code, key.modifiers) {
            (KeyCode::Enter, m) if m.contains(KeyModifiers::ALT) => {
                textarea.insert_str("\n");
            }
            (KeyCode::Enter, KeyModifiers::NONE) => {
                let feedback = textarea.text().to_string();
                textarea.set_text_clearing_elements("");
                *prompt_mode = PromptMode::Chat;
                *menu = MenuContent::None;
                if let Some(aid) = pending_approval_id.take() {
                    let decision = if feedback.trim().is_empty() {
                        ApprovalDecision::Reject
                    } else {
                        ApprovalDecision::RejectWithFeedback { feedback }
                    };
                    let _ = cmd_tx
                        .send(EngineCommand::ApprovalResponse { id: aid, decision })
                        .await;
                }
            }
            (KeyCode::Esc, _) => {
                textarea.set_text_clearing_elements("");
                *prompt_mode = PromptMode::Chat;
                *menu = MenuContent::None;
                if let Some(aid) = pending_approval_id.take() {
                    let _ = cmd_tx
                        .send(EngineCommand::ApprovalResponse {
                            id: aid,
                            decision: ApprovalDecision::Reject,
                        })
                        .await;
                }
            }
            _ => {
                textarea.input(key);
            }
        }
        return;
    }

    // General keys during inference
    match (key.code, key.modifiers) {
        (KeyCode::Enter, m) if m.contains(KeyModifiers::ALT) => {
            textarea.insert_str("\n");
        }
        // Enter during inference — default lane is "next" (mid-turn steer).
        // The text is sent directly to the engine as QueueNext; it will be
        // injected before the next provider request in the current turn.
        (KeyCode::Enter, KeyModifiers::NONE) => {
            let text = textarea.text().to_string();
            if !text.trim().is_empty() {
                // #1211: reject slash-command attempts before they
                // silently steer. `/cancel`, `/clear` etc. only run
                // through `dispatch_command` at idle; during inference
                // the queue-as-text path would inject them as raw user
                // input. Surface a visible warning so the user can
                // either Esc-interrupt or strip the leading slash to
                // queue as a real steer message.
                if is_slash_command_attempt(&text) {
                    textarea.set_text_clearing_elements("");
                    crate::tui_output::warn_msg(
                        scroll_buffer,
                        "Slash commands are disabled during inference. \
                         Press Esc to interrupt, then run the command at idle."
                            .into(),
                    );
                    return;
                }
                textarea.set_text_clearing_elements("");
                history.push(text.clone());
                let _ = db.history_push(&text).await;
                *history_idx = None;

                let preview = truncate_preview(&text, 80);
                scroll_buffer.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled("\u{1f4e5} Next: ", Style::default().fg(Color::Green)),
                    Span::styled(preview, Style::default().fg(Color::DarkGray)),
                ]));

                let _ = cmd_tx.send(EngineCommand::QueueNext { text }).await;
            }
        }
        // Ctrl+J during inference — "later" lane: defer text until after the
        // current turn fully completes, then batch with other later items into
        // one new turn.
        (KeyCode::Char('j'), m) if m.contains(KeyModifiers::CONTROL) => {
            let text = textarea.text().to_string();
            if !text.trim().is_empty() {
                // #1211: same slash guard as the QueueNext path. A
                // slash command queued for "later" would still arrive
                // at the model as raw text — same silent-steer footgun,
                // same fix.
                if is_slash_command_attempt(&text) {
                    textarea.set_text_clearing_elements("");
                    crate::tui_output::warn_msg(
                        scroll_buffer,
                        "Slash commands are disabled during inference. \
                         Press Esc to interrupt, then run the command at idle."
                            .into(),
                    );
                    return;
                }
                textarea.set_text_clearing_elements("");
                history.push(text.clone());
                let _ = db.history_push(&text).await;
                *history_idx = None;

                let preview = truncate_preview(&text, 80);
                if let Some(later_n) = crate::queue_lanes::enqueue_later(later_queue, text) {
                    scroll_buffer.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(
                            format!("\u{1f4cb} Later ({later_n}): "),
                            Style::default().fg(Color::Yellow),
                        ),
                        Span::styled(preview, Style::default().fg(Color::DarkGray)),
                    ]));
                }
            }
        }
        (KeyCode::Esc, _) => {
            // #1216: Gemini-style cascade. `session.interrupt()` fires
            // the session-lifetime cancel root — collapsing the
            // per-turn `cancel_token` (it's a child), every bg agent's
            // per-task cancel (also children), and every nested bg
            // agent transitively — then atomically swaps in a fresh
            // root so the next turn isn't born cancelled.
            //
            // We still call `cancel_all_bg_work` for its `mark_cancelling`
            // side effect (flips overlay icons red instantly so the user
            // gets visual feedback even before the underlying status
            // transition propagates through the watch channel).
            session_cancel.interrupt();
            crate::tui_bg_tasks::cancel_all_bg_work(
                scroll_buffer,
                bg_agents,
                bg_processes,
                child_activity,
            );
        }
        (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
            // Same cascade as Esc — during inference both keys are
            // intentional aliases. The asymmetry shows up at idle
            // (Esc dismisses popups, Ctrl+C arms quit), see the idle
            // handler in `tui_context::events`.
            session_cancel.interrupt();
            crate::tui_bg_tasks::cancel_all_bg_work(
                scroll_buffer,
                bg_agents,
                bg_processes,
                child_activity,
            );
        }
        // Ctrl+U: clear the later_queue (deferred messages) without cancelling inference.
        (KeyCode::Char('u'), m) if m.contains(KeyModifiers::CONTROL) => {
            let n = crate::queue_lanes::clear_later(later_queue);
            if n > 0 {
                scroll_buffer.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        format!("\u{1f6ab} Cleared {n} deferred message(s)"),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
        }
        (KeyCode::BackTab, _) => {
            // Sandbox-aware cycle (#860 hard refusal). Keep the user
            // in the current mode and surface a visible warning if
            // Auto isn't safe to enter on this system.
            if let Err(msg) =
                trust::cycle_trust_checked(shared_mode, koda_core::sandbox::is_available())
            {
                scroll_buffer.push(Line::styled(
                    format!("\u{26a0} {msg}"),
                    Style::default().fg(Color::Yellow),
                ));
            }
        }
        // Up during inference: pop the last later_queue item back into the
        // editor so the user can edit it before re-submitting.
        // Falls back to normal textarea movement when the queue is empty.
        (KeyCode::Up, KeyModifiers::NONE) => {
            if let Some(popped) = crate::queue_lanes::pop_later(later_queue) {
                textarea.set_text_clearing_elements("");
                textarea.insert_str(&popped);
                scroll_buffer.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        "\u{21a9} Popped from later queue",
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            } else {
                textarea.input(key);
            }
        }
        (KeyCode::Tab, KeyModifiers::NONE) => {
            let current = textarea.text().to_string();
            if let Some(completed) = completer.complete(&current) {
                textarea.set_text_clearing_elements("");
                textarea.insert_str(&completed);
            }
        }
        _ => {
            completer.reset();
            textarea.input(key);
        }
    }
}

/// Detect a slash-command attempt typed during inference (#1211).
///
/// Slash commands (`/cancel`, `/clear`, `/model`, …) are routed through
/// `dispatch_command` only at idle. During inference the Enter handler
/// would otherwise silently queue the text as a steer message — the
/// model would receive raw `/cancel agent:1` as user input and the
/// command would never actually run. Trim leading whitespace so a
/// stray space doesn't mask the slash; trailing content is irrelevant
/// (a real word starting with `/` like a path `/etc/hosts` is still
/// rare enough mid-inference that the false positive is acceptable —
/// the warning tells the user how to recover).
fn is_slash_command_attempt(text: &str) -> bool {
    text.trim_start().starts_with('/')
}

/// Truncate a string to `max_chars`, replacing newlines with ↵ and
/// appending "…" if it was shortened.
/// Truncate a string to `max_chars`, replacing newlines with ↵ and
/// appending "…" if it was shortened.
fn truncate_preview(s: &str, max_chars: usize) -> String {
    let flat: String = s.chars().map(|c| if c == '\n' { '↵' } else { c }).collect();
    if flat.len() <= max_chars {
        flat
    } else {
        let mut out: String = flat.chars().take(max_chars.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Handle a UI event during inference (field-level borrows).
fn handle_inference_ui_inline(
    ui_event: UiEvent,
    buffer: &mut ScrollBuffer,
    menu: &mut MenuContent,
    prompt_mode: &mut PromptMode,
    renderer: &mut crate::tui_render::TuiRenderer,
    child_activity: &mut crate::child_activity::ChildActivityTracker,
) {
    match ui_event {
        UiEvent::Engine(EngineEvent::AskUserRequest {
            id,
            question,
            options,
        }) => {
            *prompt_mode = PromptMode::WizardInput {
                label: "Answer".into(),
                mask: false,
            };
            *menu = MenuContent::AskUser {
                id,
                question,
                options,
            };
        }
        UiEvent::Engine(EngineEvent::ApprovalRequest {
            id,
            tool_name,
            detail,
            preview,
            ..
        }) => {
            if preview.is_some() {
                renderer.preview_shown = true;
            }
            if let Some(ref prev) = preview {
                let diff_lines = crate::diff_render::render_lines(prev);
                let gutter = crate::diff_render::GUTTER_WIDTH;
                for line in diff_lines {
                    buffer.push_with_gutter(line, gutter);
                }
            }
            *menu = MenuContent::Approval {
                id,
                tool_name,
                detail,
            };
        }
        UiEvent::Engine(EngineEvent::LoopCapReached { cap, recent_tools }) => {
            buffer.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("\u{26a0} Hard cap reached ({cap} iterations)"),
                    Style::default().fg(Color::Yellow),
                ),
            ]));
            for name in &recent_tools {
                buffer.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(
                        format!("\u{25cf} {name}"),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
            *menu = MenuContent::LoopCap;
        }
        UiEvent::Engine(event) => {
            // Tap bg-activity events into the live tracker BEFORE the
            // renderer no-ops them (#1207 dropped scroll output for
            // ChildAgentActivity; #1210 routes them to the activity
            // overlay instead). Done here so every fall-through engine
            // event passes through the same tap point — no risk of a
            // future variant-specific arm bypassing it.
            if let EngineEvent::ChildAgentActivity { task_id, kind, .. } = &event {
                child_activity.record_activity(*task_id, kind);
            }
            renderer.render_to_buffer(event, buffer);
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

#[cfg(test)]
mod slash_guard_tests {
    //! Regression tests for #1211 — slash commands typed during
    //! inference must not silently steer.
    //!
    //! These pin the classifier (`is_slash_command_attempt`) that the
    //! Enter (QueueNext) and Ctrl+J (later) handlers consult before
    //! sending. The handlers themselves are deeply entangled with
    //! `TuiContext` field-borrows and the engine command channel — the
    //! cleanest unit-level surface is the classifier, so we keep these
    //! tests focused there. End-to-end coverage of the rejection path
    //! belongs in a future scripted-TUI test (none exists yet).

    use super::is_slash_command_attempt;

    #[test]
    fn bare_slash_command_is_detected() {
        assert!(is_slash_command_attempt("/cancel"));
        assert!(is_slash_command_attempt("/clear"));
        assert!(is_slash_command_attempt("/model"));
    }

    #[test]
    fn slash_command_with_args_is_detected() {
        // The canonical post-#1213 victim: `/cancel <id>` typed during
        // a WaitTask. Without the guard this becomes a steer message.
        assert!(is_slash_command_attempt("/cancel agent:1"));
        assert!(is_slash_command_attempt("/model gpt-4o"));
    }

    #[test]
    fn leading_whitespace_does_not_bypass_guard() {
        // A stray space is overwhelmingly more likely to be a typo on
        // a slash command than a real intent to steer with leading
        // whitespace. Trim before classifying so the guard isn't
        // trivially defeated.
        assert!(is_slash_command_attempt(" /cancel"));
        assert!(is_slash_command_attempt("  /clear"));
        assert!(is_slash_command_attempt("\t/model gpt-4o"));
    }

    #[test]
    fn plain_text_is_allowed_through() {
        // The whole point of QueueNext during inference is mid-turn
        // steers — those must keep working unmolested.
        assert!(!is_slash_command_attempt("please use rust 2024 edition"));
        assert!(!is_slash_command_attempt("actually, skip the test for now"));
        assert!(!is_slash_command_attempt(""));
        assert!(!is_slash_command_attempt("   "));
    }

    #[test]
    fn mid_text_slash_is_not_a_command() {
        // A slash mid-message is a path / URL / regex / fraction —
        // not a command. Only a leading `/` (after trim) counts.
        assert!(!is_slash_command_attempt("check /etc/hosts for the entry"));
        assert!(!is_slash_command_attempt("see https://example.com/path"));
        assert!(!is_slash_command_attempt("the ratio is 1/2"));
    }
}
