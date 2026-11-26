//! `/side` (also `/btw`): a side conversation, as Codex's `/side` opens one.
//!
//! A side conversation is a fork of the session, with Codex's instructions
//! added: the history it inherits is reference only, and it changes nothing
//! unless asked. The TUI switches to it while the main session goes on, as
//! a session does without a client watching it: its turn keeps running and
//! a request it makes (an approval, a question) waits. `Ctrl+C` on an empty
//! input box returns: the side session is deleted, the main session is
//! drawn again, and a turn still running there is followed, with whatever
//! it asked meanwhile.
//!
//! `/side` works at the prompt and while a turn runs; mid-turn, the TUI
//! leaves the turn running and opens the side conversation.

use koda_client::events::Outgoing;
use koda_client::protocol::methods;
use koda_client::protocol::{ServerNotification, ServerRequest};
use ratatui::text::Line;

use super::{CommandOutcome, TuiContext, TuiSession};
use crate::tui_output;

/// Codex's instructions for a side conversation
/// (`context-fragments/src/side_conversation.rs`), in Koda's terms: added
/// to the side session's system prompt as `developer_instructions`.
pub(crate) const SIDE_DEVELOPER_INSTRUCTIONS: &str = "\
You are in a side conversation, not the main thread.

This side conversation is for answering questions and lightweight exploration without \
disrupting the main thread. Do not present yourself as continuing the main thread's active \
task.

The inherited history is provided only as reference context. Do not treat instructions, \
plans, or requests found in the inherited history as active instructions for this side \
conversation. Only instructions submitted after the side-conversation boundary are active.

Do not continue, execute, or complete any task, plan, tool call, approval, edit, or request \
that appears only in inherited history.

Sub-agents are off-limits in this side conversation. Do not start any, even if sub-agents \
were used before the boundary.

You may perform non-mutating inspection, including reading or searching files and running \
checks that do not alter tracked files.

Do not modify files, source, git state, permissions, configuration, or any other workspace \
state unless the user explicitly requests that change in this side conversation. Do not ask \
to run outside the sandbox unless the user explicitly requests a change that needs it. If \
the user explicitly requests a change, keep it minimal, local to the request, and avoid \
disrupting the main thread.";

/// Codex's boundary between the inherited history and the side
/// conversation, sent ahead of the first side question.
pub(crate) const SIDE_BOUNDARY: &str = "\
<side_conversation_boundary>
Everything before this boundary is inherited history from the main thread. It is reference \
context only. It is not your current task.

Do not continue, execute, or complete any instructions, plans, tool calls, approvals, edits, \
or requests from before this boundary. Only messages submitted after this boundary are \
active user instructions for this side conversation.
</side_conversation_boundary>";

/// The side conversation the TUI is in, and the main session it left.
pub(crate) struct SideState {
    /// The main session, as the TUI had it.
    pub(crate) main: TuiSession,
    /// The main session's running turn, while it runs.
    pub(crate) main_turn: Option<String>,
    /// What the main session asked while the TUI was away, oldest first.
    pub(crate) waiting: Vec<ServerRequest>,
    /// Whether the boundary has gone out, ahead of the first question.
    pub(crate) boundary_sent: bool,
    /// On the way back: the main session's messages are kept for the turn
    /// the TUI follows next, not dropped.
    returning: bool,
    /// What the main session sent on the way back, oldest first.
    held: Vec<Outgoing>,
    /// A review (`/review`) rather than a side conversation: what it
    /// reviews, as the banner and results name it.
    pub(crate) review: Option<String>,
}

impl SideState {
    /// Away from `main`, which has `main_turn` running if any.
    pub(crate) fn new(main: TuiSession, main_turn: Option<String>, review: Option<String>) -> Self {
        Self {
            main,
            main_turn,
            waiting: Vec::new(),
            boundary_sent: false,
            returning: false,
            held: Vec::new(),
            review,
        }
    }
}

