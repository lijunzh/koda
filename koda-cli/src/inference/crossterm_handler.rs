//! Crossterm event routing during inference.
//!
//! Two free functions called from
//! [`super::select_loop::TuiContext::run_inference_turn`]'s
//! `SelectArm::Crossterm` arm:
//!
//!   - [`handle_crossterm_event_inline`] — the small dispatcher
//!     (resize / mouse / paste / key).
//!   - [`handle_inference_key_inline`] — the large key handler
//!     (approval / loop-cap / ask-user / feedback / general keys).
//!
//! These were extracted to free functions back in the original file
//! because the parent select holds a `&mut self.session` borrow for the
//! whole turn future, which prevents any `&mut self`-taking method on
//! `TuiContext` from being called inside the select arm. Field-level
//! borrows let us pass exactly the slices of state each branch touches.
//!
//! `is_slash_command_attempt` (the #1211 guard) and `truncate_preview`
//! are private helpers used only inside the key handler, plus the
//! slash-guard regression tests.

use crate::input;
use crate::scroll_buffer::ScrollBuffer;
use crate::tui_types::{MenuContent, PromptMode};

use crossterm::event::{Event, KeyCode, KeyModifiers};
use koda_core::engine::{ApprovalDecision, EngineCommand};
use koda_core::trust::{self, TrustMode};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use tokio::sync::mpsc;

/// Route a crossterm event during inference (field-level borrows).
///
/// Extracted from the inline `tokio::select!` arm so the select body
/// stays small and readable.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_crossterm_event_inline(
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
