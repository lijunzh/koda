//! Inference turn lifecycle — inner event loop + post-turn cleanup.
//!
//! Extracted from `TuiContext::run_event_loop()` (Step 3a, #447).
//! Handles: running the turn future, approval/loop-cap hotkeys,
//! engine event rendering, feedback input, post-turn compaction.

use crate::input;
use crate::sink::UiEvent;
use crate::tui_context::{TuiContext, save_history};
use crate::tui_types::{MenuContent, PromptMode, TuiState};
use crate::tui_viewport::{
    drain_pending_resizes, draw_viewport, emit_above, scroll_past_and_reinit,
};

use crossterm::event::{Event, KeyCode, KeyModifiers};
use futures_util::StreamExt;
use koda_core::approval::{self, ApprovalMode};
use koda_core::engine::{ApprovalDecision, EngineCommand, EngineEvent};
use koda_core::persistence::Persistence;
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use tokio::sync::mpsc;

impl TuiContext {
    /// Run a full inference turn: start the turn future, handle events
    /// inside the inner `tokio::select!` loop, and perform post-turn
    /// cleanup (undo commit, event drain, auto-compact).
    pub(crate) async fn run_inference_turn(
        &mut self,
        pending_images: Option<Vec<koda_core::providers::ImageData>>,
        ui_tx: &mpsc::Sender<UiEvent>,
        ui_rx: &mut mpsc::Receiver<UiEvent>,
        cmd_tx: &mpsc::Sender<EngineCommand>,
        cmd_rx: &mut mpsc::Receiver<EngineCommand>,
    ) -> anyhow::Result<()> {
        let cli_sink = crate::sink::CliSink::channel(ui_tx.clone());
        let cancel_token = self.session.cancel.clone();

        self.tui_state = TuiState::Inferring;
        self.inference_start = Some(std::time::Instant::now());
        self.renderer.last_turn_stats = None;

        // Run the inference turn as a pinned future
        {
            let turn = self
                .session
                .run_turn(&self.config, pending_images, &cli_sink, cmd_rx);
            tokio::pin!(turn);

            loop {
                // Redraw viewport (swallow DSR timeout errors during resize)
                let mode = approval::read_mode(&self.shared_mode);
                let ctx = koda_core::context::percentage() as u32;
                let _ = self.terminal.draw(|f| {
                    draw_viewport(
                        f,
                        &self.textarea,
                        &self.config.model,
                        mode,
                        ctx,
                        self.tui_state,
                        &self.prompt_mode,
                        self.input_queue.len(),
                        self.inference_start
                            .map(|s| s.elapsed().as_secs())
                            .unwrap_or(0),
                        self.renderer.last_turn_stats.as_ref(),
                        &self.menu,
                    );
                });

                tokio::select! {
                    result = &mut turn => {
                        if let Err(e) = result {
                            emit_above(
                                &mut self.terminal,
                                Line::from(vec![
                                    Span::raw("  "),
                                    Span::styled(
                                        format!("\u{2717} Turn failed: {e:#}"),
                                        Style::default().fg(Color::Red),
                                    ),
                                ]),
                            );
                        }
                        break;
                    }
                    Some(Ok(ev)) = self.crossterm_events.next() => {
                        // Inline: field-level borrows to satisfy borrow checker
                        // (turn holds &mut self.session, so we can't call &mut self methods)
                        match ev {
                            Event::Resize(_, _) => {
                                let _ = drain_pending_resizes(&mut self.crossterm_events);
                                scroll_past_and_reinit(
                                    &mut self.terminal,
                                    &mut self.crossterm_events,
                                    self.viewport_height,
                                )?;
                                emit_above(
                                    &mut self.terminal,
                                    Line::from(vec![
                                        Span::styled("  \u{26a0} ", Style::default().fg(Color::Yellow)),
                                        Span::styled(
                                            "Terminal resized \u{2014} visual artifacts may appear above. Press Ctrl+L to refresh.",
                                            Style::default().fg(Color::DarkGray),
                                        ),
                                    ]),
                                );
                            }
                            Event::Paste(text) => {
                                let char_count = text.chars().count();
                                if char_count < input::PASTE_BLOCK_THRESHOLD {
                                    self.textarea.insert_str(&text);
                                } else {
                                    self.paste_blocks.push(input::PasteBlock {
                                        content: text,
                                        char_count,
                                    });
                                }
                            }
                            Event::Key(key) => {
                                handle_inference_key_inline(
                                    key,
                                    &cancel_token,
                                    cmd_tx,
                                    &mut self.menu,
                                    &mut self.prompt_mode,
                                    &mut self.pending_approval_id,
                                    &mut self.textarea,
                                    &self.shared_mode,
                                    &mut self.completer,
                                    &mut self.history,
                                    &mut self.history_idx,
                                    &mut self.input_queue,
                                ).await;
                            }
                            _ => {}
                        }
                    }
                    Some(ui_event) = ui_rx.recv() => {
                        handle_inference_ui_inline(
                            ui_event,
                            &mut self.terminal,
                            &mut self.menu,
                            &mut self.renderer,
                        );
                    }
                }
            }
        }

        // Post-turn cleanup
        self.post_turn_cleanup(ui_rx).await;
        Ok(())
    }

