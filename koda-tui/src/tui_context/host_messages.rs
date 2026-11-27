//! The host's messages in the order it sent them, none thrown away.
//!
//! The TUI reads one stream of host messages, as Codex's TUI reads one
//! stream of `EventMsg`s. A wait for one message, such as the settings
//! report after `session/settings/update`, sets aside every other one in
//! [`TuiContext::held`], oldest first, and every reader takes those before
//! the client's next. A turn Koda starts on the session's goal while a wait
//! runs is followed afterwards, with its approvals, rather than lost.

use std::collections::VecDeque;

use koda_client::InProcessClient;
use koda_client::events::Outgoing;
use koda_client::protocol::ServerNotification;

use super::TuiContext;

/// The host's next message: one a wait set aside first, then the client's.
/// `None` once the host has stopped. Two fields rather than `&mut
/// TuiContext`, so a `select!` can wait on the keyboard beside it.
pub(crate) async fn next_host_event(
    client: &mut InProcessClient,
    held: &mut VecDeque<Outgoing>,
) -> Option<Outgoing> {
    match held.pop_front() {
        Some(message) => Some(message),
        None => client.next_event().await,
    }
}

/// The session `session_id`'s `turn/started`: the turn's id.
pub(crate) fn turn_started<'a>(message: &'a Outgoing, session_id: &str) -> Option<&'a str> {
    match message {
        Outgoing::Notification(ServerNotification::TurnStarted(started))
            if started.session_id == session_id =>
        {
            Some(&started.turn.id)
        }
        _ => None,
    }
}

impl TuiContext {
    /// The host's next message if one is ready, a set-aside one first.
    pub(crate) fn try_next_host_event(&mut self) -> Option<Outgoing> {
        self.held
            .pop_front()
            .or_else(|| self.client.try_next_event())
    }

    /// Put `messages` back ahead of everything else, in their order: the
    /// next reader takes them first.
    pub(crate) fn hold_first(&mut self, messages: Vec<Outgoing>) {
        for message in messages.into_iter().rev() {
            self.held.push_front(message);
        }
    }

    /// One of the host's messages while the TUI follows no turn of the
    /// session: a main session's, while in a side conversation, is kept
    /// for the return; settings are mirrored; the goal, MCP servers, a
    /// background agent's request, an update of the server and a warning
    /// (the session's, one about no session, the config's) are shown. A
    /// turn starting is the caller's to follow, before this.
    pub(crate) fn absorb_idle_message(&mut self, message: Outgoing) {
        if self.note_main_session(&message) {
            return;
        }
        if self.mirror_settings(&message) {
            return;
        }
        self.note_session(&message);
        if self.mcp.observe(&self.session.id, &message) {
            self.mcp_stale = true;
        }
        // A background agent may ask while no turn runs.
        if self.take_request(&message) {
            return;
        }
        if self.note_update(&message) {
            return;
        }
        // The session's, such as an assumed context window after `/model`,
        // or one about no session, such as a Claude model this Koda doesn't
        // support yet, which the host says once the Models API has answered.
        match message {
            Outgoing::Notification(ServerNotification::Warning(w))
                if w.session_id.as_deref().is_none_or(|s| s == self.session.id) =>
            {
                crate::tui_output::warn_msg(&mut self.scroll_buffer, w.message);
            }
            Outgoing::Notification(ServerNotification::ConfigWarning(w)) => {
                crate::tui_output::warn_msg(&mut self.scroll_buffer, w.summary);
            }
            _ => {}
        }
    }
}
