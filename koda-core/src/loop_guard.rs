//! Loop detection for the inference loop.
//!
//! Modeled after Gemini CLI's approach: simple consecutive-identical-call
//! detection + feedback injection instead of hard stops. No windowed
//! fingerprinting, no name saturation heuristics, no tool-only suppression.
//!
//! ## Design philosophy
//!
//! Claude Code and Codex have **zero** loop detection — they trust the model.
//! Gemini CLI has the only thoughtful approach: detect consecutive identical
//! tool calls (same name + args), then inject a "take a step back" feedback
//! message to nudge the model out of the loop. Hard-stop only on the 2nd
//! detection (model ignored the feedback).
//!
//! ## What we DON'T do (and why)
//!
//! - **No windowed fingerprint tracking** — nobody else does this.
//! - **No tool-name saturation** — editing 12 files in a refactoring is normal.
//! - **No tool-only response suppression** — efficient models work silently.
//! - **No per-turn tool call cap** — frontier models emit 30+ parallel calls.
//! - **No deduplication** — if a model emits 66 identical calls, the user
//!   should see that and switch models, not have us silently paper over it.
//!
//! ## What we DO
//!
//! 1. **Consecutive identical calls** — same `(tool, args)` called
//!    `CONSECUTIVE_REPEAT_THRESHOLD` times in a row → inject feedback.
//! 2. **Hard iteration cap** — absolute ceiling on loop iterations.
//!    User can extend interactively.

use crate::providers::ToolCall;
use std::collections::VecDeque;

/// Default hard cap for the main inference loop.
pub const MAX_ITERATIONS_DEFAULT: u32 = 200;

/// Hard cap for sub-agent loops.
pub const MAX_SUB_AGENT_ITERATIONS: usize = 20;

/// How many **consecutive** identical tool calls (same name + args) trigger
/// loop detection. "Consecutive" means the same fingerprint appears this
/// many times with no other tool call in between.
///
/// Set to 5 to match Gemini CLI's `TOOL_CALL_LOOP_THRESHOLD`.
/// A normal "read → edit → test" cycle never triggers this because each
/// step is a different tool call.
const CONSECUTIVE_REPEAT_THRESHOLD: usize = 5;

/// How many recent tool names to show in the hard-cap prompt.
const DISPLAY_RECENT: usize = 5;

// ── Loop detection ────────────────────────────────────────────────

/// What to do when a loop is detected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopAction {
    /// No loop detected — continue normally.
    Ok,
    /// First detection — inject feedback message to nudge the model.
    /// Contains a descriptive message for the feedback injection.
    InjectFeedback(String),
    /// Second detection — model ignored feedback, hard stop.
    HardStop(String),
}

/// Tracks consecutive identical tool calls.
///
/// Detection is simple: if the last N tool calls all have the same
/// fingerprint (tool name + args), that's a loop. On first detection,
/// the caller injects a feedback message. On second detection (model
/// ignored the feedback), the caller hard-stops.
pub struct LoopDetector {
    /// The fingerprint of the last tool call.
    last_fingerprint: Option<String>,
    /// How many consecutive times we've seen `last_fingerprint`.
    consecutive_count: usize,
    /// How many times we've detected a loop in this session.
    detection_count: u32,
    /// Ring buffer of recent tool names (for display in hard-cap prompt).
    recent: VecDeque<String>,
}

impl Default for LoopDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl LoopDetector {
    /// Create a new loop detector with empty history.
    pub fn new() -> Self {
        Self {
            last_fingerprint: None,
            consecutive_count: 0,
            detection_count: 0,
            recent: VecDeque::new(),
        }
    }

    /// Record a batch of tool calls and check for loops.
    ///
    /// Returns a [`LoopAction`] indicating what the caller should do.
    pub fn record(&mut self, tool_calls: &[ToolCall]) -> LoopAction {
        for tc in tool_calls {
            let fp = fingerprint(&tc.function_name, &tc.arguments);

            // Update consecutive counter
            if self.last_fingerprint.as_ref() == Some(&fp) {
                self.consecutive_count += 1;
            } else {
                self.last_fingerprint = Some(fp);
                self.consecutive_count = 1;
            }

            // Update display ring buffer
            self.recent.push_back(tc.function_name.clone());
            if self.recent.len() > DISPLAY_RECENT {
                self.recent.pop_front();
            }
        }

        self.check()
    }

    /// Clear the detection state after feedback injection so the model
    /// gets a fresh chance. Increments `detection_count` so the next
    /// trigger will be a hard stop.
    pub fn clear_after_feedback(&mut self) {
        self.detection_count += 1;
        self.last_fingerprint = None;
        self.consecutive_count = 0;
    }

    /// Recent tool names (most recent last), for display in the hard-cap prompt.
    pub fn recent_names(&self) -> Vec<String> {
        self.recent.iter().cloned().collect()
    }

