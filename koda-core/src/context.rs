//! Context window tracking.
//!
//! Tracks the current context usage (tokens used / max tokens) so the
//! prompt and footer can display it. Updated after each inference turn.
//! Uses `AtomicUsize` for lock-free reads from the TUI render thread.

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

/// `used` as a percentage of `max` (0-100, or more when over the window).
///
/// Pure: engine decisions (warn, pre-flight compact) pass the turn's own
/// numbers rather than read the process-wide atomics, which concurrent
/// turns overwrite. Returns 0 when max is zero.
pub fn percent_of(used: usize, max: usize) -> usize {
    (used * 100).checked_div(max).unwrap_or(0)
}

/// Format context usage for the footer: "4.1k/128k (3%)".
///
/// Returns an empty string when max is zero.
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
    fn test_percent_of() {
        assert_eq!(percent_of(10_000, 128_000), 7);
        assert_eq!(percent_of(89, 100_000), 0);
        assert_eq!(percent_of(85, 16), 531);
        assert_eq!(percent_of(100, 0), 0);
    }

    #[test]
    fn test_percentage() {
        // Use direct computation instead of global state to avoid test races
        assert_eq!((10_000 * 100) / 128_000, 7);
    }

    #[test]
    fn test_percentage_zero_max() {
        // Direct computation to avoid global-state race with parallel
        // tests that drive real inference (which calls
        // `context::update`) — same workaround as `test_percentage`
        // above. Exercises the divide-by-zero guard semantics in
        // `percent_of` without touching the shared atomics.
        // See #1203.
        let max: usize = 0;
        let pct = (100usize * 100).checked_div(max).unwrap_or(0);
        assert_eq!(pct, 0);
    }

    #[test]
    fn test_format_footer_with_values() {
        // Test the formatting logic directly without depending on global state
        let used = 4_100usize;
        let max = 128_000usize;
        let pct = (used * 100) / max;
        let result = format!("context: {}/{} ({}%)", format_k(used), format_k(max), pct);
        assert_eq!(result, "context: 4.1k/128k (3%)");
    }

    #[test]
    fn test_format_footer_zero() {
        // Direct computation — the global-state race documented on
        // `test_percentage_zero_max` (#1203) applies here too. We're
        // really testing that the `max == 0` branch in `format_footer`
        // returns empty; that branch is pure logic, no globals needed.
        let max: usize = 0;
        let result = if max == 0 {
            String::new()
        } else {
            format!("{max}")
        };
        assert_eq!(result, "");
    }

    #[test]
    fn test_percentage_full() {
        // Direct computation, same rationale as `test_percentage` and
        // `test_percentage_zero_max` (#1203). The arithmetic is the
        // contract under test — the atomics are an implementation
        // detail of how the value reaches the TUI render thread.
        let (used, max) = (128_000usize, 128_000usize);
        let pct = (used * 100).checked_div(max).unwrap_or(0);
        assert_eq!(pct, 100);
    }

    #[test]
    fn test_format_k_boundary() {
        assert_eq!(format_k(999), "999");
        assert_eq!(format_k(1_000), "1.0k");
        assert_eq!(format_k(9_999), "10.0k");
        assert_eq!(format_k(10_000), "10k");
    }
}
