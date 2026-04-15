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
        let cancel_token = self.session.cancel.clone();
        let db_handle = self.session.db.clone();

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
                // Clamp scroll offset before drawing (resize may have
                // changed wrapping, making the old offset invalid).
                // Use the actual history panel height, not the full terminal
                // height — otherwise max_offset is too small and scrolling
                // up during inference gets clamped back toward the bottom,
                // re-engaging sticky_bottom.
                let (term_w, _) = crossterm::terminal::size()
                    .map(|(c, r)| (c as usize, r as usize))
                    .unwrap_or((80, 24));
                let hist_viewport = (self.history_area_height as usize).max(1);
                self.scroll_buffer.clamp_offset(term_w, hist_viewport);

                // Redraw viewport
                let mode = trust::read_trust(&self.shared_mode);
                let ctx = self.context_pct;
                let mcp_info = self.agent.mcp_status_bar_info();
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
                    );
                });

                // Biased: prioritise terminal input so mouse/keyboard events
                // are never starved by a flood of engine TextDelta events.
                // Without this, rapid streaming causes `ui_rx` to win the
                // select race repeatedly, letting mouse escape sequences
                // pile up in the terminal buffer until the crossterm parser
                // mis-frames them as individual key events (#540).
                tokio::select! {
                    biased;

                    Some(Ok(ev)) = self.crossterm_events.next() => {
                        handle_crossterm_event_inline(
                            ev,
                            &cancel_token,
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
                        ).await;
                    }
                    Some(ui_event) = ui_rx.recv() => {
                        // Extract context usage before rendering
                        if let UiEvent::Engine(EngineEvent::ContextUsage { used, max }) = &ui_event {
                            self.context_pct = if *max > 0 { (used * 100 / max) as u32 } else { 0 };
                        }
                        handle_inference_ui_inline(
                            ui_event,
                            &mut self.scroll_buffer,
                            &mut self.menu,
                            &mut self.prompt_mode,
                            &mut self.renderer,
                        );
                        // Batch-drain queued engine events to reduce redraws.
                        // Each loop iteration triggers a full terminal.draw(),
                        // so draining N events → 1 redraw instead of N redraws.
                        while let Ok(extra) = ui_rx.try_recv() {
                            if let UiEvent::Engine(EngineEvent::ContextUsage { used, max }) = &extra {
                                self.context_pct = if *max > 0 { (used * 100 / max) as u32 } else { 0 };
                            }
                            handle_inference_ui_inline(
                                extra,
                                &mut self.scroll_buffer,
                                &mut self.menu,
                                &mut self.prompt_mode,
                                &mut self.renderer,
                            );
                        }
                    }
                    result = &mut turn => {
                        if let Err(e) = result {
                            self.scroll_buffer.push(
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
                }
            }
        }

        // Post-turn cleanup
        self.post_turn_cleanup(ui_rx).await;
        Ok(())
    }

    // ── Post-turn cleanup ──────────────────────────────────────

    async fn post_turn_cleanup(&mut self, ui_rx: &mut mpsc::UnboundedReceiver<UiEvent>) {
        // If the turn was cancelled, clear the later_queue so deferred
        // messages don’t immediately fire a new turn that may block on a
        // single-slot local server (LM Studio, ollama) (#825).
        if self.session.cancel.is_cancelled() && !self.later_queue.is_empty() {
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
        self.session.cancel = tokio_util::sync::CancellationToken::new();

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
    cancel_token: &tokio_util::sync::CancellationToken,
    cmd_tx: &mpsc::Sender<EngineCommand>,
    scroll_buffer: &mut ScrollBuffer,
    hist_h: usize,
    menu: &mut MenuContent,
    prompt_mode: &mut PromptMode,
    pending_approval_id: &mut Option<String>,
    textarea: &mut ratatui_textarea::TextArea<'static>,
    shared_mode: &koda_core::trust::SharedTrustMode,
    completer: &mut crate::completer::InputCompleter,
    history: &mut Vec<String>,
    history_idx: &mut Option<usize>,
    later_queue: &mut std::collections::VecDeque<String>,
    paste_blocks: &mut Vec<input::PasteBlock>,
    db: &koda_core::db::Database,
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
                cancel_token,
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
    cancel_token: &tokio_util::sync::CancellationToken,
    cmd_tx: &mpsc::Sender<EngineCommand>,
    scroll_buffer: &mut ScrollBuffer,
    menu: &mut MenuContent,
    prompt_mode: &mut PromptMode,
    pending_approval_id: &mut Option<String>,
    textarea: &mut ratatui_textarea::TextArea<'static>,
    shared_mode: &koda_core::trust::SharedTrustMode,
    completer: &mut crate::completer::InputCompleter,
    history: &mut Vec<String>,
    history_idx: &mut Option<usize>,
    later_queue: &mut std::collections::VecDeque<String>,
    db: &koda_core::db::Database,
) {
    // Approval hotkeys
    if let MenuContent::Approval { id, .. } = menu {
        let approval_id = id.clone();
        let decision = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Some(ApprovalDecision::Approve),
            KeyCode::Char('n') | KeyCode::Char('N') => Some(ApprovalDecision::Reject),
            KeyCode::Char('a') | KeyCode::Char('A') => {
                trust::set_trust(shared_mode, TrustMode::Auto);
                Some(ApprovalDecision::Approve)
            }
            KeyCode::Char('f') | KeyCode::Char('F') => {
                *prompt_mode = PromptMode::WizardInput {
                    label: "Feedback".into(),
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

    // AskUser: freeform text input during inference.
    // id is embedded in MenuContent::AskUser — no separate pending field needed.
    if matches!(menu, MenuContent::AskUser { .. })
        && matches!(prompt_mode, PromptMode::WizardInput { .. })
    {
        match (key.code, key.modifiers) {
            (KeyCode::Enter, m) if m.contains(KeyModifiers::ALT) => {
                textarea.insert_newline();
            }
            (KeyCode::Enter, KeyModifiers::NONE) => {
                let answer = textarea.lines().join("\n");
                textarea.select_all();
                textarea.cut();
                *prompt_mode = PromptMode::Chat;
                if let MenuContent::AskUser { id, .. } = std::mem::replace(menu, MenuContent::None)
                {
                    let _ = cmd_tx
                        .send(EngineCommand::AskUserResponse { id, answer })
                        .await;
                }
            }
            (KeyCode::Esc, _) => {
                textarea.select_all();
                textarea.cut();
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
                textarea.input(Event::Key(key));
            }
        }
        return;
    }

    // Feedback text input during inference
    if matches!(prompt_mode, PromptMode::WizardInput { .. }) && pending_approval_id.is_some() {
        match (key.code, key.modifiers) {
            (KeyCode::Enter, m) if m.contains(KeyModifiers::ALT) => {
                textarea.insert_newline();
            }
            (KeyCode::Enter, KeyModifiers::NONE) => {
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
            (KeyCode::Esc, _) => {
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
        (KeyCode::Enter, m) if m.contains(KeyModifiers::ALT) => {
            textarea.insert_newline();
        }
        // Enter during inference — default lane is "next" (mid-turn steer).
        // The text is sent directly to the engine as QueueNext; it will be
        // injected before the next provider request in the current turn.
        (KeyCode::Enter, KeyModifiers::NONE) => {
            let text = textarea.lines().join("\n");
            if !text.trim().is_empty() {
                textarea.select_all();
                textarea.cut();
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
            let text = textarea.lines().join("\n");
            if !text.trim().is_empty() {
                textarea.select_all();
                textarea.cut();
                history.push(text.clone());
                let _ = db.history_push(&text).await;
                *history_idx = None;

                let preview = truncate_preview(&text, 80);
                let later_n = later_queue.len() + 1;
                scroll_buffer.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        format!("\u{1f4cb} Later ({later_n}): "),
                        Style::default().fg(Color::Yellow),
                    ),
                    Span::styled(preview, Style::default().fg(Color::DarkGray)),
                ]));

                later_queue.push_back(text);
            }
        }
        (KeyCode::Esc, _) => {
            cancel_token.cancel();
        }
        (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
            cancel_token.cancel();
        }
        // Ctrl+U: clear the later_queue (deferred messages) without cancelling inference.
        (KeyCode::Char('u'), m) if m.contains(KeyModifiers::CONTROL) => {
            if !later_queue.is_empty() {
                let n = later_queue.len();
                later_queue.clear();
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
            trust::cycle_trust(shared_mode);
        }
        // Up during inference: pop the last later_queue item back into the
        // editor so the user can edit it before re-submitting.
        // Falls back to normal textarea movement when the queue is empty.
        (KeyCode::Up, KeyModifiers::NONE) => {
            if let Some(popped) = later_queue.pop_back() {
                textarea.select_all();
                textarea.cut();
                textarea.insert_str(&popped);
                scroll_buffer.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        "\u{21a9} Popped from later queue",
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            } else {
                textarea.input(Event::Key(key));
            }
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
) {
    match ui_event {
        UiEvent::Engine(EngineEvent::AskUserRequest {
            id,
            question,
            options,
        }) => {
            *prompt_mode = PromptMode::WizardInput {
                label: "Answer".into(),
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
            renderer.render_to_buffer(event, buffer);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use koda_core::trust::SharedTrustMode;
    use std::collections::VecDeque;

    /// Build a `KeyEvent` from code + modifiers.
    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    /// Minimal harness: sets up the moving parts that `handle_inference_key_inline`
    /// touches, runs a closure with mutable refs, and returns the queue + history.
    async fn run_key(
        k: KeyEvent,
        initial_text: &str,
        initial_queue: &[&str],
    ) -> (VecDeque<String>, Vec<String>, Vec<EngineCommand>) {
        let cancel = tokio_util::sync::CancellationToken::new();
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<EngineCommand>(16);
        let mut scroll_buffer = ScrollBuffer::new(5000);
        let mut menu = crate::tui_types::MenuContent::None;
        let mut prompt_mode = PromptMode::Chat;
        let mut pending_approval_id = None;
        let mut textarea = ratatui_textarea::TextArea::default();
        let shared_mode = SharedTrustMode::default();
        let mut completer = crate::completer::InputCompleter::new(std::path::PathBuf::from("/tmp"));
        let mut history = Vec::new();
        let mut history_idx = None;
        let mut later_queue: VecDeque<String> =
            initial_queue.iter().map(|s| s.to_string()).collect();

        // Seed the textarea with the initial text.
        if !initial_text.is_empty() {
            textarea.insert_str(initial_text);
        }

        // Open a temp database.
        let tmp = tempfile::TempDir::new().unwrap();
        let db = koda_core::db::Database::init(tmp.path()).await.unwrap();

        handle_inference_key_inline(
            k,
            &cancel,
            &cmd_tx,
            &mut scroll_buffer,
            &mut menu,
            &mut prompt_mode,
            &mut pending_approval_id,
            &mut textarea,
            &shared_mode,
            &mut completer,
            &mut history,
            &mut history_idx,
            &mut later_queue,
            &db,
        )
        .await;

        // Drain engine commands.
        drop(cmd_tx);
        let mut cmds = Vec::new();
        while let Some(c) = cmd_rx.recv().await {
            cmds.push(c);
        }

        (later_queue, history, cmds)
    }

    // ── Ctrl+J: push to later queue ───────────────────────────────────

    #[tokio::test]
    async fn ctrl_j_pushes_to_later_queue() {
        let (queue, history, _) = run_key(
            key(KeyCode::Char('j'), KeyModifiers::CONTROL),
            "deferred message",
            &[],
        )
        .await;
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0], "deferred message");
        assert_eq!(history, vec!["deferred message"]);
    }

    #[tokio::test]
    async fn ctrl_j_fifo_ordering() {
        // Simulate two Ctrl+J presses by pre-seeding the queue
        // and adding a third item.
        let (queue, _, _) = run_key(
            key(KeyCode::Char('j'), KeyModifiers::CONTROL),
            "third",
            &["first", "second"],
        )
        .await;
        assert_eq!(queue.len(), 3);
        assert_eq!(queue[0], "first");
        assert_eq!(queue[1], "second");
        assert_eq!(queue[2], "third");
    }

    #[tokio::test]
    async fn ctrl_j_ignores_empty_text() {
        let (queue, history, _) = run_key(
            key(KeyCode::Char('j'), KeyModifiers::CONTROL),
            "   ", // whitespace-only
            &[],
        )
        .await;
        assert!(queue.is_empty(), "empty text must not enqueue");
        assert!(history.is_empty(), "empty text must not push history");
    }

    // ── Up: pop from later queue ──────────────────────────────────────

    #[tokio::test]
    async fn up_pops_last_from_later_queue() {
        let (queue, _, _) = run_key(
            key(KeyCode::Up, KeyModifiers::NONE),
            "",
            &["first", "second"],
        )
        .await;
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0], "first");
    }

    #[tokio::test]
    async fn up_on_empty_queue_is_noop() {
        let (queue, _, _) = run_key(key(KeyCode::Up, KeyModifiers::NONE), "", &[]).await;
        assert!(queue.is_empty());
    }

    // ── Ctrl+U: clear later queue ────────────────────────────────────

    #[tokio::test]
    async fn ctrl_u_clears_later_queue() {
        let (queue, _, _) = run_key(
            key(KeyCode::Char('u'), KeyModifiers::CONTROL),
            "",
            &["a", "b", "c"],
        )
        .await;
        assert!(queue.is_empty(), "Ctrl+U must clear the queue");
    }

    #[tokio::test]
    async fn ctrl_u_on_empty_queue_is_noop() {
        let (queue, _, _) = run_key(key(KeyCode::Char('u'), KeyModifiers::CONTROL), "", &[]).await;
        assert!(queue.is_empty());
    }

    // ── Enter: "next" lane (QueueNext engine command) ─────────────────

    #[tokio::test]
    async fn enter_sends_queue_next_command() {
        let (queue, history, cmds) = run_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            "steer message",
            &[],
        )
        .await;
        // Later queue must be untouched.
        assert!(queue.is_empty());
        // History must record the message.
        assert_eq!(history, vec!["steer message"]);
        // Must emit QueueNext.
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            EngineCommand::QueueNext { text } => assert_eq!(text, "steer message"),
            other => panic!("expected QueueNext, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn enter_ignores_empty_text() {
        let (_, history, cmds) = run_key(key(KeyCode::Enter, KeyModifiers::NONE), "", &[]).await;
        assert!(history.is_empty());
        assert!(cmds.is_empty());
    }

    // ── Esc: cancels inference ────────────────────────────────────────

    #[tokio::test]
    async fn esc_cancels_token() {
        let cancel = tokio_util::sync::CancellationToken::new();
        let (cmd_tx, _cmd_rx) = mpsc::channel::<EngineCommand>(1);
        let mut scroll_buffer = ScrollBuffer::new(100);
        let mut menu = crate::tui_types::MenuContent::None;
        let mut prompt_mode = PromptMode::Chat;
        let mut pending = None;
        let mut textarea = ratatui_textarea::TextArea::default();
        let shared = SharedTrustMode::default();
        let mut comp = crate::completer::InputCompleter::new(std::path::PathBuf::from("/tmp"));
        let mut hist = Vec::new();
        let mut idx = None;
        let mut queue = VecDeque::new();
        let tmp = tempfile::TempDir::new().unwrap();
        let db = koda_core::db::Database::init(tmp.path()).await.unwrap();

        handle_inference_key_inline(
            key(KeyCode::Esc, KeyModifiers::NONE),
            &cancel,
            &cmd_tx,
            &mut scroll_buffer,
            &mut menu,
            &mut prompt_mode,
            &mut pending,
            &mut textarea,
            &shared,
            &mut comp,
            &mut hist,
            &mut idx,
            &mut queue,
            &db,
        )
        .await;

        assert!(cancel.is_cancelled(), "Esc must cancel the token");
    }

    // ── truncate_preview ─────────────────────────────────────────────

    #[test]
    fn truncate_preview_short() {
        assert_eq!(truncate_preview("hello", 80), "hello");
    }

    #[test]
    fn truncate_preview_replaces_newlines() {
        assert_eq!(truncate_preview("a\nb", 80), "a↵b");
    }

    #[test]
    fn truncate_preview_truncates_long() {
        let long = "x".repeat(100);
        let result = truncate_preview(&long, 10);
        assert_eq!(result.chars().count(), 10);
        assert!(result.ends_with('\u{2026}'));
    }
}
