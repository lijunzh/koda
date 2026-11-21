//! Inference turn lifecycle — inner event loop + post-turn cleanup.
//!
//! Originally a single 1.2k-line `tui_handlers_inference.rs` (extracted
//! from `TuiContext::run_event_loop` in #447). Split into cohesive
//! sub-modules under #1144 once it crossed the 600-line guideline:
//!
//! - [`select_loop`] — the inner `tokio::select!` loop, `SelectArm`,
//!   bounded drain, the `run_inference_turn` entry point. Hottest and
//!   most subtle piece (rotating biased select + frame coalescing +
//!   per-turn cancel cascade).
//! - [`post_turn`] — `post_turn_cleanup`. Runs once per turn after the
//!   select loop breaks.
//! - [`crossterm_handler`] — terminal input routing during inference
//!   (resize, mouse, paste, key dispatch) plus the giant
//!   `handle_inference_key_inline` (approval / loop-cap / ask-user /
//!   feedback / general keys) and the #1211 slash-guard.
//! - [`ui_handler`] — the host's requests during inference become the
//!   dialogs (approval, ask-user, loop cap).
//! - [`turn_tracker`] — keeps steering text until the model receives it,
//!   and the turn's end (PR 5c: the host runs the turn, the TUI is its
//!   client).
//!
//! Multiple `impl TuiContext` blocks (one in `select_loop.rs`, one in
//! `post_turn.rs`) are stitched together by rustc — same type, same
//! crate, just different files. The free helper functions live as
//! `pub(super)` so `select_loop` can dispatch into the handler files
//! without exposing them to the rest of the crate.

mod crossterm_handler;
mod post_turn;
mod select_loop;
mod turn_tracker;
mod ui_handler;

use koda_client::protocol::rpc::RequestId;

/// What the key handlers send the running turn.
#[derive(Debug)]
pub(crate) enum TurnCommand {
    /// The answer to one of the host's requests: a dialog's.
    Answer {
        request: RequestId,
        response: serde_json::Value,
    },
    /// Text for the model, sent with `turn/steer`.
    Steer { text: String },
    /// Text for after the turn (`Tab`), sent as the next prompt once the
    /// turn completes. Its pastes and images are taken from the input box's.
    Queue { text: String },
    /// Take the message queued last back into the input box (`Alt+Up`).
    Unqueue,
    /// `/side` typed mid-turn: leave the turn running and open a side
    /// conversation, with this question.
    Side { question: String },
}

impl TurnCommand {
    /// The answer `response` to `request`.
    pub(crate) fn answer(request: RequestId, response: impl serde::Serialize) -> Self {
        Self::Answer {
            request,
            response: serde_json::to_value(response).unwrap_or_default(),
        }
    }
}

/// Stops the running turn: sends `turn/interrupt` without waiting, since
/// the host answers once the turn has stopped and the TUI keeps drawing
/// meanwhile. Pressing it again sends another, which the host refuses
/// harmlessly once the turn has ended.
pub(crate) struct TurnStop {
    requests: koda_client::RequestHandle,
    session_id: String,
    turn_id: String,
}

impl TurnStop {
    pub(crate) fn interrupt(&self) {
        let requests = self.requests.clone();
        let params = koda_client::protocol::turn::TurnInterruptParams {
            session_id: self.session_id.clone(),
            turn_id: self.turn_id.clone(),
        };
        tokio::spawn(async move {
            use koda_client::protocol::methods::TurnInterrupt;
            if let Err(e) = requests.request::<TurnInterrupt>(params).await {
                tracing::debug!("turn/interrupt: {e}");
            }
        });
    }

    /// `/stop` during the turn: stop the session's background processes,
    /// which the turn's own interruption leaves running.
    pub(crate) async fn stop_background(
        &self,
        buffer: &mut crate::scroll_buffer::ScrollBuffer,
        tracker: &mut crate::child_activity::ChildActivityTracker,
    ) {
        crate::tui_context::background::stop_background(
            &self.requests,
            &self.session_id,
            buffer,
            tracker,
        )
        .await;
    }

    /// `/goal` during the turn: an interrupt stops only the turn, so this is
    /// how the goal is paused or cleared while Koda works on it.
    pub(crate) async fn goal(&self, arg: &str, buffer: &mut crate::scroll_buffer::ScrollBuffer) {
        crate::tui_context::goal::command(&self.requests, &self.session_id, arg, buffer).await;
    }

    /// Send the mode Shift+Tab chose during the turn; the host applies it
    /// once the turn has ended. Not waited for.
    pub(crate) fn set_mode(&self, mode: koda_client::protocol::state::TrustMode) {
        let requests = self.requests.clone();
        let params = koda_client::protocol::session::SessionSettingsUpdateParams {
            session_id: self.session_id.clone(),
            mode: Some(mode),
            ..Default::default()
        };
        tokio::spawn(async move {
            use koda_client::protocol::methods::SessionSettingsUpdate;
            if let Err(e) = requests.request::<SessionSettingsUpdate>(params).await {
                tracing::warn!("session/settings/update: {e}");
            }
        });
    }
}