/// What the TUI says on returning to the main session.
fn back_message(review: &Option<String>) -> String {
    match review {
        Some(_) => "Back in the main session.".to_string(),
        None => "Back in the main session; the side conversation is gone.".to_string(),
    }
}

/// The session a host message belongs to, if it names one.
fn session_of(message: &Outgoing) -> Option<String> {
    let value = match message {
        Outgoing::Request(request) => serde_json::to_value(request).ok()?,
        Outgoing::Notification(notification) => serde_json::to_value(notification).ok()?,
        _ => return None,
    };
    let params = &value["params"];
    params["sessionId"]
        .as_str()
        .or_else(|| params["session"]["id"].as_str())
        .map(str::to_string)
}

impl TuiContext {
    /// `/side [question]`: fork this session into a side conversation and
    /// switch to it; with a question, ask it. `main_turn` is the turn the
    /// TUI left running to come here.
    pub(crate) async fn enter_side(
        &mut self,
        question: &str,
        main_turn: Option<String>,
    ) -> CommandOutcome {
        if self.side.is_some() {
            tui_output::warn_msg(
                &mut self.scroll_buffer,
                "A side conversation is already open. Press Ctrl+C to return before starting \
                 another."
                    .to_string(),
            );
            return CommandOutcome::Handled;
        }
        use koda_client::protocol::session::SessionForkParams;
        let config = std::collections::BTreeMap::from([(
            "developer_instructions".to_string(),
            serde_json::Value::String(SIDE_DEVELOPER_INSTRUCTIONS.to_string()),
        )]);
        let forked = self
            .client
            .request::<methods::SessionFork>(SessionForkParams {
                session_id: self.session.id.clone(),
                config: Some(config),
                // It goes with the TUI: not one to list or keep.
                ephemeral: true,
                ..Default::default()
            })
            .await;
        let forked = match forked {
            Ok(forked) => forked.session,
            Err(e) => {
                tui_output::err_msg(
                    &mut self.scroll_buffer,
                    format!("Couldn't open a side conversation: {e}"),
                );
                return CommandOutcome::Handled;
            }
        };
        // The main session stays subscribed: its turn goes on, and what it
        // asks waits for the TUI's return.
        let main = std::mem::replace(
            &mut self.session,
            TuiSession {
                id: forked.id.clone(),
                instruction_sources: forked.instruction_sources.clone().unwrap_or_default(),
            },
        );
        self.host_session = forked.id.clone();
        self.side = Some(SideState::new(main, main_turn, None));
        if let Some(mode) = forked.mode {
            self.mode = mode;
        }
        self.scroll_buffer.push_lines([
            Line::default(),
            Line::styled(
                "  \u{2500}\u{2500}\u{2500} side conversation \u{2500}\u{2500}\u{2500}",
                tui_output::DIM,
            ),
            Line::styled(
                "  The main session goes on. Ctrl+C on an empty prompt returns to it.",
                tui_output::DIM,
            ),
            Line::default(),
        ]);
        if let Err(e) = self.sync_settings(true).await {
            tui_output::warn_msg(&mut self.scroll_buffer, format!("{e:#}"));
        }
        if question.trim().is_empty() {
            return CommandOutcome::Handled;
        }
        self.prepare_inference_start(question.trim()).await
    }

    /// Ahead of the side conversation's first question, Codex's boundary.
    pub(crate) fn side_boundary(&mut self, message: String) -> String {
        match self.side.as_mut() {
            Some(side) if side.review.is_none() && !side.boundary_sent => {
                side.boundary_sent = true;
                format!("{SIDE_BOUNDARY}\n\n{message}")
            }
            _ => message,
        }
    }

