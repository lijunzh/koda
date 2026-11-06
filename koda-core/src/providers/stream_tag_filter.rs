//! Streaming tag filter for LLM responses.
//!
//! Models using OpenAI-compatible endpoints (DeepSeek, Qwen, Llama, etc.)
//! may emit XML tags and special tokens directly in their text output
//! instead of using structured API fields. This filter intercepts them
//! at the **provider layer**:
//!
//! - **Thinking tags** (`<think>`, `<thinking>`, `<reasoning>`, `<reflection>`)
//!   → converted to `ThinkingDelta` chunks, but only where a model puts its
//!   reasoning: a block at the very start of the reply, or a lone close tag
//!   whose open tag was in the prompt. Anywhere else, and in any reply whose
//!   server sends reasoning in its own field, they are text: an answer that
//!   mentions `<think>` must not vanish into reasoning.
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
    "<|begin_of_box|>",
    "<|end_of_box|>",
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
/// `<|begin_of_box|>` = 17 bytes (the longest).
const MAX_TAG_LEN: usize = 17;

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
    StaleClose {
        pos: usize,
        close_len: usize,
        thinking: bool,
    },
    /// A special token was found: emit text before it, skip the token.
    SpecialToken { pos: usize, token_len: usize },
    /// Nothing found.
    None,
}

/// Which thinking tags still count as reasoning, rather than as text.
#[derive(Debug, Clone, Copy)]
struct ThinkTags {
    /// An open tag starts reasoning: nothing but whitespace before it.
    open: bool,
    /// A lone close tag ends reasoning whose open tag was in the prompt.
    stale_close: bool,
}

