//! Ctrl+C interrupt handling for graceful cancellation.
//!
//! Provides a double-tap force quit mechanism:
//! first Ctrl+C cancels the active turn,
//! second force-exits.

use std::sync::atomic::{AtomicBool, Ordering};

/// Shared flag that gets set to `true` when the first Ctrl+C arrives.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Handle a SIGINT. Returns `true` on double-tap (caller should force-exit).
pub fn handle_sigint() -> bool {
    if INTERRUPTED.load(Ordering::SeqCst) {
        true // second Ctrl+C — force quit
    } else {
        INTERRUPTED.store(true, Ordering::SeqCst);
        false // first Ctrl+C — cancel gracefully
    }
}

/// Reset the interrupted flag. Call after each turn.
pub fn reset() {
    INTERRUPTED.store(false, Ordering::SeqCst);
}
