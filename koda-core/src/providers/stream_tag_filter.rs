//! Streaming tag filter for LLM responses.
//!
//! Models served via OpenAI-compatible endpoints may emit XML tags and special
//! tokens directly in their text output instead of using structured APIs.
//! This filter intercepts them at the **provider layer**:
//!
//! - **Thinking tags** (`<think>`, `<thinking>`, `<reasoning>`, `<reflection>`)
//!   → converted to `ThinkingDelta` chunks
//! - **Tool call tags** (`<tool_call>`, `<function_call>`, `<tool_use>`)
//!   → suppressed with a warning (model lacks native function calling)
//! - **Special tokens** (`<|im_start|>`, `<|endoftext|>`, `<|eot_id|>`, etc.)
//!   → stripped from output
//!
//! The inference engine only sees clean `StreamChunk` variants — no raw tag
//! parsing needed upstream.
//!
//! ## Design
//!
//! The filter is a stateful streaming parser that buffers incoming text to
//! handle tags spanning chunk boundaries. It holds back up to `MAX_TAG_LEN`
//! bytes at the end of the buffer to avoid splitting a tag across calls.
//!
//! See issue #214 for the full tag taxonomy.

use crate::providers::StreamChunk;

// ── Tag definitions ──────────────────────────────────────────

/// What to do with content inside a matched tag pair.
#[derive(Debug, Clone, Copy, PartialEq)]
enum TagAction {
    /// Convert inner content to `ThinkingDelta`.
    Thinking,
    /// Silently drop inner content (log a warning once).
    Suppress,
}

/// A paired open/close tag to detect in the stream.
struct TagPair {
    open: &'static str,
    close: &'static str,
    action: TagAction,
}

/// All recognized tag pairs, ordered by open-tag length descending so longer
/// matches take priority (e.g. `<thinking>` before `<think>`).
const TAG_PAIRS: &[TagPair] = &[
    // Thinking tags → ThinkingDelta
    TagPair {
        open: "<reflection>",
        close: "</reflection>",
        action: TagAction::Thinking,
    },
    TagPair {
        open: "<reasoning>",
        close: "</reasoning>",
        action: TagAction::Thinking,
    },
    TagPair {
        open: "<thinking>",
        close: "</thinking>",
        action: TagAction::Thinking,
    },
    TagPair {
        open: "<think>",
        close: "</think>",
        action: TagAction::Thinking,
    },
    // Tool call tags → suppress
    TagPair {
        open: "<function_call>",
        close: "</function_call>",
        action: TagAction::Suppress,
    },
    TagPair {
        open: "<tool_call>",
        close: "</tool_call>",
        action: TagAction::Suppress,
    },
    TagPair {
        open: "<tool_use>",
        close: "</tool_use>",
        action: TagAction::Suppress,
    },
];

/// Special tokens to strip (standalone, not paired).
const SPECIAL_TOKENS: &[&str] = &[
    "<|endoftext|>",
    "<|im_start|>",
    "<|assistant|>",
    "<|im_end|>",
    "<|eot_id|>",
    "<|system|>",
    "<|user|>",
];

/// Maximum byte length of any open tag, close tag, or special token.
/// Used to size the hold-back buffer for chunk-boundary detection.
///
/// `</function_call>` = 16 bytes (the longest).
const MAX_TAG_LEN: usize = 16;

// ── Helpers ──────────────────────────────────────────────────

/// Find the largest byte index ≤ `index` that is a valid char boundary.
fn floor_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Result of scanning the buffer for the earliest known pattern.
#[derive(Debug)]
enum EarliestMatch {
    /// An open tag was found: emit text before it, transition to InBlock.
    OpenTag { pos: usize, pair_idx: usize },
    /// A stale close tag was found: emit text before it, skip the tag.
    StaleClose { pos: usize, close_len: usize },
    /// A special token was found: emit text before it, skip the token.
    SpecialToken { pos: usize, token_len: usize },
    /// Nothing found.
    None,
}

