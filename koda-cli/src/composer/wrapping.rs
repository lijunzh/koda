// Foundation work for #1116/#1175 — see the equivalent header note in
// composer/key_hint.rs for the full rationale. The blanket allow goes away
// when PR 2 wires up the consumers.

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
use textwrap::core::Word;
use textwrap::word_splitters::split_words;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Halfwidth katakana voiced and semi-voiced sound marks.
const HALFWIDTH_SOUND_MARKS: [char; 2] = ['\u{FF9E}', '\u{FF9F}'];

/// Display width as Ratatui draws it (upstream openai/codex#35962).
///
/// Unicode gives the halfwidth sound marks width 0, but Ratatui draws each
/// in its own cell; counting them keeps wrapping, the cursor and the mask
/// in step with what is on screen.
pub(crate) fn display_width(text: &str) -> usize {
    text.width() + text.matches(HALFWIDTH_SOUND_MARKS).count()
}

/// Projected text keeps source-offset lookup separate from legal grapheme
/// split points.
struct ProjectedText {
    text: String,
    source_boundaries: Vec<(usize, usize)>,
    grapheme_boundaries: Vec<usize>,
}

/// Replaces halfwidth sound-mark graphemes with equally wide, textwrap-safe
/// placeholders, since textwrap measures them as width 0.
///
/// Source boundaries recover original byte offsets, while grapheme
/// boundaries keep placeholders indivisible and preserve leading whitespace
/// as a wrapping opportunity.
fn project_halfwidth_sound_marks(text: &str) -> Option<ProjectedText> {
    if !text.contains(HALFWIDTH_SOUND_MARKS) {
        return None;
    }

    let mut projected = String::with_capacity(text.len());
    let mut source_boundaries = vec![(0, 0)];
    let mut grapheme_boundaries = vec![0];
    for (source_start, grapheme) in text.grapheme_indices(/*is_extended*/ true) {
        if grapheme.contains(HALFWIDTH_SOUND_MARKS) {
            let source_end = source_start + grapheme.len();
            let content_start = grapheme
                .find(|ch: char| !ch.is_whitespace())
                .unwrap_or(grapheme.len());
            let (whitespace, content) = grapheme.split_at(content_start);
            for (offset, ch) in whitespace.char_indices() {
                projected.push(ch);
                source_boundaries.push((projected.len(), source_start + offset + ch.len_utf8()));
                grapheme_boundaries.push(projected.len());
            }

            // Two-column and one-column placeholders joined by WORD JOINER so
            // textwrap never breaks inside one source grapheme.
            let width = display_width(content);
            let projected_start = projected.len();
            for _ in 0..width / 2 {
                if projected.len() > projected_start {
                    projected.push('\u{2060}');
                }
                projected.push('界');
                source_boundaries.push((projected.len(), source_end));
            }
            if width % 2 == 1 {
                if projected.len() > projected_start {
                    projected.push('\u{2060}');
                }
                projected.push('a');
                source_boundaries.push((projected.len(), source_end));
            }
        } else {
            for (offset, ch) in grapheme.char_indices() {
                projected.push(ch);
                source_boundaries.push((projected.len(), source_start + offset + ch.len_utf8()));
            }
        }
        grapheme_boundaries.push(projected.len());
    }

    Some(ProjectedText {
        text: projected,
        source_boundaries,
        grapheme_boundaries,
    })
}

/// Maps a projected byte offset back to the corresponding original-text
/// boundary.
fn source_offset(boundaries: &[(usize, usize)], projected_offset: usize) -> usize {
    boundaries
        .binary_search_by_key(&projected_offset, |(offset, _)| *offset)
        .map(|index| boundaries[index].1)
        .unwrap_or(projected_offset)
}

