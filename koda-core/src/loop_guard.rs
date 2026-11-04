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
//! 2. **Hard iteration cap (top-level only)** — absolute ceiling on
//!    the main inference loop. User can extend interactively.
//!    Sub-agent loops are **uncapped** as of #1110; they trust the
//!    model and rely on consecutive-identical detection, provider
//!    stop reasons, cancellation, and context bounds (P3 in DESIGN.md).
//! 3. **A reply repeating itself** ([`crate::loop_guard::ReplyRepetition`]), for local models
//!    only. A model can loop inside one reply, reasoning the same paragraph
//!    over and over, and never make a tool call for the check above to see.
//!    qwen-code, the Qwen team's own agent, watches the reply and its
//!    reasoning for this; Claude Code and Codex don't, and Koda doesn't for
//!    the models they serve.

use crate::providers::ToolCall;
use std::collections::{HashMap, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::LazyLock;

/// Default hard cap for the main inference loop.
pub const MAX_ITERATIONS_DEFAULT: u32 = 200;

/// Replies in a row cut off at the token cap before the turn ends.
pub const MAX_TRUNCATED_REPLIES: u32 = 3;

// `MAX_SUB_AGENT_ITERATIONS` deleted in #1110: per `DESIGN.md` P3 ("Build for
// the world six months from now"), sub-agents trust the model and rely on
// `LoopDetector`, provider stop reasons, cancellation, and context bounds
// instead of a hardcoded iteration count. Codex and Zed both ship without
// any per-sub-agent iteration cap.

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

/// Stable fingerprint: tool name + the first 200 bytes of args, cut back to
/// a char boundary so multi-byte text never splits.
fn fingerprint(name: &str, args: &str) -> String {
    let prefix = &args[..args.floor_char_boundary(200)];
    format!("{name}:{prefix}")
}

// ── Repetition inside one reply ───────────────────────────────────

/// Characters in each window the reply check compares.
const REPEAT_WINDOW: usize = 50;
/// Short repeats: this many sightings of one window, on average no more
/// than one and a half windows apart.
const CLUSTER_SIGHTINGS: usize = 10;
/// Long repeats: this many sightings of one window at exactly the same
/// spacing, three once the oldest have left the history.
const PERIODIC_SIGHTINGS: usize = 5;
const PERIODIC_SIGHTINGS_TRIMMED: usize = 3;
/// A long repeat must cover this many characters before it counts.
const PERIODIC_MIN_SPAN: usize = 1000;
/// How much of the reply the check keeps.
const REPEAT_HISTORY: usize = 4000;
/// The longest excerpt of the repeated text a warning quotes.
const EXCERPT_CHARS: usize = 80;

/// Replies in a row that may be stopped for repeating before the turn stops.
/// One Ornith run that passed repeated itself in four replies in a row.
pub const MAX_REPEATED_REPLIES: u32 = 5;

/// Lines that start a markdown table, list, heading or quote. Text in
/// these repeats by design, so the check starts over at them.
static MARKDOWN_BLOCK: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(?:^|\n)\s*(?:\|.*\||[|+-]{3,})|(?:^|\n)\s*[-*+]\s|(?:^|\n)\s*\d+\.\s|(?:^|\n)#+\s|(?:^|\n)>\s",
    )
    .expect("valid regex")
});
static DIVIDER: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^[-+_=*\u{2500}-\u{257F}]+$").expect("valid regex"));

/// Watches one streamed reply for a model repeating itself verbatim.
///
/// qwen-code's two rules (`loopDetectionService.ts`): a 50-character window
/// seen ten times about a window apart is a short chant, and one seen five
/// times at exactly the same spacing, over at least 1,000 characters, is a
/// long passage repeated. gemini-cli has only the first rule, which misses a
/// repeated paragraph: Ornith's loops in Koda's evals repeated 459 to 1,538
/// characters at a time. Reasoning is checked as well as text, since that is
/// where a local model loops. One change from qwen-code: a window of one
/// character repeated (code indentation, a box-drawing line) is skipped.
/// Replaying the check over Koda's eval history, those were its only false
/// alarms, and a model emitting one character forever hits the reply cap.
#[derive(Default)]
pub struct ReplyRepetition {
    /// The kept end of the reply.
    text: Vec<char>,
    /// Characters dropped from the front of `text`.
    dropped: usize,
    /// Where the next window starts, counting from the reply's start.
    next: usize,
    /// Where each window was seen, by hash.
    seen: HashMap<u64, Vec<usize>>,
    /// Inside a fenced code block of the reply text.
    in_code: bool,
}

impl ReplyRepetition {
    /// A check with nothing seen.
    pub fn new() -> Self {
        Self::default()
    }

    /// Start over, at a tool call: a repeat has to be in one stretch of text.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Add reasoning. Returns an excerpt of the repeated text once the reply
    /// is repeating itself.
    pub fn push_reasoning(&mut self, delta: &str) -> Option<String> {
        self.text.extend(delta.chars());
        self.trim();
        self.scan()
    }

