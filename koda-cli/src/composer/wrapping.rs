// Foundation work for #1116/#1175 — see the equivalent header note in
// composer/key_hint.rs for the full rationale. The blanket allow goes away
// when PR 2 wires up the consumers.
#![allow(dead_code)]

//! Reduced port of codex's `codex-rs/tui/src/wrapping.rs`.
//!
//! ## Provenance
//!
//! Selected helpers ported from `codex-rs/tui/src/wrapping.rs` at upstream
//! commit `d55479488e125ef7a0a8584505d839a22eaf6204` (codex `main` as of
//! 2026-05-01).
//!
//! Verified byte-identical between this SHA and our previous reference
//! `7e8594fc198615068018b198ab86a9ae0a541dff` (`git diff` returns 0
//! lines), so the re-stamp is a pure attribution change.
//!
//! Original work: Copyright (c) OpenAI / codex contributors,
//! licensed under the Apache License, Version 2.0.
//! See `LICENSES/codex-APACHE-2.0` for the full license text.
//!
//! ## What was kept vs dropped
//!
//! - **Kept:** [`wrap_ranges`] and its helper [`map_owned_wrapped_line_to_range`].
//!   These are the only wrapping primitives [`super::textarea::TextArea`]
//!   needs to drive its cursor-position math.
//! - **Dropped:** all the URL-aware adaptive wrapping (~1,300 LoC of
//!   codex's `wrapping.rs`). koda's history-render path uses different
//!   wrapping logic (`koda-cli/src/history_render.rs`) and the URL
//!   helpers there are not needed inside the composer's textarea
//!   (which renders user-typed input, not agent output). If we later
//!   want URL-aware wrapping in the composer, we can port more.
//! - **Dropped:** `wrap_ranges_trim` (used by codex but not by the
//!   textarea sub-module; would be dead code here).

use std::ops::Range;
use textwrap::Options;

/// Returns byte-ranges into `text` for each wrapped line, including
/// trailing whitespace and a +1 sentinel byte. Used by the textarea
/// cursor-position logic.
///
/// The sentinel byte is critical: without it, a cursor at the END of a
/// wrapped line wouldn't have a valid byte position to point at, and
/// the textarea couldn't distinguish "cursor at end of line N" from
/// "cursor at start of line N+1" in the soft-wrapped representation.
pub(crate) fn wrap_ranges<'a, O>(text: &str, width_or_options: O) -> Vec<Range<usize>>
where
    O: Into<Options<'a>>,
{
    let opts = width_or_options.into();
    let mut lines: Vec<Range<usize>> = Vec::new();
    let mut cursor = 0usize;
    for (line_index, line) in textwrap::wrap(text, &opts).iter().enumerate() {
        match line {
            std::borrow::Cow::Borrowed(slice) => {
                // SAFETY: `slice` is a sub-slice of `text` (Cow::Borrowed
                // implies same allocation), so the offset_from is well-defined.
                let start = unsafe { slice.as_ptr().offset_from(text.as_ptr()) as usize };
                let end = start + slice.len();
                let trailing_spaces = text[end..].chars().take_while(|c| *c == ' ').count();
                lines.push(start..end + trailing_spaces + 1);
                cursor = end + trailing_spaces;
            }
            std::borrow::Cow::Owned(slice) => {
                let synthetic_prefix = if line_index == 0 {
                    opts.initial_indent
                } else {
                    opts.subsequent_indent
                };
                let mapped = map_owned_wrapped_line_to_range(text, cursor, slice, synthetic_prefix);
                let trailing_spaces = text[mapped.end..].chars().take_while(|c| *c == ' ').count();
                lines.push(mapped.start..mapped.end + trailing_spaces + 1);
                cursor = mapped.end + trailing_spaces;
            }
        }
    }
    lines
}

/// Maps an owned (materialized) wrapped line back to a byte range in `text`.
///
/// `textwrap` returns `Cow::Owned` when it inserts a hyphenation penalty
/// character (typically `-`) that does not exist in the source. This
/// function walks the owned string character-by-character against the
/// source, skipping trailing penalty chars, and returns the
/// corresponding source byte range starting from `cursor`.
fn map_owned_wrapped_line_to_range(
    text: &str,
    cursor: usize,
    wrapped: &str,
    synthetic_prefix: &str,
) -> Range<usize> {
    let wrapped = if synthetic_prefix.is_empty() {
        wrapped
    } else {
        wrapped.strip_prefix(synthetic_prefix).unwrap_or(wrapped)
    };

    let mut start = cursor;
    while start < text.len() && !wrapped.starts_with(' ') {
        let Some(ch) = text[start..].chars().next() else {
            break;
        };
        if ch != ' ' {
            break;
        }
        start += ch.len_utf8();
    }

    let mut end = start;
    let mut saw_source_char = false;
    let mut chars = wrapped.chars().peekable();
    while let Some(ch) = chars.next() {
        if end < text.len() {
            let Some(src) = text[end..].chars().next() else {
                unreachable!("checked end < text.len()");
            };
            if ch == src {
                end += src.len_utf8();
                saw_source_char = true;
                continue;
            }
        }

        // textwrap can materialize owned lines when penalties are inserted.
        // The default penalty is a trailing '-'; it does not correspond to
        // source bytes, so we skip it while keeping byte ranges in source text.
        if ch == '-' && chars.peek().is_none() {
            continue;
        }

        // Non-source chars can be synthesized by textwrap in owned output
        // (e.g. non-space indent prefixes). Keep going and map the source bytes
        // we can confidently match instead of crashing the app.
        if !saw_source_char {
            continue;
        }

        tracing::warn!(
            wrapped = %wrapped,
            cursor,
            end,
            "wrap_ranges: could not fully map owned line; returning partial source range"
        );
        break;
    }

    start..end
}
