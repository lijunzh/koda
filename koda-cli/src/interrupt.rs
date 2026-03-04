//! Ctrl+C interrupt handling for graceful cancellation.
//!
//! Provides a double-tap force quit mechanism:
//! first Ctrl+C sets a flag, second force-exits.

use std::sync::atomic::{AtomicBool, Ordering};

/// Shared flag that gets set to `true` when Ctrl+C is pressed.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Install the Ctrl+C handler. Call once at startup.
///
/// First Ctrl+C sets the flag (engine checks `CancellationToken`).
/// Second Ctrl+C force-exits the process.
pub fn install_handler() {
    let _ = ctrlc::set_handler(move || {
        if INTERRUPTED.load(Ordering::SeqCst) {
            // Second Ctrl+C: force exit
            eprintln!("\n\x1b[31mForce quit.\x1b[0m");
            std::process::exit(130);
        }
        INTERRUPTED.store(true, Ordering::SeqCst);
    });
}
