//! Fuzzy (whitespace-normalized) text matching for the Edit tool.
//!
//! When `old_str` fails an exact match, we try a line-by-line comparison that
//! strips trailing whitespace from every line.  This handles the most common
//! model failure modes:
//!
//! * Trailing spaces/tabs the model silently dropped
//! * CRLF vs LF line endings (`\r` is whitespace, so `trim_end` handles it)
//!
//! **Safety:** we only proceed if there is *exactly one* fuzzy match.
//! Zero matches → hard fail (no match at all).
//! Two or more matches → hard fail with "ambiguous" message.

use std::ops::Range;

/// Locate `needle` inside `haystack` using trailing-whitespace-stripped line
/// comparison.  Returns byte ranges **in the original `haystack`** that match.
///
/// Never called when `haystack.contains(needle)` — exact matching is always
/// tried first so this is the fallback path only.
///
/// # Examples
///
/// ```
/// use koda_core::tools::fuzzy::fuzzy_match_ranges;
///
/// // File has trailing spaces that the model's edit stripped:
/// let file = "fn foo() {\n    let x = 1;   \n}\n";
/// let needle = "    let x = 1;";
/// let matches = fuzzy_match_ranges(needle, file);
/// assert_eq!(matches.len(), 1);
///
/// // No match at all:
/// assert!(fuzzy_match_ranges("not here", file).is_empty());
/// ```
pub fn fuzzy_match_ranges(needle: &str, haystack: &str) -> Vec<Range<usize>> {
    // Strip trailing whitespace from every needle line.
    let norm_needle: Vec<&str> = needle.lines().map(str::trim_end).collect();
    if norm_needle.is_empty() {
        return vec![];
    }
    let n = norm_needle.len();

    // Build (start_byte, end_byte, line_content) for each haystack line.
    // end_byte points to the last character of the line, NOT the '\n'.
    let hay_lines = collect_lines(haystack);
    let h = hay_lines.len();

    let mut matches = Vec::new();

    for start in 0..h {
        if start + n > h {
            break;
        }

        let hit = norm_needle
            .iter()
            .enumerate()
            .all(|(j, &nline)| hay_lines[start + j].2.trim_end() == nline);

        if hit {
            let match_start = hay_lines[start].0;
            let match_end = hay_lines[start + n - 1].1;
            // If the needle ended with '\n', include the newline so the
            // replacement length is consistent with what exact-match would do.
            let end = if needle.ends_with('\n') && match_end < haystack.len() {
                match_end + 1
            } else {
                match_end
            };
            matches.push(match_start..end);
        }
    }

    matches
}

/// Split `text` into `(start_byte, end_byte_excl_newline, &str)` triples.
///
/// `end_byte_excl_newline` is the byte position *after* the last character of
/// the line, not counting the `\n` itself.  For a CRLF file the `\r` is part
/// of the line content and will be stripped by `trim_end`.
fn collect_lines(text: &str) -> Vec<(usize, usize, &str)> {
    let mut result = Vec::new();
    let mut offset = 0usize;
    let mut rest = text;

    loop {
        match rest.find('\n') {
            Some(nl) => {
                let line = &rest[..nl];
                result.push((offset, offset + line.len(), line));
                offset += nl + 1;
                rest = &rest[nl + 1..];
            }
            None => {
                // Final line with no trailing newline.
                result.push((offset, offset + rest.len(), rest));
                break;
            }
        }
    }

    result
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn apply_one(needle: &str, haystack: &str, replacement: &str) -> String {
        let ranges = fuzzy_match_ranges(needle, haystack);
        assert_eq!(ranges.len(), 1, "expected exactly one fuzzy match");
        let r = ranges.into_iter().next().unwrap();
        format!(
            "{}{}{}",
            &haystack[..r.start],
            replacement,
            &haystack[r.end..]
        )
    }

    #[test]
    fn exact_match_returns_nothing_here() {
        // fuzzy_match_ranges is only called when exact fails; if the text IS
        // present we still get a result — that's fine, callers guard on exact first.
        let hay = "fn foo() {\n    let x = 1;\n}\n";
        let needle = "    let x = 1;";
        let r = fuzzy_match_ranges(needle, hay);
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn trailing_space_on_needle_matches() {
        // Model strips trailing space; file has "let x = 1;   "
        let hay = "fn foo() {\n    let x = 1;   \n}\n";
        let needle = "    let x = 1;";
        let result = apply_one(needle, hay, "    let x = 2;");
        assert_eq!(result, "fn foo() {\n    let x = 2;\n}\n");
    }

    #[test]
    fn trailing_space_on_file_multiline() {
        let hay = "a  \nb  \nc\n";
        let needle = "a\nb";
        let result = apply_one(needle, hay, "X\nY");
        assert_eq!(result, "X\nY\nc\n");
    }

    #[test]
    fn crlf_file_matches_lf_needle() {
        // File has CRLF; model sends LF-only needle
        let hay = "fn foo() {\r\n    let x = 1;\r\n}\r\n";
        let needle = "    let x = 1;";
        let ranges = fuzzy_match_ranges(needle, hay);
        assert_eq!(ranges.len(), 1, "CRLF file should match LF needle");
    }

    #[test]
    fn no_match_returns_empty() {
        let hay = "hello world\n";
        let needle = "does not exist";
        assert!(fuzzy_match_ranges(needle, hay).is_empty());
    }

    #[test]
    fn ambiguous_match_returns_multiple() {
        let hay = "foo  \nbar\nfoo  \nbar\n";
        let needle = "foo\nbar";
        let ranges = fuzzy_match_ranges(needle, hay);
        assert_eq!(ranges.len(), 2, "should detect ambiguity");
    }

    #[test]
    fn needle_with_trailing_newline_includes_newline_in_range() {
        let hay = "a\nb\nc\n";
        let needle = "a\nb\n";
        let ranges = fuzzy_match_ranges(needle, hay);
        assert_eq!(ranges.len(), 1);
        // Should include up to and including the '\n' after 'b'
        assert_eq!(&hay[ranges[0].clone()], "a\nb\n");
    }

    #[test]
    fn single_line_trailing_space() {
        let hay = "    return Ok(value);   \n";
        let needle = "    return Ok(value);";
        let result = apply_one(needle, hay, "    return Ok(new_value);");
        assert_eq!(result, "    return Ok(new_value);\n");
    }
}