/// Scan `buffer` for the earliest open tag, stale close tag, or special token.
fn find_earliest_match(buffer: &str) -> EarliestMatch {
    let mut best_pos = usize::MAX;
    let mut best = EarliestMatch::None;

    // Check open tags
    for (i, pair) in TAG_PAIRS.iter().enumerate() {
        if let Some(pos) = buffer.find(pair.open)
            && pos < best_pos
        {
            best_pos = pos;
            best = EarliestMatch::OpenTag { pos, pair_idx: i };
        }
    }

    // Check stale close tags (only if they appear before any open tag)
    for pair in TAG_PAIRS {
        if let Some(pos) = buffer.find(pair.close)
            && pos < best_pos
        {
            best_pos = pos;
            best = EarliestMatch::StaleClose {
                pos,
                close_len: pair.close.len(),
            };
        }
    }

    // Check special tokens
    for token in SPECIAL_TOKENS {
        if let Some(pos) = buffer.find(token)
            && pos < best_pos
        {
            best_pos = pos;
            best = EarliestMatch::SpecialToken {
                pos,
                token_len: token.len(),
            };
        }
    }

    best
}

// ── Filter state ─────────────────────────────────────────────

/// Current parser state.
#[derive(Debug)]
enum FilterState {
    /// Not inside any known block.
    Normal,
    /// Inside a matched open/close tag pair.
    InBlock {
        close_tag: &'static str,
        action: TagAction,
    },
}

/// A streaming filter that intercepts XML tags and special tokens in LLM output.
///
/// Feed it `StreamChunk::TextDelta` chunks and it emits a transformed stream
/// where known tags are handled appropriately.
pub struct StreamTagFilter {
    buffer: String,
    state: FilterState,
    /// Only warn about suppressed tool calls once per stream.
    warned_suppressed: bool,
}