/// Splits oversized projected words without separating placeholders for one
/// source grapheme.
fn break_projected_words<'a>(
    words: impl Iterator<Item = Word<'a>>,
    projected: &'a ProjectedText,
    line_width: usize,
) -> Vec<Word<'a>> {
    let projected_start = projected.text.as_ptr() as usize;
    let mut pieces = Vec::new();

    for word in words {
        if display_width(word.word) <= line_width {
            pieces.push(word);
            continue;
        }

        let word_start = word.word.as_ptr() as usize - projected_start;
        let word_end = word_start + word.word.len();
        let mut piece_start = word_start;
        let mut piece_width = 0;
        let mut atom_start = word_start;
        let boundary_start = projected
            .grapheme_boundaries
            .partition_point(|atom_end| *atom_end <= word_start);

        for atom_end in projected
            .grapheme_boundaries
            .iter()
            .copied()
            .skip(boundary_start)
        {
            if atom_end > word_end {
                break;
            }

            let atom_width = display_width(&projected.text[atom_start..atom_end]);
            if piece_width > 0 && piece_width + atom_width > line_width {
                pieces.push(Word::from(&projected.text[piece_start..atom_start]));
                piece_start = atom_start;
                piece_width = 0;
            }
            piece_width += atom_width;
            atom_start = atom_end;
        }

        let mut last = Word::from(&projected.text[piece_start..word_end]);
        last.whitespace = word.whitespace;
        last.penalty = word.penalty;
        pieces.push(last);
    }

    pieces
}

