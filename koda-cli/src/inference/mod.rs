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
//!
//! Multiple `impl TuiContext` blocks (one in `select_loop.rs`, one in
//! `post_turn.rs`) are stitched together by rustc — same type, same
//! crate, just different files. The free helper functions live as
//! `pub(super)` so `select_loop` can dispatch into the handler files
//! without exposing them to the rest of the crate.

mod crossterm_handler;
mod post_turn;
mod select_loop;
mod ui_handler;
