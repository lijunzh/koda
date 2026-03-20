//! Shared word-wrap line counting.
//!
//! Single source of truth for the word-boundary wrapping algorithm
//! used by both `scroll_buffer` (history panel) and `wrap_input`
//! (input area). Must match ratatui's `Wrap { trim: false }` behavior.
//!
//! Extracted from duplicated implementations (#527).

use unicode_width::UnicodeWidthChar;

/// Compute how many visual lines a text string occupies at a given width.
///
/// Uses word-boundary wrapping consistent with ratatui's
/// `Paragraph::wrap(Wrap { trim: false })`. When a word would overflow
/// the current row, it breaks *before* the word.
///
/// For words longer than the terminal width, force-breaks mid-word.
pub fn visual_line_count(text: &str, width: usize) -> usize {
    if text.is_empty() {
        return 1;
    }
    let w = width.max(1);
    let mut rows = 1usize;
    let mut col = 0usize;
    let mut word_start_col = 0usize;
    let mut in_word = false;

    for ch in text.chars() {
        let char_w = ch.width().unwrap_or(0);
        let is_space = ch == ' ' || ch == '\t';

        if is_space {
            in_word = false;
            if col + char_w > w {
                rows += 1;
                col = char_w;
            } else {
                col += char_w;
            }
            word_start_col = col;
        } else {
            if !in_word {
                word_start_col = col;
                in_word = true;
            }
            if col + char_w > w {
                if word_start_col > 0 && word_start_col <= w {
                    // Word doesn't fit but row had prior content:
                    // wrap *before* this word.
                    rows += 1;
                    let word_len_so_far = col - word_start_col;
                    col = word_len_so_far + char_w;
                    word_start_col = 0;
                } else {
                    // Word at column 0 (longer than width): force-break.
                    rows += 1;
                    col = char_w;
                    word_start_col = 0;
                }
            } else {
                col += char_w;
            }
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_line() {
        assert_eq!(visual_line_count("hello", 80), 1);
    }

    #[test]
    fn empty_line() {
        assert_eq!(visual_line_count("", 80), 1);
    }

    #[test]
    fn char_wrap() {
        assert_eq!(visual_line_count(&"x".repeat(160), 80), 2);
    }

    #[test]
    fn word_wrap() {
        let line = format!("{} {}", "a".repeat(75), "b".repeat(75));
        assert_eq!(visual_line_count(&line, 80), 2);
    }

    #[test]
    fn word_longer_than_width() {
        assert_eq!(visual_line_count(&"x".repeat(200), 80), 3);
    }

    #[test]
    fn exact_width() {
        assert_eq!(visual_line_count(&"x".repeat(80), 80), 1);
    }

    #[test]
    fn exact_width_plus_one() {
        assert_eq!(visual_line_count(&"x".repeat(81), 80), 2);
    }

    #[test]
    fn word_wrap_breaks_before_word() {
        let text = format!("{}  foobar", "a".repeat(76));
        assert_eq!(visual_line_count(&text, 80), 2);
    }
}
