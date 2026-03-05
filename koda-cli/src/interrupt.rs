//! Ctrl+C interrupt handling for graceful cancellation.
//!
//! Provides a double-tap force quit mechanism:
//! first Ctrl+C sets a flag and cancels the active turn,
//! second force-exits.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio_util::sync::CancellationToken;

/// Shared flag that gets set to `true` when Ctrl+C is pressed.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Slot holding the current turn's cancellation token.
static CANCEL_SLOT: OnceLock<Arc<Mutex<Option<CancellationToken>>>> = OnceLock::new();

fn slot() -> &'static Arc<Mutex<Option<CancellationToken>>> {
    CANCEL_SLOT.get_or_init(|| Arc::new(Mutex::new(None)))
}

/// Install the Ctrl+C handler. Call once at startup.
///
/// First Ctrl+C sets the flag and cancels the active inference turn.
/// Second Ctrl+C force-exits the process.
pub fn install_handler() {
    let slot = slot().clone();
    let _ = ctrlc::set_handler(move || {
        if INTERRUPTED.load(Ordering::SeqCst) {
            // Second Ctrl+C: force exit
            eprintln!("\n\x1b[31mForce quit.\x1b[0m");
            std::process::exit(130);
        }
        INTERRUPTED.store(true, Ordering::SeqCst);
        if let Ok(guard) = slot.lock()
            && let Some(ref token) = *guard
        {
            token.cancel();
        }
    });
}

/// Set the current turn's cancel token. Call before each `run_turn()`.
pub fn set_cancel_token(token: CancellationToken) {
    if let Ok(mut guard) = slot().lock() {
        *guard = Some(token);
    }
}

/// Reset the interrupted flag and clear the cancel token. Call after each turn.
pub fn reset() {
    INTERRUPTED.store(false, Ordering::SeqCst);
    if let Ok(mut guard) = slot().lock() {
        *guard = None;
    }
}
