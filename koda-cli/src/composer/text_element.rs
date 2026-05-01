//! Reduced port of codex's `codex-rs/protocol/src/user_input.rs`.
//!
//! ## Provenance
//!
//! Selected types ported from `codex-rs/protocol/src/user_input.rs` at
//! upstream commit `7e8594fc198615068018b198ab86a9ae0a541dff`.
//!
//! Original work: Copyright (c) OpenAI / codex contributors,
//! licensed under the Apache License, Version 2.0.
//! See `LICENSES/codex-APACHE-2.0` (vendored at the workspace root) for
//! the full license text.
//!
//! ## What was kept vs dropped
//!
//! - **Kept:** [`ByteRange`], [`TextElement`], [`MAX_USER_INPUT_TEXT_CHARS`].
//!   These are the primitives the ported [`super::textarea::TextArea`] needs
//!   to track non-text spans (e.g. image placeholders) embedded in the
//!   composer buffer.
//! - **Dropped:** the `UserInput` enum and its protocol-serialization
//!   derive (`schemars::JsonSchema`, `serde::{Deserialize, Serialize}`,
//!   `ts_rs::TS`). koda's protocol layer is separate from codex's; we
//!   only need the in-memory composer types here. Adding those derives
//!   would drag four crates into `koda-cli` for no current benefit.
//!
//! ## Adaptations
//!
//! - Removed all `#[derive(Deserialize, Serialize, TS, JsonSchema)]`
//!   from the ported types.
//! - Removed the `UserInput` enum entirely (not used by textarea).

use std::ops::Range;

/// Conservative cap so one user message cannot monopolize a large context window.
///
/// Ported verbatim from codex; unchanged for koda since the rationale
/// (model context-window protection) is identical.
pub const MAX_USER_INPUT_TEXT_CHARS: usize = 1 << 20;

/// A non-text span inside the composer buffer.
///
/// Used by [`super::textarea::TextArea`] to track placeholders such as
/// pasted image references that should be displayed/persisted as a
/// single logical unit even though they're embedded in the UTF-8 text
/// stream. Cursor motion across an element treats it as one grapheme;
/// deletion removes the whole span atomically.
#[derive(Debug, Clone, PartialEq)]
pub struct TextElement {
    /// Byte range in the parent `text` buffer that this element occupies.
    pub byte_range: ByteRange,
    /// Optional human-readable placeholder for the element, displayed in the UI.
    placeholder: Option<String>,
}

impl TextElement {
    pub fn new(byte_range: ByteRange, placeholder: Option<String>) -> Self {
        Self {
            byte_range,
            placeholder,
        }
    }

    /// Returns a copy of this element with a remapped byte range.
    ///
    /// The placeholder is preserved as-is; callers must ensure the new range
    /// still refers to the same logical element (and same placeholder)
    /// within the new text.
    pub fn map_range<F>(&self, map: F) -> Self
    where
        F: FnOnce(ByteRange) -> ByteRange,
    {
        Self {
            byte_range: map(self.byte_range),
            placeholder: self.placeholder.clone(),
        }
    }

    pub fn set_placeholder(&mut self, placeholder: Option<String>) {
        self.placeholder = placeholder;
    }

    /// Returns the placeholder string for this element, falling back to
    /// the underlying text in the buffer if no explicit placeholder was
    /// set. Used by the textarea when rendering the element in the
    /// composer.
    pub fn placeholder<'a>(&'a self, text: &'a str) -> Option<&'a str> {
        self.placeholder
            .as_deref()
            .or_else(|| text.get(self.byte_range.start..self.byte_range.end))
    }
}

/// A byte range within a UTF-8 text buffer.
///
/// `start` is inclusive, `end` is exclusive. The range is expected to
/// align to UTF-8 character boundaries (the textarea ensures this on
/// insertion).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    /// Start byte offset (inclusive) within the UTF-8 text buffer.
    pub start: usize,
    /// End byte offset (exclusive) within the UTF-8 text buffer.
    pub end: usize,
}

impl From<Range<usize>> for ByteRange {
    fn from(range: Range<usize>) -> Self {
        Self {
            start: range.start,
            end: range.end,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_range_from_std_range() {
        let r: ByteRange = (3..7).into();
        assert_eq!(r.start, 3);
        assert_eq!(r.end, 7);
    }

    #[test]
    fn text_element_placeholder_falls_back_to_buffer_text() {
        let elem = TextElement::new(ByteRange { start: 6, end: 11 }, None);
        let text = "Hello world";
        assert_eq!(elem.placeholder(text), Some("world"));
    }

    #[test]
    fn text_element_explicit_placeholder_takes_precedence() {
        let elem = TextElement::new(
            ByteRange { start: 6, end: 11 },
            Some("[image]".to_string()),
        );
        let text = "Hello world";
        assert_eq!(elem.placeholder(text), Some("[image]"));
    }

    #[test]
    fn text_element_map_range_preserves_placeholder() {
        let elem = TextElement::new(
            ByteRange { start: 6, end: 11 },
            Some("[img]".to_string()),
        );
        let mapped = elem.map_range(|r| ByteRange {
            start: r.start + 10,
            end: r.end + 10,
        });
        assert_eq!(mapped.byte_range.start, 16);
        assert_eq!(mapped.byte_range.end, 21);
        assert_eq!(mapped.placeholder("anything goes here..."), Some("[img]"));
    }
}