    /// Add reply text. Code blocks are skipped, and markdown blocks start the
    /// check over, as in qwen-code: both repeat by design.
    pub fn push_text(&mut self, delta: &str) -> Option<String> {
        let fences = delta.matches("```").count();
        let divider = DIVIDER.is_match(delta);
        if fences > 0 || divider || MARKDOWN_BLOCK.is_match(delta) {
            let in_code = self.in_code;
            self.reset();
            self.in_code = in_code;
        }
        let was_in_code = self.in_code;
        if fences % 2 == 1 {
            self.in_code = !self.in_code;
        }
        if was_in_code || self.in_code || divider {
            return None;
        }
        self.push_reasoning(delta)
    }

    fn end(&self) -> usize {
        self.dropped + self.text.len()
    }

    /// The first position still inside the history.
    fn history_start(&self) -> usize {
        self.dropped + self.text.len().saturating_sub(REPEAT_HISTORY)
    }

    fn window(&self, at: usize) -> &[char] {
        let i = at - self.dropped;
        &self.text[i..i + REPEAT_WINDOW]
    }

    /// Drop the front once the history is a thousand characters over, so
    /// trimming doesn't run on every delta.
    fn trim(&mut self) {
        if self.text.len() <= REPEAT_HISTORY + 1000 {
            return;
        }
        let cut = self.text.len() - REPEAT_HISTORY;
        self.text.drain(..cut);
        self.dropped += cut;
        self.next = self.next.max(self.dropped);
        let dropped = self.dropped;
        self.seen.retain(|_, at| {
            at.retain(|&i| i >= dropped);
            !at.is_empty()
        });
    }

    fn scan(&mut self) -> Option<String> {
        while self.next + REPEAT_WINDOW <= self.end() {
            let at = self.next;
            self.next += 1;
            let window = self.window(at);
            if window.iter().all(|c| *c == window[0]) {
                continue;
            }
            let mut hasher = DefaultHasher::new();
            window.hash(&mut hasher);
            if let Some(excerpt) = self.sighting(hasher.finish(), at) {
                return Some(excerpt);
            }
        }
        None
    }

    fn sighting(&mut self, key: u64, at: usize) -> Option<String> {
        let start = self.history_start();
        let sightings = self.seen.entry(key).or_default();
        sightings.retain(|&i| i >= start);
        let Some(&first) = sightings.first() else {
            sightings.push(at);
            return None;
        };
        // A hash collision isn't a repeat.
        let (a, b) = (first - self.dropped, at - self.dropped);
        if self.text[a..a + REPEAT_WINDOW] != self.text[b..b + REPEAT_WINDOW] {
            return None;
        }
        sightings.push(at);
        let sightings = sightings.clone();
        (clustered(&sightings) || self.periodic(&sightings)).then(|| self.excerpt(&sightings))
    }

    fn periodic(&self, at: &[usize]) -> bool {
        if at.len() < PERIODIC_SIGHTINGS_TRIMMED {
            return false;
        }
        let last = at.len() - 1;
        let stride = at[last] - at[last - 1];
        let mut first = last;
        while first > 0 && at[first] - at[first - 1] == stride {
            first -= 1;
        }
        let run = last - first + 1;
        let end = at[last] + REPEAT_WINDOW;
        if run >= PERIODIC_SIGHTINGS {
            return self.repeats_with_stride(at[first], end, stride);
        }
        run >= PERIODIC_SIGHTINGS_TRIMMED
            && self.text.len() >= REPEAT_HISTORY
            && self.repeats_with_stride(self.history_start(), end, stride)
    }

    /// Whether `start..end` is one passage `stride` characters long, repeated.
    fn repeats_with_stride(&self, start: usize, end: usize, stride: usize) -> bool {
        if end - start < PERIODIC_MIN_SPAN {
            return false;
        }
        let span = &self.text[start - self.dropped..end - self.dropped];
        span[stride..] == span[..span.len() - stride]
    }

    /// One pass of the repeated text, cut to [`EXCERPT_CHARS`].
    fn excerpt(&self, at: &[usize]) -> String {
        let start = at[at.len() - 2];
        let stride = at[at.len() - 1] - start;
        let from = start - self.dropped;
        let to = (from + stride.min(EXCERPT_CHARS)).min(self.text.len());
        let excerpt: String = self.text[from..to].iter().collect();
        excerpt.split_whitespace().collect::<Vec<_>>().join(" ")
    }
}

