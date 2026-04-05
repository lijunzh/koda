//! Loop detection and hard-cap for the inference loop.
//!
//! Tracks recent tool call fingerprints in a sliding window and flags
//! when the same tool+args combination repeats too many times.
//!
//! ## Detection method
//!
//! Each tool call is fingerprinted as `(tool_name, hash(args))`. The guard
//! maintains a sliding window of the last N fingerprints. When the same
//! fingerprint appears more than the threshold, a loop is detected.
//!
//! ## What happens on detection
//!
//! - **Soft limit** (repeated tool calls): injects a system message telling
//!   the model it's looping, asking it to try a different approach
//! - **Hard limit** (iteration cap): prompts the user interactively to
//!   continue or stop — falls back to stop in headless environments
//!
//! ## Why this matters
//!
//! Without loop detection, a confused model can burn thousands of tokens
//! retrying the same failing edit or grep. The guard is the safety net.

use crate::providers::ToolCall;
use std::collections::{HashMap, VecDeque};

/// Default hard cap for the main inference loop.
pub const MAX_ITERATIONS_DEFAULT: u32 = 200;

/// Hard cap for sub-agent loops.
pub const MAX_SUB_AGENT_ITERATIONS: usize = 20;

/// How many times the same fingerprint must appear to flag a loop.
const REPEAT_THRESHOLD: usize = 3;

/// Sliding window size (individual tool calls, not batches).
const WINDOW_SIZE: usize = 20;

/// How many recent tool names to show in the hard-cap prompt.
const DISPLAY_RECENT: usize = 5;

// ── Loop detection ────────────────────────────────────────────────

/// Tracks repeated tool call patterns.
#[derive(Default)]
pub struct LoopDetector {
    /// Sliding window of recent tool fingerprints.
    window: VecDeque<String>,
    /// Ring buffer of the last N tool names (for display only).
    recent: VecDeque<String>,
}

impl LoopDetector {
    /// Create a new loop detector with empty history.
    pub fn new() -> Self {
        Self {
            window: VecDeque::new(),
            recent: VecDeque::new(),
        }
    }

    /// Record a batch of tool calls.
    /// Returns `Some(repeated_fingerprint)` when a loop is detected.
    pub fn record(&mut self, tool_calls: &[ToolCall]) -> Option<String> {
        for tc in tool_calls {
            let fp = fingerprint(&tc.function_name, &tc.arguments);

            // Sliding window for loop detection ONLY tracks mutating tools.
            // Repeating read-only operations is handled by stale-read optimization.
            if crate::tools::is_mutating_tool(&tc.function_name) {
                self.window.push_back(fp);
                if self.window.len() > WINDOW_SIZE {
                    self.window.pop_front();
                }
            }

            // Ring buffer for display always tracks all tools
            self.recent.push_back(tc.function_name.clone());
            if self.recent.len() > DISPLAY_RECENT {
                self.recent.pop_front();
            }
        }

        self.check()
    }

    /// Recent tool names (most recent last), for display in the hard-cap prompt.
    pub fn recent_names(&self) -> Vec<String> {
        self.recent.iter().cloned().collect()
    }

    fn check(&self) -> Option<String> {
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for fp in &self.window {
            *counts.entry(fp.as_str()).or_insert(0) += 1;
        }
        counts
            .into_iter()
            .find(|(_, n)| *n >= REPEAT_THRESHOLD)
            .map(|(fp, _)| fp.to_string())
    }
}

/// Stable fingerprint: tool name + first 200 chars of args.
fn fingerprint(name: &str, args: &str) -> String {
    let prefix = &args[..args.len().min(200)];
    format!("{name}:{prefix}")
}

// ── Hard-cap prompt ───────────────────────────────────────────────

/// Prompt the user when the hard iteration cap is hit.
///
/// Options for continuing after hitting the hard cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopContinuation {
    /// Stop the inference loop.
    Stop,
    /// Continue for 50 more iterations.
    Continue50,
    /// Continue for 200 more iterations.
    Continue200,
}

impl LoopContinuation {
    /// Number of additional iterations granted.
    pub fn extra_iterations(self) -> u32 {
        match self {
            Self::Stop => 0,
            Self::Continue50 => 50,
            Self::Continue200 => 200,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: "x".into(),
            function_name: name.into(),
            arguments: args.into(),
            thought_signature: None,
        }
    }

    #[test]
    fn no_loop_on_unique_calls() {
        let mut d = LoopDetector::new();
        assert!(d.record(&[call("Edit", "{\"path\":\"a.rs\"}")]).is_none());
        assert!(d.record(&[call("Edit", "{\"path\":\"b.rs\"}")]).is_none());
        assert!(d.record(&[call("Bash", "{\"cmd\":\"ls\"}")]).is_none());
    }

    #[test]
    fn detects_repeated_identical_call() {
        let mut d = LoopDetector::new();
        let tc = call("Edit", "{\"path\":\"src/main.rs\"}");
        assert!(d.record(std::slice::from_ref(&tc)).is_none());
        assert!(d.record(std::slice::from_ref(&tc)).is_none());
        // Third repetition should trigger
        assert!(d.record(std::slice::from_ref(&tc)).is_some());
    }

    #[test]
    fn different_args_not_a_loop() {
        let mut d = LoopDetector::new();
        for i in 0..10 {
            let args = format!("{{\"path\":\"file{i}.rs\"}}");
            assert!(d.record(&[call("Edit", &args)]).is_none());
        }
    }

    #[test]
    fn ignores_readonly_tools() {
        let mut d = LoopDetector::new();
        let tc = call("Read", "{\"path\":\"src/main.rs\"}");
        assert!(d.record(std::slice::from_ref(&tc)).is_none());
        assert!(d.record(std::slice::from_ref(&tc)).is_none());
        assert!(d.record(std::slice::from_ref(&tc)).is_none());
        assert!(d.record(std::slice::from_ref(&tc)).is_none());
        // Even 4 repetitions shouldn't trigger because Read is ignored
        assert!(d.check().is_none());
    }

    #[test]
    fn recent_names_tracks_last_five() {
        let mut d = LoopDetector::new();
        for i in 0..8 {
            let name = format!("Tool{i}");
            d.record(&[call(&name, "{}")]);
        }
        let names = d.recent_names();
        assert_eq!(names.len(), 5);
        assert_eq!(names[0], "Tool3");
        assert_eq!(names[4], "Tool7");
    }

    #[test]
    fn fingerprint_truncates_long_args() {
        let long_args = "x".repeat(500);
        let fp = fingerprint("Bash", &long_args);
        // name + ":" + 200 chars
        assert_eq!(fp.len(), "Bash:".len() + 200);
    }
}