impl StreamTagFilter {
    /// Create a new filter with empty state.
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            state: FilterState::Normal,
            warned_suppressed: false,
        }
    }

    /// Process a stream chunk. Returns zero or more output chunks.
    pub fn process(&mut self, chunk: StreamChunk) -> Vec<StreamChunk> {
        match chunk {
            StreamChunk::TextDelta(delta) => self.process_text(&delta),
            other => vec![other],
        }
    }

    /// Flush any remaining buffered content (call when stream ends).
    pub fn flush(&mut self) -> Vec<StreamChunk> {
        if self.buffer.is_empty() {
            return vec![];
        }
        let remaining = std::mem::take(&mut self.buffer);
        match self.state {
            FilterState::InBlock {
                action: TagAction::Thinking,
                ..
            } => {
                vec![StreamChunk::ThinkingDelta(remaining)]
            }
            FilterState::InBlock {
                action: TagAction::Suppress,
                ..
            } => {
                // Drop suppressed content
                vec![]
            }
            FilterState::Normal => {
                vec![StreamChunk::TextDelta(remaining)]
            }
        }
    }

    fn process_text(&mut self, delta: &str) -> Vec<StreamChunk> {
        self.buffer.push_str(delta);
        let mut output = Vec::new();

        loop {
            match self.state {
                FilterState::InBlock { close_tag, action } => {
                    if let Some(end_pos) = self.buffer.find(close_tag) {
                        // Found the close tag — extract content and transition
                        let content = self.buffer[..end_pos].to_string();
                        self.buffer = self.buffer[end_pos + close_tag.len()..].to_string();
                        self.state = FilterState::Normal;

                        match action {
                            TagAction::Thinking if !content.is_empty() => {
                                output.push(StreamChunk::ThinkingDelta(content));
                            }
                            TagAction::Suppress => {
                                self.warn_suppressed();
                            }
                            _ => {}
                        }
                        continue;
                    } else {
                        // Still accumulating — emit safe content, hold back
                        // enough bytes in case the close tag spans chunks.
                        let hold = close_tag.len();
                        let safe_len = floor_char_boundary(
                            &self.buffer,
                            self.buffer.len().saturating_sub(hold),
                        );
                        if safe_len > 0 {
                            let safe = self.buffer[..safe_len].to_string();
                            self.buffer = self.buffer[safe_len..].to_string();
                            if action == TagAction::Thinking {
                                output.push(StreamChunk::ThinkingDelta(safe));
                            }
                            // Suppress action: drop the content silently
                        }
                        break;
                    }
                }
                FilterState::Normal => {
                    match find_earliest_match(&self.buffer) {
                        EarliestMatch::OpenTag { pos, pair_idx } => {
                            let pair = &TAG_PAIRS[pair_idx];
                            let before = self.buffer[..pos].to_string();
                            self.buffer = self.buffer[pos + pair.open.len()..].to_string();
                            self.state = FilterState::InBlock {
                                close_tag: pair.close,
                                action: pair.action,
                            };
                            if !before.is_empty() {
                                output.push(StreamChunk::TextDelta(before));
                            }
                            continue;
                        }
                        EarliestMatch::StaleClose { pos, close_len } => {
                            let before = self.buffer[..pos].to_string();
                            self.buffer = self.buffer[pos + close_len..].to_string();
                            if !before.is_empty() {
                                output.push(StreamChunk::TextDelta(before));
                            }
                            continue;
                        }
                        EarliestMatch::SpecialToken { pos, token_len } => {
                            let before = self.buffer[..pos].to_string();
                            self.buffer = self.buffer[pos + token_len..].to_string();
                            if !before.is_empty() {
                                output.push(StreamChunk::TextDelta(before));
                            }
                            continue;
                        }
                        EarliestMatch::None => {
                            // Nothing found — emit safe content, hold back for
                            // potential tag spanning the chunk boundary.
                            let safe_len = floor_char_boundary(
                                &self.buffer,
                                self.buffer.len().saturating_sub(MAX_TAG_LEN),
                            );
                            if safe_len > 0 {
                                let safe = self.buffer[..safe_len].to_string();
                                self.buffer = self.buffer[safe_len..].to_string();
                                output.push(StreamChunk::TextDelta(safe));
                            }
                            break;
                        }
                    }
                }
            }
        }

        output
    }

    fn warn_suppressed(&mut self) {
        if !self.warned_suppressed {
            self.warned_suppressed = true;
            tracing::warn!(
                "Model emitted XML tool-call tags in text stream (no native function calling). \
                 Tool calls from this model will not execute. Consider using a model with native \
                 function calling support."
            );
        }
    }
}

impl Default for StreamTagFilter {
    fn default() -> Self {
        Self::new()
    }
}

// Re-export old name for backward compat within the crate.
/// Backward-compatible alias for [`StreamTagFilter`].
pub type ThinkTagFilter = StreamTagFilter;

#[cfg(test)]
mod tests {
    use super::*;

    // ── Helpers ──────────────────────────────────────────────

