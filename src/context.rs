//! Context window tracking.
//!
//! Tracks the current context usage (tokens used / max tokens) so the
//! prompt and footer can display it. Updated after each inference turn.

use std::sync::atomic::{AtomicUsize, Ordering};

static CONTEXT_USED: AtomicUsize = AtomicUsize::new(0);
static CONTEXT_MAX: AtomicUsize = AtomicUsize::new(0);

/// Update the context usage after assembling messages.
pub fn update(used: usize, max: usize) {
    CONTEXT_USED.store(used, Ordering::Relaxed);
    CONTEXT_MAX.store(max, Ordering::Relaxed);
}

/// Get current context usage as (used, max).
pub fn get() -> (usize, usize) {
    (
        CONTEXT_USED.load(Ordering::Relaxed),
        CONTEXT_MAX.load(Ordering::Relaxed),
    )
}

/// Get context usage as a percentage (0-100).
pub fn percentage() -> usize {
    let (used, max) = get();
    if max == 0 {
        return 0;
    }
    (used * 100) / max
}

/// Format context usage for the footer: "4.1k/128k (3%)"
pub fn format_footer() -> String {
    let (used, max) = get();
    if max == 0 {
        return String::new();
    }
    let pct = (used * 100) / max;
    format!("context: {}/{} ({}%)", format_k(used), format_k(max), pct)
}

/// Format a number as "1.2k" or "128k".
fn format_k(n: usize) -> String {
    if n < 1_000 {
        format!("{n}")
    } else if n < 10_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{}k", n / 1_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_k() {
        assert_eq!(format_k(500), "500");
        assert_eq!(format_k(4_100), "4.1k");
        assert_eq!(format_k(128_000), "128k");
    }

    #[test]
    fn test_percentage() {
        update(10_000, 128_000);
        assert_eq!(percentage(), 7);
    }

    #[test]
    fn test_percentage_zero_max() {
        update(0, 0);
        assert_eq!(percentage(), 0);
    }

    #[test]
    fn test_format_footer() {
        update(4_100, 128_000);
        assert_eq!(format_footer(), "context: 4.1k/128k (3%)");
    }

    #[test]
    fn test_format_footer_zero() {
        update(0, 0);
        assert_eq!(format_footer(), "");
    }
}
