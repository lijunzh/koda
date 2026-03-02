//! Loop detection and hard-cap user prompt for the inference loop.
//!
//! Tracks recent tool call fingerprints in a sliding window and flags
//! when the same tool+args combination repeats too many times.
//! When the hard iteration cap is hit, prompts the user interactively
//! to continue or stop — falling back to stop in headless environments.

use crate::providers::ToolCall;
use crate::tui::SelectOption;
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

            // Sliding window for repetition detection
            self.window.push_back(fp);
            if self.window.len() > WINDOW_SIZE {
                self.window.pop_front();
            }

            // Ring buffer for display
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
/// Returns the number of additional iterations granted (0 = stop).
/// Defaults to stopping if the terminal is not interactive (headless mode).
pub fn ask_continue_or_stop(cap: u32, recent_names: &[String]) -> u32 {
    println!(
        "\n  \x1b[33m\u{26a0}  Hard cap reached ({cap} iterations).\x1b[0m"
    );

    if !recent_names.is_empty() {
        println!("  Last tool calls:");
        for name in recent_names {
            println!("    \x1b[90m\u{25cf}\x1b[0m {name}");
        }
    }
    println!();

    let options = vec![
        SelectOption::new("Stop", "End the task here"),
        SelectOption::new("+50 more", "Continue for 50 more iterations"),
        SelectOption::new("+200 more", "Continue for 200 more iterations"),
    ];

    match crate::tui::select("Continue?", &options, 0) {
        Ok(Some(1)) => 50,
        Ok(Some(2)) => 200,
        _ => 0, // Stop — includes headless / non-TTY / Esc
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
        }
    }

    #[test]
    fn no_loop_on_unique_calls() {
        let mut d = LoopDetector::new();
        assert!(d.record(&[call("Read", "{\"path\":\"a.rs\"}")]).is_none());
        assert!(d.record(&[call("Read", "{\"path\":\"b.rs\"}")]).is_none());
        assert!(d.record(&[call("Edit", "{\"path\":\"a.rs\"}")]).is_none());
    }

    #[test]
    fn detects_repeated_identical_call() {
        let mut d = LoopDetector::new();
        let tc = call("Read", "{\"path\":\"src/main.rs\"}");
        assert!(d.record(&[tc.clone()]).is_none());
        assert!(d.record(&[tc.clone()]).is_none());
        // Third repetition should trigger
        assert!(d.record(&[tc]).is_some());
    }

    #[test]
    fn different_args_not_a_loop() {
        let mut d = LoopDetector::new();
        for i in 0..10 {
            let args = format!("{{\"path\":\"file{i}.rs\"}}");
            assert!(d.record(&[call("Read", &args)]).is_none());
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