    fn check(&self) -> LoopAction {
        if self.consecutive_count < CONSECUTIVE_REPEAT_THRESHOLD {
            return LoopAction::Ok;
        }

        let fp = self.last_fingerprint.as_deref().unwrap_or("unknown");
        let tool_name = fp.split(':').next().unwrap_or(fp);
        let detail = format!(
            "'{tool_name}' called {n} times consecutively with identical arguments",
            n = self.consecutive_count,
        );

        if self.detection_count == 0 {
            // First detection — inject feedback
            LoopAction::InjectFeedback(detail)
        } else {
            // Already injected feedback before — hard stop
            LoopAction::HardStop(detail)
        }
    }
}

/// Stable fingerprint: tool name + first 200 chars of args.
fn fingerprint(name: &str, args: &str) -> String {
    let prefix = &args[..args.len().min(200)];
    format!("{name}:{prefix}")
}

// ── Hard-cap prompt ───────────────────────────────────────────────

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
        assert_eq!(
            d.record(&[call("Edit", "{\"path\":\"a.rs\"}")]),
            LoopAction::Ok
        );
        assert_eq!(
            d.record(&[call("Edit", "{\"path\":\"b.rs\"}")]),
            LoopAction::Ok
        );
        assert_eq!(
            d.record(&[call("Bash", "{\"cmd\":\"ls\"}")]),
            LoopAction::Ok
        );
    }

    #[test]
    fn detects_consecutive_identical_calls() {
        let mut d = LoopDetector::new();
        let tc = call("Edit", "{\"path\":\"src/main.rs\"}");
        for _ in 0..CONSECUTIVE_REPEAT_THRESHOLD - 1 {
            assert_eq!(d.record(std::slice::from_ref(&tc)), LoopAction::Ok);
        }
        // Should trigger feedback on threshold
        assert!(matches!(
            d.record(std::slice::from_ref(&tc)),
            LoopAction::InjectFeedback(_)
        ));
    }

    #[test]
    fn different_tool_resets_consecutive_count() {
        let mut d = LoopDetector::new();
        let tc = call("Edit", "{\"path\":\"src/main.rs\"}");
        // Almost at threshold
        for _ in 0..CONSECUTIVE_REPEAT_THRESHOLD - 2 {
            assert_eq!(d.record(std::slice::from_ref(&tc)), LoopAction::Ok);
        }
        // Different tool resets the count
        assert_eq!(
            d.record(&[call("Bash", "{\"cmd\":\"test\"}")]),
            LoopAction::Ok
        );
        // Back to same tool — starts from 1 again
        for _ in 0..CONSECUTIVE_REPEAT_THRESHOLD - 1 {
            assert_eq!(d.record(std::slice::from_ref(&tc)), LoopAction::Ok);
        }
        assert!(matches!(
            d.record(std::slice::from_ref(&tc)),
            LoopAction::InjectFeedback(_)
        ));
    }

    #[test]
    fn read_edit_test_cycle_never_triggers() {
        // The most common coding workflow should NEVER trigger.
        let mut d = LoopDetector::new();
        let test_cmd = "{\"command\":\"cargo test\"}";
        let read_args = "{\"path\":\"src/lib.rs\"}";

        for cycle in 0..20 {
            assert_eq!(
                d.record(&[call("Read", read_args)]),
                LoopAction::Ok,
                "read should not trigger at cycle {cycle}"
            );
            let edit_args = format!("{{\"path\":\"src/lib.rs\",\"old\":\"v{cycle}\"}}");
            assert_eq!(
                d.record(&[call("Edit", &edit_args)]),
                LoopAction::Ok,
                "edit should not trigger at cycle {cycle}"
            );
            assert_eq!(
                d.record(&[call("Bash", test_cmd)]),
                LoopAction::Ok,
                "test should not trigger at cycle {cycle}"
            );
        }
    }

    #[test]
    fn feedback_then_hard_stop() {
        let mut d = LoopDetector::new();
        let tc = call("Read", "{\"path\":\"stuck.rs\"}");

        // First detection → feedback
        for _ in 0..CONSECUTIVE_REPEAT_THRESHOLD {
            d.record(std::slice::from_ref(&tc));
        }
        // The last record returned InjectFeedback — now simulate the
        // caller clearing state and the model looping again
        d.detection_count = 1; // feedback was injected
        d.clear_after_feedback();

        // Second detection → hard stop
        for _ in 0..CONSECUTIVE_REPEAT_THRESHOLD {
            d.record(std::slice::from_ref(&tc));
        }
        assert!(matches!(d.check(), LoopAction::HardStop(_)));
    }

    #[test]
    fn parallel_calls_same_tool_not_a_loop() {
        // 10 parallel Read calls with DIFFERENT args in one batch — not a loop
        let mut d = LoopDetector::new();
        let batch: Vec<ToolCall> = (0..10)
            .map(|i| call("Read", &format!("{{\"path\":\"file{i}.rs\"}}")))
            .collect();
        assert_eq!(d.record(&batch), LoopAction::Ok);
    }

    #[test]
    fn same_tool_different_args_not_consecutive() {
        // Same tool name but different args each time — not consecutive
        let mut d = LoopDetector::new();
        for i in 0..20 {
            let args = format!("{{\"command\":\"ls -variant-{i}\"}}");
            assert_eq!(
                d.record(&[call("Bash", &args)]),
                LoopAction::Ok,
                "different args should not trigger at call {i}"
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
}
