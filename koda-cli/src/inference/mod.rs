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
//! - [`post_turn`] — `post_turn_cleanup` + `maybe_auto_compact`. Runs
//!   once per turn after the select loop breaks. Lives in its own file
//!   so the auto-compact policy is easy to find and pin.
//! - [`crossterm_handler`] — terminal input routing during inference
//!   (resize, mouse, paste, key dispatch) plus the giant
//!   `handle_inference_key_inline` (approval / loop-cap / ask-user /
//!   feedback / general keys) and the #1211 slash-guard.
//! - [`ui_handler`] — engine→UI rendering during inference (approval
//!   prompts, ask-user, loop cap, fall-through to `TuiRenderer`).
//! - [`turn_tracker`] — links the TUI's answers to the host's requests and
//!   keeps steering text until the model receives it (PR 5c: the host runs
//!   the turn, the TUI is its client).
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
}
