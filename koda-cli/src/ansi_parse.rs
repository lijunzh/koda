//! ANSI escape code → ratatui Span conversion.
//!
//! Converts ANSI color/style escape sequences into native ratatui `Span`s
//! so tool output (cargo, git, pytest, etc.) renders with proper styles
//! in the fullscreen TUI instead of showing raw escape codes.
//!
//! Uses `ansi-to-tui` v8 for parsing, with a fast path that skips
//! lines without escape codes entirely.

use ansi_to_tui::IntoText;
use ratatui::text::Span;

/// Parse ANSI escape codes in a single line into ratatui `Span`s.
///
/// Uses `ansi-to-tui` to convert escape sequences (colors, bold, etc.)
/// into native ratatui styles. Plain text without ANSI passes through
/// as a single unstyled span — zero overhead for non-colored output.
pub(crate) fn parse_ansi_spans(line: &str) -> Vec<Span<'static>> {
    // Fast path: no escape codes → skip parsing entirely
    if !line.contains('\x1b') {
        return vec![Span::raw(line.to_string())];
    }

    // Parse ANSI → ratatui Text (may produce multiple Lines for embedded \n)
    match line.as_bytes().into_text() {
        Ok(text) => {
            // Flatten all lines' spans into a single line
            // (we're processing line-by-line, so typically 1 Line)
            text.lines
                .into_iter()
                .flat_map(|l| l.spans)
                .map(|s| Span::styled(s.content.into_owned(), s.style))
                .collect()
        }
        Err(_) => {
            // Fallback: strip escapes and render plain
            let stripped = strip_ansi_escapes(line);
            vec![Span::raw(stripped)]
        }
    }
}

/// Fallback ANSI stripper for malformed escape sequences.
/// Removes all `\x1b[...m` style codes.
pub(crate) fn strip_ansi_escapes(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip until 'm' or end
            while let Some(&next) = chars.peek() {
                chars.next();
                if next == 'm' {
                    break;
                }
            }
        } else {
            result.push(c);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    #[test]
    fn test_parse_ansi_plain_text() {
        let spans = parse_ansi_spans("hello world");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.as_ref(), "hello world");
    }

    #[test]
    fn test_parse_ansi_colored_text() {
        // Red text: \x1b[31mERROR\x1b[0m
        let input = "\x1b[31mERROR\x1b[0m: something failed";
        let spans = parse_ansi_spans(input);
        assert!(spans.len() >= 2, "should produce multiple spans: {spans:?}");
        // First span should contain "ERROR" with red styling
        assert_eq!(spans[0].content.as_ref(), "ERROR");
        assert_eq!(spans[0].style.fg, Some(Color::Red));
    }

    #[test]
    fn test_parse_ansi_no_escape_fast_path() {
        // No \x1b → fast path, single raw span
        let spans = parse_ansi_spans("just plain text");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.as_ref(), "just plain text");
    }

    #[test]
    fn test_strip_ansi_escapes_fallback() {
        let input = "\x1b[1;32mOK\x1b[0m done";
        let stripped = strip_ansi_escapes(input);
        assert_eq!(stripped, "OK done");
    }

    #[test]
    fn test_strip_ansi_empty() {
        assert_eq!(strip_ansi_escapes(""), "");
    }
}
