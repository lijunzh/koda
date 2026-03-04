//! Loop detection and hard-cap user prompt for the inference loop.
//!
//! Tracks recent tool call fingerprints in a sliding window and flags
//! when the same tool+args combination repeats too many times.
//! When the hard iteration cap is hit, prompts the user interactively
//! to continue or stop — falling back to stop in headless environments.

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
            if is_mutating_tool(&tc.function_name) {
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

/// Tools that can cause destructive/mutating loops if repeated blindly.
/// Read-only tools (Read, List, Grep) are excluded to allow safe exploration.
fn is_mutating_tool(name: &str) -> bool {
    matches!(
        name,
        "Bash" | "Edit" | "Write" | "Delete" | "MemoryWrite" | "CreateAgent" | "InvokeAgent"
    )
}

// ── Hard-cap prompt ───────────────────────────────────────────────

/// Prompt the user when the hard iteration cap is hit.
///
/// Options for continuing after hitting the hard cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopContinuation {
    Stop,
    Continue50,
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

/// Returns the number of additional iterations granted (0 = stop).
///
/// The `prompt_fn` callback is responsible for asking the user (terminal,
/// server, or headless). It receives `(cap, recent_tool_names)` and returns
/// the user's choice.
pub fn ask_continue_or_stop(
    cap: u32,
    recent_names: &[String],
    prompt_fn: &dyn Fn(u32, &[String]) -> LoopContinuation,
) -> u32 {
    prompt_fn(cap, recent_names).extra_iterations()
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