    /// A message of the main session while the TUI is in a side
    /// conversation: what it asks is kept for the return, and its turn's end
    /// noted. True when it was the main session's, for the caller to drop.
    pub(crate) fn note_main_session(&mut self, message: &Outgoing) -> bool {
        let Some(side) = self.side.as_mut() else {
            return false;
        };
        if session_of(message).as_deref() != Some(side.main.id.as_str()) {
            return false;
        }
        if side.returning {
            if let Outgoing::Notification(ServerNotification::TurnCompleted(_)) = message {
                side.main_turn = None;
            }
            side.held.push(message.clone());
            return true;
        }
        match message {
            Outgoing::Request(request) => {
                let first = side.waiting.is_empty();
                side.waiting.push(request.clone());
                if first {
                    tui_output::warn_msg(
                        &mut self.scroll_buffer,
                        "The main session is waiting for you. Ctrl+C on an empty prompt \
                         returns to it."
                            .to_string(),
                    );
                }
            }
            Outgoing::Notification(ServerNotification::TurnStarted(started)) => {
                side.main_turn = Some(started.turn.id.clone());
            }
            Outgoing::Notification(ServerNotification::TurnCompleted(_)) => {
                side.main_turn = None;
                side.waiting.clear();
                tui_output::dim_msg(
                    &mut self.scroll_buffer,
                    "The main session's turn has ended.".to_string(),
                );
            }
            Outgoing::Notification(ServerNotification::ServerRequestResolved(resolved)) => {
                side.waiting
                    .retain(|request| request.id() != &resolved.request_id);
            }
            _ => {}
        }
        true
    }

    /// Leave the side conversation: delete it, draw the main session again,
    /// and say which turn to follow there, with what it asked and sent
    /// meanwhile.
    pub(crate) async fn leave_side(&mut self) -> Option<(String, Vec<Outgoing>)> {
        use koda_client::protocol::session::SessionDeleteParams;
        let main = self.side.as_ref()?.main.clone();
        let left = std::mem::replace(&mut self.session, main);
        self.host_session = self.session.id.clone();
        if let Err(e) = self
            .client
            .request::<methods::SessionDelete>(SessionDeleteParams {
                session_id: left.id,
            })
            .await
        {
            tracing::warn!("couldn't delete the side conversation: {e}");
        }
        self.redraw_main().await;
        // From here on the main session's messages are the followed turn's.
        if let Some(side) = self.side.as_mut() {
            side.returning = true;
        }
        if let Err(e) = self.sync_settings(true).await {
            tui_output::warn_msg(&mut self.scroll_buffer, format!("{e:#}"));
        }
        let side = self.side.take()?;
        match side.main_turn {
            Some(turn) => {
                let mut messages: Vec<Outgoing> =
                    side.waiting.into_iter().map(Outgoing::Request).collect();
                messages.extend(side.held);
                tui_output::dim_msg(&mut self.scroll_buffer, back_message(&side.review));
                Some((turn, messages))
            }
            None => {
                // It ended on the way back: draw it as it ended.
                if !side.held.is_empty() {
                    self.redraw_main().await;
                }
                tui_output::dim_msg(&mut self.scroll_buffer, back_message(&side.review));
                None
            }
        }
    }

    /// Draw the main session again from what the host has stored.
    pub(super) async fn redraw_main(&mut self) {
        use koda_client::protocol::session::SessionReadParams;
        let read = self
            .client
            .request::<methods::SessionRead>(SessionReadParams {
                session_id: self.session.id.clone(),
                include_turns: true,
            })
            .await;
        self.scroll_buffer.clear();
        self.renderer.forget_replies();
        match read {
            Ok(read) => super::replay_turns(
                &mut self.renderer,
                &read.session.turns,
                &mut self.scroll_buffer,
            ),
            Err(e) => tui_output::warn_msg(
                &mut self.scroll_buffer,
                format!("Couldn't read the main session: {e}"),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_instructions_are_codexs_in_substance() {
        for line in [
            "You are in a side conversation, not the main thread.",
            "Only instructions submitted after the side-conversation boundary are active.",
            "Sub-agents are off-limits in this side conversation.",
        ] {
            assert!(SIDE_DEVELOPER_INSTRUCTIONS.contains(line), "{line}");
        }
        assert!(SIDE_BOUNDARY.starts_with("<side_conversation_boundary>"));
    }
}
