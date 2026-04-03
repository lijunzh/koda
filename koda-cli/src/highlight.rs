//! Syntax highlighting for code blocks using syntect.
//!
//! Provides terminal-colored syntax highlighting for code in
//! fenced markdown code blocks. Uses the same engine as `bat`.

use once_cell::sync::Lazy;
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
#[cfg(test)]
use syntect::util::as_24_bit_terminal_escaped;

/// Lazily loaded syntax definitions and theme.
static SYNTAX_SET: Lazy<SyntaxSet> = Lazy::new(SyntaxSet::load_defaults_newlines);
static THEME_SET: Lazy<ThemeSet> = Lazy::new(ThemeSet::load_defaults);

/// A syntax highlighter for a specific language.
///
/// Stores a reference to the static `SyntaxReference` and creates a fresh
/// `HighlightLines` on demand — no unsafe code needed.
pub struct CodeHighlighter {
    /// Persistent parse state for stateful (cross-line) highlighting.
    state: Option<HighlightLines<'static>>,
}

impl CodeHighlighter {
    /// Create a highlighter for the given language hint (e.g., "rust", "python").
    ///
    /// Maintains parse state across calls to `highlight_spans_stateful()`
    /// so multiline strings, comments, and heredocs highlight correctly.
    /// Use `highlight_spans()` for one-off single-line highlighting.
    pub fn new(lang: &str) -> Self {
        let syntax = SYNTAX_SET
            .find_syntax_by_token(lang)
            .or_else(|| SYNTAX_SET.find_syntax_by_extension(lang));

        let state = syntax.map(|syn| {
            let theme = &THEME_SET.themes["base16-ocean.dark"];
            HighlightLines::new(syn, theme)
        });

        Self { state }
    }

    /// Highlight a single line of code, returning ANSI-colored output.
    ///
    /// Stateful — parse state carries across calls.
    #[cfg(test)]
    pub fn highlight_line(&mut self, line: &str) -> String {
        match self.state.as_mut() {
            Some(h) => {
                let ranges = h.highlight_line(line, &SYNTAX_SET).unwrap_or_default();
                let escaped = as_24_bit_terminal_escaped(&ranges[..], false);
                format!("{escaped}\x1b[0m")
            }
            None => line.to_string(),
        }
    }

    /// Highlight a line and return ratatui `Span`s with foreground colors.
    ///
    /// **Stateful** — parse state carries across calls, so multiline
    /// strings/comments highlight correctly. Call lines in order.
    ///
    /// No background is set — the caller controls backgrounds for diff rendering.
    pub fn highlight_spans(&mut self, line: &str) -> Vec<ratatui::text::Span<'static>> {
        use ratatui::style::{Color, Style as RStyle};
        use ratatui::text::Span;

        match self.state.as_mut() {
            Some(h) => {
                let ranges = h.highlight_line(line, &SYNTAX_SET).unwrap_or_default();
                ranges
                    .into_iter()
                    .map(|(style, text)| {
                        let fg =
                            Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);
                        Span::styled(text.to_string(), RStyle::default().fg(fg))
                    })
                    .collect()
            }
            None => vec![Span::raw(line.to_string())],
        }
    }
}

/// Pre-highlight an entire file, returning styled spans per line.
///
/// Maintains syntect parse state across lines for correct multiline
/// string / comment / heredoc highlighting. Used by the diff renderer
/// to look up pre-computed highlights by line number.
pub fn pre_highlight(content: &str, ext: &str) -> Vec<Vec<ratatui::text::Span<'static>>> {
    let mut hl = CodeHighlighter::new(ext);
    content.lines().map(|line| hl.highlight_spans(line)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_known_language_highlights() {
        let mut h = CodeHighlighter::new("rust");
        let result = h.highlight_line("fn main() {}");
        // Should contain ANSI escape codes
        assert!(result.contains("\x1b["));
        assert!(result.contains("fn"));
    }

    #[test]
    fn test_unknown_language_passthrough() {
        let mut h = CodeHighlighter::new("nonexistent_lang_xyz");
        let result = h.highlight_line("hello world");
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_python_highlights() {
        let mut h = CodeHighlighter::new("python");
        let result = h.highlight_line("def hello():");
        assert!(result.contains("\x1b["));
    }

    #[test]
    fn test_extension_lookup() {
        // "rs" should find Rust syntax
        let mut h = CodeHighlighter::new("rs");
        let result = h.highlight_line("let x = 42;");
        assert!(result.contains("\x1b["));
    }
}
