//! `<think>` tag filter for streaming LLM responses.
//!
//! Some models (DeepSeek-R1, Qwen QwQ) embed reasoning inside `<think>...</think>`
//! XML tags in their regular text output. This filter detects these tags in the
//! streaming token stream and converts them to proper `ThinkingDelta` chunks.
//!
//! This runs at the **provider layer** so the inference engine only sees typed
//! `StreamChunk` variants — no string parsing needed upstream.

use crate::providers::StreamChunk;

/// Find the largest byte index ≤ `index` that is a valid char boundary.
/// Equivalent to `str::floor_char_boundary` (stabilized in Rust 1.82+).
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

/// A streaming filter that converts `<think>` tags into `ThinkingDelta` chunks.
///
/// Feed it `StreamChunk::TextDelta` chunks and it emits a transformed stream
/// where content inside `<think>...</think>` becomes `ThinkingDelta` instead.
pub struct ThinkTagFilter {
    buffer: String,
    in_think_block: bool,
}

impl ThinkTagFilter {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            in_think_block: false,
        }
    }

    /// Process a stream chunk. Returns zero or more output chunks.
    ///
    /// Most of the time this returns a single chunk, but when a `<think>` or
    /// `</think>` tag spans a chunk boundary, it may return multiple chunks
    /// or hold content until the next call.
    pub fn process(&mut self, chunk: StreamChunk) -> Vec<StreamChunk> {
        match chunk {
            StreamChunk::TextDelta(delta) => self.process_text(&delta),
            // Pass everything else through unchanged
            other => vec![other],
        }
    }

    /// Flush any remaining buffered content (call when stream ends).
    pub fn flush(&mut self) -> Vec<StreamChunk> {
        if self.buffer.is_empty() {
            return vec![];
        }
        let remaining = std::mem::take(&mut self.buffer);
        if self.in_think_block {
            // Unclosed <think> block — emit as thinking
            vec![StreamChunk::ThinkingDelta(remaining)]
        } else {
            vec![StreamChunk::TextDelta(remaining)]
        }
    }

    fn process_text(&mut self, delta: &str) -> Vec<StreamChunk> {
        self.buffer.push_str(delta);
        let mut output = Vec::new();

        loop {
            if self.in_think_block {
                // Looking for </think>
                if let Some(end_pos) = self.buffer.find("</think>") {
                    let thinking = self.buffer[..end_pos].to_string();
                    self.buffer = self.buffer[end_pos + 8..].to_string();
                    self.in_think_block = false;
                    if !thinking.is_empty() {
                        output.push(StreamChunk::ThinkingDelta(thinking));
                    }
                    continue; // process remaining buffer
                } else {
                    // Still accumulating thinking content.
                    // Emit what we have so far as thinking (for progressive display)
                    // but keep the last 8 chars in case "</think>" spans chunks.
                    let safe_len =
                        floor_char_boundary(&self.buffer, self.buffer.len().saturating_sub(8));
                    if safe_len > 0 {
                        let safe = self.buffer[..safe_len].to_string();
                        self.buffer = self.buffer[safe_len..].to_string();
                        output.push(StreamChunk::ThinkingDelta(safe));
                    }
                    break;
                }
            } else {
                // Strip stale </think> closing tags left over from a previous
                // turn. This happens when the model sends `<think>...` + tool
                // calls in turn N, then starts turn N+1 with `</think>answer`.
                // The filter is re-created per turn, so `in_think_block` is false
                // and the bare `</think>` would leak into visible output.
                if let Some(pos) = self.buffer.find("</think>") {
                    // Only strip if it appears before any <think> open tag
                    // (otherwise the <think> handler below should run first).
                    let open_pos = self.buffer.find("<think>");
                    if open_pos.is_none() || pos < open_pos.unwrap() {
                        let before = self.buffer[..pos].to_string();
                        self.buffer = self.buffer[pos + 8..].to_string();
                        if !before.is_empty() {
                            output.push(StreamChunk::TextDelta(before));
                        }
                        continue;
                    }
                }

                // Looking for <think>
                if let Some(start_pos) = self.buffer.find("<think>") {
                    let before = self.buffer[..start_pos].to_string();
                    self.buffer = self.buffer[start_pos + 7..].to_string();
                    self.in_think_block = true;
                    if !before.is_empty() {
                        output.push(StreamChunk::TextDelta(before));
                    }
                    continue; // process remaining buffer
                } else {
                    // No <think> tag found. Emit safe content, keeping
                    // the last 7 chars in case "<think>" spans chunks.
                    // Also keep 8 chars for "</think>" spanning chunks.
                    let hold_back = 8; // max(len("<think>"), len("</think>"))
                    let safe_len = floor_char_boundary(
                        &self.buffer,
                        self.buffer.len().saturating_sub(hold_back),
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

        output
    }
}

impl Default for ThinkTagFilter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_think_tags() {
        let mut filter = ThinkTagFilter::new();
        let chunks = filter.process(StreamChunk::TextDelta("Hello world".into()));
        let flushed = filter.flush();
        let all: Vec<_> = chunks.into_iter().chain(flushed).collect();
        let text: String = all
            .iter()
            .filter_map(|c| match c {
                StreamChunk::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello world");
    }

    #[test]
    fn test_think_block_single_chunk() {
        let mut filter = ThinkTagFilter::new();
        let chunks = filter.process(StreamChunk::TextDelta(
            "<think>reasoning here</think>answer".into(),
        ));
        let flushed = filter.flush();
        let all: Vec<_> = chunks.into_iter().chain(flushed).collect();

        let thinking: Vec<&str> = all
            .iter()
            .filter_map(|c| match c {
                StreamChunk::ThinkingDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        let text: Vec<&str> = all
            .iter()
            .filter_map(|c| match c {
                StreamChunk::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();

        assert_eq!(thinking.join(""), "reasoning here");
        assert_eq!(text.join(""), "answer");
    }

    #[test]
    fn test_think_block_across_chunks() {
        let mut filter = ThinkTagFilter::new();
        let mut all = Vec::new();
        all.extend(filter.process(StreamChunk::TextDelta("<thi".into())));
        all.extend(filter.process(StreamChunk::TextDelta("nk>reas".into())));
        all.extend(filter.process(StreamChunk::TextDelta("oning</th".into())));
        all.extend(filter.process(StreamChunk::TextDelta("ink>answer".into())));
        all.extend(filter.flush());

        let thinking: String = all
            .iter()
            .filter_map(|c| match c {
                StreamChunk::ThinkingDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        let text: String = all
            .iter()
            .filter_map(|c| match c {
                StreamChunk::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();

        assert_eq!(thinking, "reasoning");
        assert_eq!(text, "answer");
    }

    #[test]
    fn test_passthrough_non_text_chunks() {
        let mut filter = ThinkTagFilter::new();
        let chunks = filter.process(StreamChunk::ThinkingDelta("native thinking".into()));
        assert_eq!(chunks.len(), 1);
        assert!(matches!(&chunks[0], StreamChunk::ThinkingDelta(t) if t == "native thinking"));
    }

    #[test]
    fn test_multibyte_emoji_no_panic() {
        // Regression test for #30: slicing inside multi-byte char (🐻 = 4 bytes)
        let mut filter = ThinkTagFilter::new();
        let mut all = Vec::new();
        // Buffer " 🐻** is" — 10 bytes, saturating_sub(7) = 3, which is inside 🐻
        all.extend(filter.process(StreamChunk::TextDelta(" 🐻** is".into())));
        all.extend(filter.flush());

        let text: String = all
            .iter()
            .filter_map(|c| match c {
                StreamChunk::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, " 🐻** is");
    }

    #[test]
    fn test_multibyte_in_think_block() {
        let mut filter = ThinkTagFilter::new();
        let mut all = Vec::new();
        all.extend(filter.process(StreamChunk::TextDelta("<think>思考中🤔</think>答え".into())));
        all.extend(filter.flush());

        let thinking: String = all
            .iter()
            .filter_map(|c| match c {
                StreamChunk::ThinkingDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        let text: String = all
            .iter()
            .filter_map(|c| match c {
                StreamChunk::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(thinking, "思考中🤔");
        assert_eq!(text, "答え");
    }

    #[test]
    fn test_multiple_think_blocks() {
        let mut filter = ThinkTagFilter::new();
        let mut all = Vec::new();
        all.extend(filter.process(StreamChunk::TextDelta(
            "intro<think>thought1</think>middle<think>thought2</think>end".into(),
        )));
        all.extend(filter.flush());

        let thinking: String = all
            .iter()
            .filter_map(|c| match c {
                StreamChunk::ThinkingDelta(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        let text: String = all
            .iter()
            .filter_map(|c| match c {
                StreamChunk::TextDelta(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");

        assert_eq!(thinking, "thought1thought2");
        assert_eq!(text, "intromiddleend");
    }

    // ── Stale </think> tag tests (issue #191) ──────────────────

    #[test]
    fn test_stale_close_tag_stripped() {
        // Simulates turn N+1 starting with </think> from turn N's unclosed block.
        let mut filter = ThinkTagFilter::new();
        let mut all = Vec::new();
        all.extend(filter.process(StreamChunk::TextDelta(
            "</think>\n\nHere is the answer".into(),
        )));
        all.extend(filter.flush());

        let text: String = all
            .iter()
            .filter_map(|c| match c {
                StreamChunk::TextDelta(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        let thinking: String = all
            .iter()
            .filter_map(|c| match c {
                StreamChunk::ThinkingDelta(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");

        assert_eq!(text, "\n\nHere is the answer");
        assert!(thinking.is_empty());
    }

    #[test]
    fn test_stale_close_tag_across_chunks() {
        // </think> split across chunk boundary at the start of a response.
        let mut filter = ThinkTagFilter::new();
        let mut all = Vec::new();
        all.extend(filter.process(StreamChunk::TextDelta("</thi".into())));
        all.extend(filter.process(StreamChunk::TextDelta("nk>answer".into())));
        all.extend(filter.flush());

        let text: String = all
            .iter()
            .filter_map(|c| match c {
                StreamChunk::TextDelta(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");

        assert_eq!(text, "answer");
    }

    #[test]
    fn test_stale_close_then_new_think_block() {
        // Stale </think> followed by a fresh <think>...</think>.
        let mut filter = ThinkTagFilter::new();
        let mut all = Vec::new();
        all.extend(filter.process(StreamChunk::TextDelta(
            "</think>prefix<think>reasoning</think>answer".into(),
        )));
        all.extend(filter.flush());

        let text: String = all
            .iter()
            .filter_map(|c| match c {
                StreamChunk::TextDelta(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        let thinking: String = all
            .iter()
            .filter_map(|c| match c {
                StreamChunk::ThinkingDelta(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");

        assert_eq!(text, "prefixanswer");
        assert_eq!(thinking, "reasoning");
    }

    #[test]
    fn test_text_before_stale_close() {
        // Some models emit a bit of text before the stale </think>.
        let mut filter = ThinkTagFilter::new();
        let mut all = Vec::new();
        all.extend(filter.process(StreamChunk::TextDelta("oops </think>real answer".into())));
        all.extend(filter.flush());

        let text: String = all
            .iter()
            .filter_map(|c| match c {
                StreamChunk::TextDelta(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");

        assert_eq!(text, "oops real answer");
    }
}