/// Scan `buffer` for the earliest open tag, stale close tag, or special token.
///
/// Also says whether a thinking open tag in `buffer` is text (it comes after
/// other text) and precedes any stale close tag: after one, the model is
/// writing about the tags, so a later close tag is text too.
fn find_earliest_match(buffer: &str, think: ThinkTags) -> (EarliestMatch, bool) {
    let mut best_pos = usize::MAX;
    let mut best = EarliestMatch::None;
    let mut literal_open = usize::MAX;
    let mut stale_think = usize::MAX;

    // Check open tags
    for (i, pair) in TAG_PAIRS.iter().enumerate() {
        let Some(pos) = buffer.find(pair.open) else {
            continue;
        };
        if pair.action == TagAction::Thinking && !(think.open && buffer[..pos].trim().is_empty()) {
            literal_open = literal_open.min(pos);
            continue;
        }
        if pos < best_pos {
            best_pos = pos;
            best = EarliestMatch::OpenTag { pos, pair_idx: i };
        }
    }

    // Check stale close tags (only if they appear before any open tag)
    for pair in TAG_PAIRS {
        if pair.action == TagAction::Thinking && !think.stale_close {
            continue;
        }
        if pair.action == TagAction::Thinking
            && let Some(pos) = buffer.find(pair.close)
            && pos < literal_open
        {
            stale_think = stale_think.min(pos);
        }
        if let Some(pos) = buffer.find(pair.close)
            && pos < best_pos
            && (pair.action != TagAction::Thinking || pos < literal_open)
        {
            best_pos = pos;
            best = EarliestMatch::StaleClose {
                pos,
                close_len: pair.close.len(),
                thinking: pair.action == TagAction::Thinking,
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

    (best, literal_open < stale_think)
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
    /// Which thinking tags still count as reasoning.
    think: ThinkTags,
}

impl StreamTagFilter {
    /// Create a new filter with empty state.
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            state: FilterState::Normal,
            warned_suppressed: false,
            think: ThinkTags {
                open: true,
                stale_close: true,
            },
        }
    }

    /// The server sends this reply's reasoning in its own field, so thinking
    /// tags in the text are the model writing about them.
    pub fn saw_native_reasoning(&mut self) {
        self.think.open = false;
        self.think.stale_close = false;
    }

    /// Queue `text` for output; once it holds more than whitespace, a
    /// thinking open tag is no longer at the reply's start.
    fn emit_text(&mut self, output: &mut Vec<StreamChunk>, text: String) {
        if text.is_empty() {
            return;
        }
        if !text.trim().is_empty() {
            self.think.open = false;
        }
        output.push(StreamChunk::TextDelta(text));
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
                        if action == TagAction::Thinking {
                            // A reply reasons once, at its start.
                            self.think.open = false;
                            self.think.stale_close = false;
                        }

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
                    let (found, literal_open_first) = find_earliest_match(&self.buffer, self.think);
                    if literal_open_first {
                        self.think.stale_close = false;
                    }
                    match found {
                        EarliestMatch::OpenTag { pos, pair_idx } => {
                            let pair = &TAG_PAIRS[pair_idx];
                            let before = self.buffer[..pos].to_string();
                            self.buffer = self.buffer[pos + pair.open.len()..].to_string();
                            self.state = FilterState::InBlock {
                                close_tag: pair.close,
                                action: pair.action,
                            };
                            self.emit_text(&mut output, before);
                            continue;
                        }
                        EarliestMatch::StaleClose {
                            pos,
                            close_len,
                            thinking,
                        } => {
                            let before = self.buffer[..pos].to_string();
                            self.buffer = self.buffer[pos + close_len..].to_string();
                            if thinking {
                                self.think.open = false;
                                self.think.stale_close = false;
                            }
                            self.emit_text(&mut output, before);
                            continue;
                        }
                        EarliestMatch::SpecialToken { pos, token_len } => {
                            let before = self.buffer[..pos].to_string();
                            self.buffer = self.buffer[pos + token_len..].to_string();
                            self.emit_text(&mut output, before);
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
                                self.emit_text(&mut output, safe);
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
    fn think_block_after_text_is_text() {
        // A reply reasons at its start; a tag after other text is the model
        // writing about tags, and hiding it would hide the answer.
        let all = run_filter(&["intro<think>thought1</think>middle"]);
        assert!(collect_thinking(&all).is_empty());
        assert_eq!(collect_text(&all), "intro<think>thought1</think>middle");
    }

    #[test]
    fn second_think_block_is_text() {
        let all = run_filter(&["<think>thought1</think>middle<think>thought2</think>end"]);
        assert_eq!(collect_thinking(&all), "thought1");
        assert_eq!(collect_text(&all), "middle<think>thought2</think>end");
    }

    #[test]
    fn whitespace_before_think_block() {
        let all = run_filter(&["\n\n<think>reasoning</think>answer"]);
        assert_eq!(collect_thinking(&all), "reasoning");
        assert_eq!(collect_text(&all), "\n\nanswer");
    }

    #[test]
    fn answer_mentioning_think_tags_is_kept() {
        // The think-tag-multibyte eval: the model explains the filter's own tags.
        // (A lone close tag before any open tag can't be told from reasoning
        // whose open tag was in the prompt, so without native reasoning it is
        // stripped once; see `only_the_first_stale_close_is_stripped`.)
        let answer = "A partial \"<think>\" across chunks is held back, so \
                      `find(\"</think>\")` sees the whole tag \u{2014} no slicing inside \u{1F43B}.";
        let all = run_filter(&["Fixed. ", answer]);
        assert!(collect_thinking(&all).is_empty());
        assert_eq!(collect_text(&all), format!("Fixed. {answer}"));
    }

    #[test]
    fn think_tag_mid_answer_across_chunks() {
        let all = run_filter(&["The tag <thi", "nk> opens a block, </thi", "nk> ends it."]);
        assert!(collect_thinking(&all).is_empty());
        assert_eq!(
            collect_text(&all),
            "The tag <think> opens a block, </think> ends it."
        );
    }

    #[test]
    fn native_reasoning_turns_think_tags_into_text() {
        let mut filter = StreamTagFilter::new();
        filter.saw_native_reasoning();
        let mut all = filter.process(StreamChunk::TextDelta(
            "<think>x</think> and </think>".into(),
        ));
        all.extend(filter.flush());
        assert!(collect_thinking(&all).is_empty());
        assert_eq!(collect_text(&all), "<think>x</think> and </think>");
    }

    #[test]
    fn native_reasoning_keeps_other_filtering() {
        let mut filter = StreamTagFilter::new();
        filter.saw_native_reasoning();
        let mut all = filter.process(StreamChunk::TextDelta("Done!<|im_end|>".into()));
        all.extend(filter.flush());
        assert_eq!(collect_text(&all), "Done!");
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
    fn stale_close_then_think_tags_are_text() {
        // Reasoning ended at the stale close; later tags are text.
        let all = run_filter(&["</think>prefix<think>reasoning</think>answer"]);
        assert_eq!(collect_text(&all), "prefix<think>reasoning</think>answer");
        assert!(collect_thinking(&all).is_empty());
    }

    #[test]
    fn only_the_first_stale_close_is_stripped() {
        let all = run_filter(&["reasoning</think>answer mentions </think> too"]);
        assert_eq!(collect_text(&all), "reasoninganswer mentions </think> too");
    }

    #[test]
    fn stale_close_after_a_special_token_still_stripped() {
        let all = run_filter(&["<|im_start|>reasoning</think>answer about <think>"]);
        assert_eq!(collect_text(&all), "reasoninganswer about <think>");
        assert!(collect_thinking(&all).is_empty());
    }

    #[test]
    fn close_tag_after_a_literal_open_tag_is_text() {
        let all = run_filter(&["Write <think> and then </think>."]);
        assert_eq!(collect_text(&all), "Write <think> and then </think>.");
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
        let all = run_filter(&["<think></think>after"]);
        assert_eq!(collect_text(&all), "after");
        assert!(collect_thinking(&all).is_empty());
    }

    #[test]
    fn empty_tool_call_block() {
        let all = run_filter(&["before<tool_call></tool_call>after"]);
        assert_eq!(collect_text(&all), "beforeafter");
    }

    // ── Box token stripping (issue #550) ────────────────────

    #[test]
    fn begin_of_box_stripped() {
        let all = run_filter(&["Hello<|begin_of_box|>world"]);
        assert_eq!(collect_text(&all), "Helloworld");
    }

    #[test]
    fn end_of_box_stripped() {
        let all = run_filter(&["Hello<|end_of_box|>world"]);
        assert_eq!(collect_text(&all), "Helloworld");
    }

    #[test]
    fn both_box_tokens_stripped() {
        let all = run_filter(&["<|begin_of_box|>content here<|end_of_box|>"]);
        assert_eq!(collect_text(&all), "content here");
    }

    #[test]
    fn box_token_across_chunks() {
        let all = run_filter(&["Hello<|begin_", "of_box|>world"]);
        assert_eq!(collect_text(&all), "Helloworld");
    }

    #[test]
    fn multiple_box_tokens_in_response() {
        let all = run_filter(&[
            "<|begin_of_box|>first<|end_of_box|> middle <|begin_of_box|>second<|end_of_box|>",
        ]);
        assert_eq!(collect_text(&all), "first middle second");
    }
}