    fn collect_text(chunks: &[StreamChunk]) -> String {
        chunks
            .iter()
            .filter_map(|c| match c {
                StreamChunk::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    fn collect_thinking(chunks: &[StreamChunk]) -> String {
        chunks
            .iter()
            .filter_map(|c| match c {
                StreamChunk::ThinkingDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    fn run_filter(inputs: &[&str]) -> Vec<StreamChunk> {
        let mut filter = StreamTagFilter::new();
        let mut all = Vec::new();
        for input in inputs {
            all.extend(filter.process(StreamChunk::TextDelta((*input).into())));
        }
        all.extend(filter.flush());
        all
    }

    // ── Think tag tests (existing behavior, issue #191) ─────

    #[test]
    fn no_tags_passthrough() {
        let all = run_filter(&["Hello world"]);
        assert_eq!(collect_text(&all), "Hello world");
        assert!(collect_thinking(&all).is_empty());
    }

    #[test]
    fn think_block_single_chunk() {
        let all = run_filter(&["<think>reasoning here</think>answer"]);
        assert_eq!(collect_thinking(&all), "reasoning here");
        assert_eq!(collect_text(&all), "answer");
    }

    #[test]
    fn think_block_across_chunks() {
        let all = run_filter(&["<thi", "nk>reas", "oning</th", "ink>answer"]);
        assert_eq!(collect_thinking(&all), "reasoning");
        assert_eq!(collect_text(&all), "answer");
    }

    #[test]
    fn passthrough_non_text_chunks() {
        let mut filter = StreamTagFilter::new();
        let chunks = filter.process(StreamChunk::ThinkingDelta("native thinking".into()));
        assert_eq!(chunks.len(), 1);
        assert!(matches!(&chunks[0], StreamChunk::ThinkingDelta(t) if t == "native thinking"));
    }

    #[test]
    fn multibyte_emoji_no_panic() {
        let all = run_filter(&[" 🐻** is"]);
        assert_eq!(collect_text(&all), " 🐻** is");
    }

    #[test]
    fn multibyte_in_think_block() {
        let all = run_filter(&["<think>思考中🤔</think>答え"]);
        assert_eq!(collect_thinking(&all), "思考中🤔");
        assert_eq!(collect_text(&all), "答え");
    }

    #[test]
    fn multiple_think_blocks() {
        let all = run_filter(&["intro<think>thought1</think>middle<think>thought2</think>end"]);
        assert_eq!(collect_thinking(&all), "thought1thought2");
        assert_eq!(collect_text(&all), "intromiddleend");
    }

    // ── Stale close tag tests (issue #191) ───────────────────

    #[test]
    fn stale_close_tag_stripped() {
        let all = run_filter(&["</think>\n\nHere is the answer"]);
        assert_eq!(collect_text(&all), "\n\nHere is the answer");
        assert!(collect_thinking(&all).is_empty());
    }

    #[test]
    fn stale_close_tag_across_chunks() {
        let all = run_filter(&["</thi", "nk>answer"]);
        assert_eq!(collect_text(&all), "answer");
    }

    #[test]
    fn stale_close_then_new_think_block() {
        let all = run_filter(&["</think>prefix<think>reasoning</think>answer"]);
        assert_eq!(collect_text(&all), "prefixanswer");
        assert_eq!(collect_thinking(&all), "reasoning");
    }

    #[test]
    fn text_before_stale_close() {
        let all = run_filter(&["oops </think>real answer"]);
        assert_eq!(collect_text(&all), "oops real answer");
    }

    // ── Extended thinking tags (issue #214) ──────────────────

    #[test]
    fn thinking_tag_converted() {
        let all = run_filter(&["<thinking>deep thought</thinking>result"]);
        assert_eq!(collect_thinking(&all), "deep thought");
        assert_eq!(collect_text(&all), "result");
    }

    #[test]
    fn reasoning_tag_converted() {
        let all = run_filter(&["<reasoning>step by step</reasoning>answer"]);
        assert_eq!(collect_thinking(&all), "step by step");
        assert_eq!(collect_text(&all), "answer");
    }

    #[test]
    fn reflection_tag_converted() {
        let all = run_filter(&["<reflection>hmm let me reconsider</reflection>better answer"]);
        assert_eq!(collect_thinking(&all), "hmm let me reconsider");
        assert_eq!(collect_text(&all), "better answer");
    }

    #[test]
    fn thinking_tag_across_chunks() {
        let all = run_filter(&["<thin", "king>deep", " thought</thi", "nking>done"]);
        assert_eq!(collect_thinking(&all), "deep thought");
        assert_eq!(collect_text(&all), "done");
    }

    #[test]
    fn stale_thinking_close_stripped() {
        let all = run_filter(&["</thinking>clean output"]);
        assert_eq!(collect_text(&all), "clean output");
    }

    // ── Tool call tag suppression (issue #214) ──────────────

    #[test]
    fn tool_call_tag_suppressed() {
        let all = run_filter(&[
            "Let me check. <tool_call>{\"name\": \"List\", \"args\": {}}</tool_call>Here are the files.",
        ]);
        assert_eq!(collect_text(&all), "Let me check. Here are the files.");
        assert!(collect_thinking(&all).is_empty());
    }

    #[test]
    fn function_call_tag_suppressed() {
        let all = run_filter(&[
            "<function_call>read_file(\"main.rs\")</function_call>The file contains...",
        ]);
        assert_eq!(collect_text(&all), "The file contains...");
    }

    #[test]
    fn tool_use_tag_suppressed() {
        let all = run_filter(&["<tool_use>grep(\"TODO\")</tool_use>Found 3 matches."]);
        assert_eq!(collect_text(&all), "Found 3 matches.");
    }

    #[test]
    fn tool_call_across_chunks() {
        let all = run_filter(&[
            "checking <tool_",
            "call>{\"name\": \"Bash\"}</to",
            "ol_call>done",
        ]);
        assert_eq!(collect_text(&all), "checking done");
    }

    #[test]
    fn stale_tool_call_close_stripped() {
        let all = run_filter(&["</tool_call>actual response"]);
        assert_eq!(collect_text(&all), "actual response");
    }

    // ── Special token stripping (issue #214) ────────────────

    #[test]
    fn im_start_end_stripped() {
        let all = run_filter(&["<|im_start|>assistant\nHello<|im_end|>"]);
        assert_eq!(collect_text(&all), "assistant\nHello");
    }

    #[test]
    fn endoftext_stripped() {
        let all = run_filter(&["The answer is 42.<|endoftext|>"]);
        assert_eq!(collect_text(&all), "The answer is 42.");
    }

    #[test]
    fn eot_id_stripped() {
        let all = run_filter(&["Done!<|eot_id|>"]);
        assert_eq!(collect_text(&all), "Done!");
    }

    #[test]
    fn special_token_across_chunks() {
        let all = run_filter(&["Hello<|endo", "ftext|>world"]);
        assert_eq!(collect_text(&all), "Helloworld");
    }

    #[test]
    fn multiple_special_tokens() {
        let all = run_filter(&["<|im_start|>assistant\n<|im_end|>Hello<|endoftext|>"]);
        assert_eq!(collect_text(&all), "assistant\nHello");
    }

    // ── Mixed scenarios ─────────────────────────────────────

    #[test]
    fn think_then_tool_call() {
        let all = run_filter(&[
            "<think>Let me reason</think>I'll check.<tool_call>List</tool_call>Here are files.",
        ]);
        assert_eq!(collect_thinking(&all), "Let me reason");
        assert_eq!(collect_text(&all), "I'll check.Here are files.");
    }

    #[test]
    fn special_token_inside_think_block() {
        // Special tokens inside a think block should be emitted as thinking
        // (we don't strip inside blocks — the block handler owns that content)
        let all = run_filter(&["<think>thinking<|im_end|>more</think>answer"]);
        assert_eq!(collect_thinking(&all), "thinking<|im_end|>more");
        assert_eq!(collect_text(&all), "answer");
    }

    #[test]
    fn thinking_tag_longer_match_wins() {
        // <thinking> should match before <think> because it's listed first
        // and found at the same position.
        let all = run_filter(&["<thinking>long form</thinking>done"]);
        assert_eq!(collect_thinking(&all), "long form");
        assert_eq!(collect_text(&all), "done");
    }

    #[test]
    fn empty_blocks_produce_no_chunks() {
        let all = run_filter(&["before<think></think>after"]);
        assert_eq!(collect_text(&all), "beforeafter");
        assert!(collect_thinking(&all).is_empty());
    }

    #[test]
    fn empty_tool_call_block() {
        let all = run_filter(&["before<tool_call></tool_call>after"]);
        assert_eq!(collect_text(&all), "beforeafter");
    }
}