    // ── Post-turn cleanup ──────────────────────────────────────

    async fn post_turn_cleanup(&mut self, ui_rx: &mut mpsc::Receiver<UiEvent>) {
        self.tui_state = TuiState::Idle;
        self.inference_start = None;
        self.session.cancel = tokio_util::sync::CancellationToken::new();

        // Commit undo snapshots for this turn
        if let Ok(mut undo) = self.agent.tools.undo.lock() {
            undo.commit_turn();
        }

        // Drain remaining UI events
        while let Ok(UiEvent::Engine(e)) = ui_rx.try_recv() {
            self.renderer.render_to_terminal(e, &mut self.terminal);
        }

        // Auto-compact
        self.maybe_auto_compact().await;
    }

    async fn maybe_auto_compact(&mut self) {
        if self.config.auto_compact_threshold == 0 {
            return;
        }

        let ctx_pct = koda_core::context::percentage();
        if ctx_pct < self.config.auto_compact_threshold {
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
                emit_above(
                    &mut self.terminal,
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
        emit_above(
            &mut self.terminal,
            Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("\u{1f43b} Context at {ctx_pct}% \u{2014} auto-compacting..."),
                    Style::default().fg(Color::Cyan),
                ),
            ]),
        );

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
                emit_above(
                    &mut self.terminal,
                    Line::styled(
                        format!(
                            "  \u{2713} Compacted {} messages \u{2192} ~{} tokens",
                            result.deleted, result.summary_tokens
                        ),
                        Style::default().fg(Color::Green),
                    ),
                );
            }
            Ok(Err(_skip)) => {} // silently skip
            Err(e) => {
                emit_above(
                    &mut self.terminal,
                    Line::styled(
                        format!("  \u{2717} Auto-compact failed: {e:#}"),
                        Style::default().fg(Color::Red),
                    ),
                );
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Free functions that take individual fields to avoid &mut self borrow
// conflicts with the pinned `turn` future.
// ─────────────────────────────────────────────────────────────────────

/// Handle a key event during inference (field-level borrows).
#[allow(clippy::too_many_arguments)]
async fn handle_inference_key_inline(
    key: crossterm::event::KeyEvent,
    cancel_token: &tokio_util::sync::CancellationToken,
    cmd_tx: &mpsc::Sender<EngineCommand>,
    menu: &mut MenuContent,
    prompt_mode: &mut PromptMode,
    pending_approval_id: &mut Option<String>,
    textarea: &mut ratatui_textarea::TextArea<'static>,
    shared_mode: &koda_core::approval::SharedMode,
    completer: &mut crate::completer::InputCompleter,
    history: &mut Vec<String>,
    history_idx: &mut Option<usize>,
    input_queue: &mut std::collections::VecDeque<String>,
) {
    // Approval hotkeys
    if let MenuContent::Approval { id, .. } = menu {
        let approval_id = id.clone();
        let decision = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Some(ApprovalDecision::Approve),
            KeyCode::Char('n') | KeyCode::Char('N') => Some(ApprovalDecision::Reject),
            KeyCode::Char('a') | KeyCode::Char('A') => {
                approval::set_mode(shared_mode, ApprovalMode::Auto);
                Some(ApprovalDecision::Approve)
            }
            KeyCode::Char('f') | KeyCode::Char('F') => {
                *prompt_mode = PromptMode::WizardInput {
                    label: "Feedback".into(),
                    masked: false,
                };
                *menu = MenuContent::WizardTrail(vec![(
                    "Action".into(),
                    "Rejected with feedback".into(),
                )]);
                *pending_approval_id = Some(approval_id.clone());
                textarea.select_all();
                textarea.cut();
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

    // Feedback text input during inference
    if matches!(prompt_mode, PromptMode::WizardInput { .. }) && pending_approval_id.is_some() {
        match key.code {
            KeyCode::Enter => {
                let feedback = textarea.lines().join("\n");
                textarea.select_all();
                textarea.cut();
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
            KeyCode::Esc => {
                textarea.select_all();
                textarea.cut();
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
                textarea.input(Event::Key(key));
            }
        }
        return;
    }

    // General keys during inference
    match (key.code, key.modifiers) {
        (KeyCode::Enter, KeyModifiers::NONE) => {
            let text = textarea.lines().join("\n");
            if !text.trim().is_empty() {
                textarea.select_all();
                textarea.cut();
                history.push(text.clone());
                save_history(history);
                *history_idx = None;
                input_queue.push_back(text);
            }
        }
        (KeyCode::Esc, _) => {
            cancel_token.cancel();
        }
        (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
            cancel_token.cancel();
        }
        (KeyCode::BackTab, _) => {
            approval::cycle_mode(shared_mode);
        }
        (KeyCode::Tab, KeyModifiers::NONE) => {
            let current = textarea.lines().join("\n");
            if let Some(completed) = completer.complete(&current) {
                textarea.select_all();
                textarea.cut();
                textarea.insert_str(&completed);
            }
        }
        _ => {
            completer.reset();
            textarea.input(Event::Key(key));
        }
    }
}

/// Handle a UI event during inference (field-level borrows).
fn handle_inference_ui_inline(
    ui_event: UiEvent,
    terminal: &mut crate::tui_types::Term,
    menu: &mut MenuContent,
    renderer: &mut crate::tui_render::TuiRenderer,
) {
    match ui_event {
        UiEvent::Engine(EngineEvent::ApprovalRequest {
            id,
            tool_name,
            detail,
            preview,
        }) => {
            if preview.is_some() {
                renderer.preview_shown = true;
            }
            if let Some(ref prev) = preview {
                let diff_lines = crate::diff_render::render_lines(prev);
                for line in &diff_lines {
                    emit_above(terminal, line.clone());
                }
            }
            *menu = MenuContent::Approval {
                id,
                tool_name,
                detail,
            };
        }
        UiEvent::Engine(EngineEvent::LoopCapReached { cap, recent_tools }) => {
            emit_above(
                terminal,
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        format!("\u{26a0} Hard cap reached ({cap} iterations)"),
                        Style::default().fg(Color::Yellow),
                    ),
                ]),
            );
            for name in &recent_tools {
                emit_above(
                    terminal,
                    Line::from(vec![
                        Span::raw("    "),
                        Span::styled(
                            format!("\u{25cf} {name}"),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]),
                );
            }
            *menu = MenuContent::LoopCap;
        }
        UiEvent::Engine(event) => {
            renderer.render_to_terminal(event, terminal);
        }
    }
}
