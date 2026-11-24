//! The host's requests to the person, one on screen at a time.
//!
//! Sub-agents ask in the main session, as in Claude Code and Codex, so several
//! requests can be open at once: the main agent's, a blocking sub-agent's, a
//! background agent's. The one on screen is answered first; the rest wait in
//! [`TuiContext::queued_requests`]. A background agent may ask while no turn
//! runs, so the idle loop shows and answers requests too.

use koda_client::events::Outgoing;
use koda_client::protocol::ServerNotification;
use koda_client::protocol::item::{ApprovalDecision, ToolCallApprovalResponse};

use super::TuiContext;
use crate::tui_types::{MenuContent, PromptMode};

impl TuiContext {
    /// Take `message` if it is a request or a request's withdrawal: show it
    /// now when nothing else is asking, or queue it. Returns whether it was
    /// taken.
    pub(crate) fn take_request(&mut self, message: &Outgoing) -> bool {
        match message {
            Outgoing::Request(request) => {
                self.queued_requests.push_back(request.clone());
                self.show_next_request();
                true
            }
            // The host withdrew it (its sub-agent stopped, or the turn it
            // belonged to ended): drop it, from the screen too.
            Outgoing::Notification(ServerNotification::ServerRequestResolved(resolved)) => {
                self.queued_requests
                    .retain(|request| request.id() != &resolved.request_id);
                if self.shown_request() == Some(&resolved.request_id) {
                    self.menu = MenuContent::None;
                    if matches!(self.prompt_mode, PromptMode::WizardInput { .. }) {
                        self.prompt_mode = PromptMode::Chat;
                        self.textarea.set_text_clearing_elements("");
                    }
                    self.pending_approval_id = None;
                }
                false
            }
            Outgoing::Notification(_) => false,
        }
    }

    /// Show the next queued request once nothing is asking.
    pub(crate) fn show_next_request(&mut self) {
        if self.shown_request().is_some() || self.pending_approval_id.is_some() {
            return;
        }
        if !self.menu.is_none() && !matches!(self.menu, MenuContent::WizardTrail(_)) {
            return;
        }
        let Some(request) = self.queued_requests.pop_front() else {
            return;
        };
        crate::inference::ui_handler::show_request(
            &request,
            &mut self.scroll_buffer,
            &mut self.menu,
            &mut self.prompt_mode,
            &mut self.renderer,
        );
    }

    /// The request on screen, if one is.
    fn shown_request(&self) -> Option<&koda_client::protocol::rpc::RequestId> {
        match &self.menu {
            MenuContent::Approval { id, .. }
            | MenuContent::AskUser { id, .. }
            | MenuContent::LoopCap { id } => Some(id),
            _ => None,
        }
    }

    /// An approval key while no turn runs: a background agent asked. `y`,
    /// `s` (when offered), `n`, and Esc, which refuses that one call. Returns
    /// whether the key was for it.
    pub(crate) async fn answer_idle_approval(&mut self, key: crossterm::event::KeyEvent) -> bool {
        use crossterm::event::KeyCode;
        let MenuContent::Approval {
            id,
            allow_for_session,
            ..
        } = &self.menu
        else {
            return false;
        };
        // Ctrl+C keeps its meaning.
        if key.code == KeyCode::Char('c')
            && key
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL)
        {
            return false;
        }
        let decision = match key.code {
            KeyCode::Char('y' | 'Y') => ApprovalDecision::Approve,
            KeyCode::Char('s' | 'S') if *allow_for_session => ApprovalDecision::ApproveForSession,
            KeyCode::Char('n' | 'N') | KeyCode::Esc => ApprovalDecision::Reject,
            // Anything else is held, so a stray key can't type into the
            // prompt behind the dialog.
            _ => return true,
        };
        let id = id.clone();
        self.menu = MenuContent::None;
        if let Err(e) = self
            .client
            .resolve_server_request(
                id,
                serde_json::to_value(ToolCallApprovalResponse { decision }).unwrap_or_default(),
            )
            .await
        {
            tracing::warn!("an answer didn't reach Koda's host: {e}");
        }
        self.show_next_request();
        true
    }
}