/// Ten sightings of a window, on average no more than 75 characters apart.
fn clustered(at: &[usize]) -> bool {
    at.len() >= CLUSTER_SIGHTINGS && {
        let recent = &at[at.len() - CLUSTER_SIGHTINGS..];
        let span = recent[CLUSTER_SIGHTINGS - 1] - recent[0];
        span * 2 <= (CLUSTER_SIGHTINGS - 1) * REPEAT_WINDOW * 3
    }
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
    fn multibyte_char_across_the_200_byte_cut_does_not_panic() {
        // '─' is 3 bytes; after 199 ASCII bytes it spans bytes 199..202.
        let args = format!("{}{}", "a".repeat(199), "─".repeat(10));
        assert_eq!(
            fingerprint("Bash", &args),
            format!("Bash:{}", "a".repeat(199))
        );

        let mut d = LoopDetector::new();
        assert_eq!(d.record(&[call("Bash", &args)]), LoopAction::Ok);
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

    /// Feed `text` as reasoning in small deltas, as a stream arrives.
    /// Returns the excerpt and how far in it fired.
    fn reason(check: &mut ReplyRepetition, text: &str) -> Option<(String, usize)> {
        let chars: Vec<char> = text.chars().collect();
        for (n, piece) in chars.chunks(7).enumerate() {
            let piece: String = piece.iter().collect();
            if let Some(excerpt) = check.push_reasoning(&piece) {
                return Some((excerpt, (n + 1) * 7));
            }
        }
        None
    }

    /// The passage Ornith repeated 900 times in one reply on
    /// vim-normal-mode-bugs, 459 characters in all.
    const VIM_LOOP: &str = "So `e` should jump to the END of the next word. Let me find which \
        function handles `e`. Let me look at the keymap defaults and how vim_normal actions map \
        to keys. Actually, let me search for where these are used — but actually the mapping \
        from key to action is in the keymap defaults. The important thing is: what does the bug \
        describe? \"`e` does nothing when the cursor is already on the last letter of a word. In \
        Vim it jumps to the end of the next word.\"\n\n";

    #[test]
    fn a_repeated_paragraph_is_caught_within_a_few_passes() {
        let mut check = ReplyRepetition::new();
        let (excerpt, at) = reason(&mut check, &VIM_LOOP.repeat(50)).expect("loop caught");
        assert!(at < VIM_LOOP.chars().count() * 7, "fired after {at} chars");
        let passage = VIM_LOOP
            .repeat(2)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(passage.contains(&excerpt), "{excerpt}");
    }

    #[test]
    fn a_short_chant_is_caught() {
        let mut check = ReplyRepetition::new();
        let chant = "Let me check the file again. ".repeat(40);
        assert!(reason(&mut check, &chant).is_some());
    }

    #[test]
    fn reasoning_that_moves_on_is_not_a_loop() {
        let mut check = ReplyRepetition::new();
        let text: String = (0..300)
            .map(|i| {
                format!(
                    "Step {i}: read line {} and compare it with {}. ",
                    i * 3,
                    i * 7
                )
            })
            .collect();
        assert_eq!(reason(&mut check, &text), None);
    }

    #[test]
    fn one_character_repeated_is_not_a_loop() {
        // Indented code quoted in reasoning, and box-drawing lines: the only
        // false alarms when qwen-code's rules were replayed over Koda's evals.
        let mut check = ReplyRepetition::new();
        let text = format!(
            "The match arm is\n{}x => y,\nand then\n{}\nso the fix goes there.",
            " ".repeat(72),
            "─".repeat(120)
        );
        assert_eq!(reason(&mut check, &text.repeat(3)), None);
    }

    #[test]
    fn repeated_code_in_the_reply_text_is_not_a_loop() {
        let mut check = ReplyRepetition::new();
        assert_eq!(check.push_text("Here is the test:\n```rust\n"), None);
        for i in 0..100 {
            let line =
                format!("    assert_eq!(parse(\"{i}\"), Some({i})); // the same check again\n");
            assert_eq!(check.push_text(&line), None);
        }
        assert_eq!(check.push_text("```\n"), None);
    }

    #[test]
    fn repeated_reply_text_is_caught() {
        let mut check = ReplyRepetition::new();
        let found = VIM_LOOP
            .repeat(20)
            .split_inclusive(' ')
            .find_map(|word| check.push_text(word));
        assert!(found.is_some());
    }

    #[test]
    fn a_reset_forgets_what_was_seen() {
        let mut check = ReplyRepetition::new();
        let half = VIM_LOOP.repeat(2);
        assert_eq!(reason(&mut check, &half), None);
        check.reset();
        assert_eq!(reason(&mut check, &half), None);
    }

    #[test]
    fn a_long_reply_is_trimmed_and_still_caught() {
        let mut check = ReplyRepetition::new();
        let text: String = (0..2000)
            .map(|i| format!("Distinct sentence number {i} about the parser. "))
            .collect();
        assert_eq!(reason(&mut check, &text), None);
        assert!(check.text.len() <= REPEAT_HISTORY + 1000);
        assert!(reason(&mut check, &VIM_LOOP.repeat(20)).is_some());
    }
}
