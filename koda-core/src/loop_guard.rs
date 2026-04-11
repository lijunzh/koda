//! Loop detection and hard-cap for the inference loop.
//!
//! Tracks recent tool call fingerprints in a sliding window and flags
//! when the same tool+args combination repeats too many times.
//!
//! ## Detection methods
//!
//! Two complementary strategies catch runaway loops:
//!
//! 1. **Exact fingerprint** — `(tool_name, hash(args))` repeated
//!    `REPEAT_THRESHOLD` times in the window → immediate stop.
//!    Catches: model retrying the same command verbatim.
//!
//! 2. **Tool-name saturation** — same *tool name* (any args) appears
//!    `NAME_SATURATION_THRESHOLD` times in the window → immediate stop.
//!    Catches: model calling `Bash(ls -la)`, `Bash(ls -l)`, `Bash(ls)`…
//!    with slightly varying args (#826).
//!
//! Both strategies track *all* tools — read-only and mutating alike.
//! A model that calls `List` or `Grep` 8 times in 20 calls is clearly
//! stuck, regardless of whether those tools have side-effects.
//!
//! ## What happens on detection
//!
//! - **Soft limit** (repeated tool calls): emits a `Warn` event naming the
//!   culprit tool and repeat count; the turn ends and the user can send a
//!   follow-up message to continue
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

/// How many times the same exact fingerprint (tool+args) must appear to flag a loop.
pub const REPEAT_THRESHOLD: usize = 3;

/// How many times the same *tool name* (any args) must appear in the
/// window to flag a saturation loop. Higher than `REPEAT_THRESHOLD`
/// because it's normal to call the same tool a few times with different args.
pub const NAME_SATURATION_THRESHOLD: usize = 8;

/// Sliding window size (individual tool calls, not batches).
const WINDOW_SIZE: usize = 20;

/// After this many consecutive tool-call-only responses (no text), tool
/// definitions are suppressed for one turn to force a text response.
/// Prevents models (especially local ones) from entering infinite
/// tool-call loops without ever producing output (#826).
pub const TOOL_ONLY_RESPONSE_LIMIT: u32 = 5;

/// How many recent tool names to show in the hard-cap prompt.
const DISPLAY_RECENT: usize = 5;

// ── Loop detection ────────────────────────────────────────────────

/// Tracks repeated tool call patterns.
#[derive(Default)]
pub struct LoopDetector {
    /// Sliding window of recent tool fingerprints (tool+args).
    window: VecDeque<String>,
    /// Parallel window of just tool names (for saturation check).
    name_window: VecDeque<String>,
    /// Ring buffer of the last N tool names (for display).
    recent: VecDeque<String>,
}

impl LoopDetector {
    /// Create a new loop detector with empty history.
    pub fn new() -> Self {
        Self {
            window: VecDeque::new(),
            name_window: VecDeque::new(),
            recent: VecDeque::new(),
        }
    }

    /// Record a batch of tool calls.
    /// Returns `Some(culprit_description)` when a loop is detected.
    pub fn record(&mut self, tool_calls: &[ToolCall]) -> Option<String> {
        for tc in tool_calls {
            let fp = fingerprint(&tc.function_name, &tc.arguments);

            // Track ALL tools — read-only loops are just as wasteful (#826).
            self.window.push_back(fp);
            if self.window.len() > WINDOW_SIZE {
                self.window.pop_front();
            }

            self.name_window.push_back(tc.function_name.clone());
            if self.name_window.len() > WINDOW_SIZE {
                self.name_window.pop_front();
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
        // Strategy 1: exact fingerprint repeated REPEAT_THRESHOLD times
        let mut fp_counts: HashMap<&str, usize> = HashMap::new();
        for fp in &self.window {
            *fp_counts.entry(fp.as_str()).or_insert(0) += 1;
        }
        if let Some((fp, _)) = fp_counts.iter().find(|(_, n)| **n >= REPEAT_THRESHOLD) {
            return Some(fp.to_string());
        }

        // Strategy 2: same tool name saturates the window (#826)
        let mut name_counts: HashMap<&str, usize> = HashMap::new();
        for name in &self.name_window {
            *name_counts.entry(name.as_str()).or_insert(0) += 1;
        }
        if let Some((name, count)) = name_counts
            .iter()
            .find(|(_, n)| **n >= NAME_SATURATION_THRESHOLD)
        {
            return Some(format!("{name} (×{count} with varying args)"));
        }

        None
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
    fn detects_readonly_tool_loop() {
        // Read-only tools are now tracked (#826)
        let mut d = LoopDetector::new();
        let tc = call("Read", "{\"path\":\"src/main.rs\"}");
        assert!(d.record(std::slice::from_ref(&tc)).is_none());
        assert!(d.record(std::slice::from_ref(&tc)).is_none());
        assert!(
            d.record(std::slice::from_ref(&tc)).is_some(),
            "read-only tools should be caught at REPEAT_THRESHOLD"
        );
    }

    #[test]
    fn detects_name_saturation_with_varying_args() {
        // Same tool name but different args each time (#826)
        let mut d = LoopDetector::new();
        for i in 0..NAME_SATURATION_THRESHOLD {
            let args = format!("{{\"command\":\"ls -variant-{i}\"}}");
            let result = d.record(&[call("Bash", &args)]);
            if i < NAME_SATURATION_THRESHOLD - 1 {
                assert!(result.is_none(), "should not trigger at call {i}");
            } else {
                assert!(result.is_some(), "should trigger at call {i}");
                assert!(
                    result.unwrap().contains("varying args"),
                    "should mention varying args"
                );
            }
        }
    }

    #[test]
    fn mixed_tools_no_false_positive() {
        // Alternating between different tools shouldn't trigger saturation
        let mut d = LoopDetector::new();
        for i in 0..20 {
            let name = if i % 3 == 0 {
                "Bash"
            } else if i % 3 == 1 {
                "Read"
            } else {
                "List"
            };
            let args = format!("{{\"i\":{i}}}");
            assert!(
                d.record(&[call(name, &args)]).is_none(),
                "mixed tools should not trigger (call {i})"
            );
        }
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