/// Wraps projected text and translates the resulting ranges back to source
/// byte offsets, in the same shape as [`wrap_ranges`].
fn wrap_projected_ranges(projected: &ProjectedText, opts: &Options<'_>) -> Vec<Range<usize>> {
    let line_widths = [
        opts.width
            .saturating_sub(display_width(opts.initial_indent)),
        opts.width
            .saturating_sub(display_width(opts.subsequent_indent)),
    ];
    let line_ending = opts.line_ending.as_str();
    let mut ranges = Vec::new();
    let mut line_start = 0;

    for line in projected.text.split(line_ending) {
        let words = opts.word_separator.find_words(line);
        let split_words = split_words(words, &opts.word_splitter);
        let mut broken_words = if opts.break_words {
            break_projected_words(split_words, projected, line_widths[1])
        } else {
            split_words.collect()
        };
        if opts.break_words && !opts.initial_indent.is_empty() {
            broken_words.insert(0, Word::from(""));
        }

        let wrapped_words = opts.wrap_algorithm.wrap(&broken_words, &line_widths);
        let mut cursor = line_start;
        for words in wrapped_words {
            let Some(last_word) = words.last() else {
                let source = source_offset(&projected.source_boundaries, cursor);
                ranges.push(source..source + 1);
                continue;
            };
            let len = words
                .iter()
                .map(|word| word.word.len() + word.whitespace.len())
                .sum::<usize>()
                - last_word.whitespace.len();
            let end = cursor + len;
            let trailing_spaces = projected.text[end..]
                .chars()
                .take_while(|ch| *ch == ' ')
                .count();
            let source_start = source_offset(&projected.source_boundaries, cursor);
            let source_end = source_offset(&projected.source_boundaries, end + trailing_spaces);
            ranges.push(source_start..source_end + 1);
            cursor = end + last_word.whitespace.len();
        }
        line_start += line.len() + line_ending.len();
    }

    ranges
}

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
    if let Some(projected) = project_halfwidth_sound_marks(text) {
        return wrap_projected_ranges(&projected, &opts);
    }
    let mut lines: Vec<Range<usize>> = Vec::new();
    let mut cursor = 0usize;
    for (line_index, line) in textwrap::wrap(text, &opts).iter().enumerate() {
        match line {
            std::borrow::Cow::Borrowed(slice) => {
                // `Cow::Borrowed` does not guarantee a sub-slice of `text`:
                // at width 0 textwrap returns a static `""` for every empty
                // hard line. Such a line starts on the next hard line.
                let Some(start) = subslice_offset(text, slice).or_else(|| {
                    slice
                        .is_empty()
                        .then(|| next_hard_line_start(text, cursor, line_index))
                }) else {
                    let mapped = map_owned_wrapped_line_to_range(text, cursor, slice, "");
                    let trailing_spaces =
                        text[mapped.end..].chars().take_while(|c| *c == ' ').count();
                    lines.push(mapped.start..mapped.end + trailing_spaces + 1);
                    cursor = mapped.end + trailing_spaces;
                    continue;
                };
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

/// Byte offset of `slice` within `text`, or `None` when `slice` does not
/// lie inside `text`. Compares addresses only, so it needs no `unsafe`.
fn subslice_offset(text: &str, slice: &str) -> Option<usize> {
    let start = (slice.as_ptr() as usize).checked_sub(text.as_ptr() as usize)?;
    (start + slice.len() <= text.len()).then_some(start)
}

/// Start of the hard line that follows `cursor`, for an empty wrapped line
/// textwrap did not borrow from `text`. The first wrapped line always
/// starts at 0.
fn next_hard_line_start(text: &str, cursor: usize, line_index: usize) -> usize {
    if line_index == 0 {
        return 0;
    }
    text[cursor..]
        .find('\n')
        .map_or(text.len(), |newline| cursor + newline + 1)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn wrap_first_fit(text: &str, width: usize) -> Vec<Range<usize>> {
        wrap_ranges(
            text,
            Options::new(width).wrap_algorithm(textwrap::WrapAlgorithm::FirstFit),
        )
    }

    // Regression: a pty with no size gave the composer width 0, and typing a
    // newline panicked with an out-of-bounds start index computed from a
    // static `""` that textwrap returns for empty lines at width 0.
    #[test]
    fn width_zero_with_newline_maps_empty_lines() {
        assert_eq!(wrap_first_fit("\n", 0), vec![0..1, 1..2]);
        assert_eq!(wrap_first_fit("a\n", 0), vec![0..2, 2..3]);
        assert_eq!(wrap_first_fit("a\n\nb", 0), vec![0..2, 2..3, 3..5]);
        assert_eq!(wrap_first_fit("\n\n", 0), vec![0..1, 1..2, 2..3]);
    }

    #[test]
    fn width_zero_matches_normal_width_for_short_lines() {
        for text in ["\n", "a\n", "a\n\nb", "\n\n", "", "a"] {
            assert_eq!(
                wrap_first_fit(text, 0),
                wrap_first_fit(text, 80),
                "{text:?}"
            );
        }
    }

    // Upstream openai/codex#35962: halfwidth sound marks count as one column
    // and stay with their base character; byte offsets are unchanged.
    #[test]
    fn wrap_ranges_count_halfwidth_sound_marks_without_changing_byte_offsets() {
        for (text, width, expected) in [
            ("abｶﾞc", 4, &["abｶﾞ", "c"][..]),
            ("abｶﾞc", 3, &["ab", "ｶﾞc"][..]),
            ("ﾞab", 2, &["ﾞa", "b"][..]),
            ("a ﾞb", 2, &["a", "ﾞb"][..]),
            ("a ﾟb", 2, &["a", "ﾟb"][..]),
            ("ｶﾞﾞx", 3, &["ｶﾞﾞ", "x"][..]),
            ("界ﾞx", 3, &["界ﾞ", "x"][..]),
            ("ｶﾞﾞab", 2, &["ｶﾞﾞ", "ab"][..]),
            ("界ﾞab", 2, &["界ﾞ", "ab"][..]),
            ("abｶﾞﾞcd", 2, &["ab", "ｶﾞﾞ", "cd"][..]),
            ("ab界ﾞcd", 2, &["ab", "界ﾞ", "cd"][..]),
        ] {
            let wrapped: Vec<&str> = wrap_first_fit(text, width)
                .into_iter()
                .map(|range| text[range.start..range.end - 1].trim_end_matches(' '))
                .collect();
            assert_eq!(wrapped, expected, "{text:?} at width {width}");
        }
    }

    #[test]
    fn subslice_offset_rejects_foreign_slices() {
        let text = String::from("hello");
        assert_eq!(subslice_offset(&text, &text[1..3]), Some(1));
        assert_eq!(subslice_offset(&text, &text[5..]), Some(5));
        assert_eq!(subslice_offset(&text, "elsewhere"), None);
    }
}
