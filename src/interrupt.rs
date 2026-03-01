//! Ctrl+C interrupt handling for graceful cancellation.
//!
//! Provides a shared interrupt flag that's set on SIGINT and checked
//! during streaming, tool execution, and confirmation prompts.

use std::sync::atomic::{AtomicBool, Ordering};

/// Shared flag that gets set to `true` when Ctrl+C is pressed.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Install the Ctrl+C handler. Call once at startup.
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

/// Check if an interrupt has been requested.
pub fn is_interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// Clear the interrupt flag (call after handling the interrupt).
pub fn clear() {
    INTERRUPTED.store(false, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_interrupt_flag() {
        clear();
        assert!(!is_interrupted());

        INTERRUPTED.store(true, Ordering::SeqCst);
        assert!(is_interrupted());

        clear();
        assert!(!is_interrupted());
    }
}
