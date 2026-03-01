//! Syntax highlighting for code blocks using syntect.
//!
//! Provides terminal-colored syntax highlighting for code in
//! fenced markdown code blocks. Uses the same engine as `bat`.

use once_cell::sync::Lazy;
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use syntect::util::as_24_bit_terminal_escaped;

/// Lazily loaded syntax definitions and theme.
static SYNTAX_SET: Lazy<SyntaxSet> = Lazy::new(SyntaxSet::load_defaults_newlines);
static THEME_SET: Lazy<ThemeSet> = Lazy::new(ThemeSet::load_defaults);

/// A syntax highlighter for a specific language.
pub struct CodeHighlighter {
    highlighter: Option<HighlightLines<'static>>,
}

impl CodeHighlighter {
    /// Create a highlighter for the given language hint (e.g., "rust", "python").
    /// Returns a no-op highlighter if the language is unknown.
    pub fn new(lang: &str) -> Self {
        let syntax = SYNTAX_SET
            .find_syntax_by_token(lang)
            .or_else(|| SYNTAX_SET.find_syntax_by_extension(lang));

        let highlighter = syntax.map(|syn| {
            let theme = &THEME_SET.themes["base16-ocean.dark"];
            // SAFETY: HighlightLines borrows SyntaxSet, which is 'static via Lazy.
            // We transmute the lifetime to satisfy the borrow checker.
            // This is safe because SYNTAX_SET lives for the entire program.
            unsafe {
                std::mem::transmute::<HighlightLines<'_>, HighlightLines<'static>>(
                    HighlightLines::new(syn, theme),
                )
            }
        });

        Self { highlighter }
    }

    /// Highlight a single line of code, returning ANSI-colored output.
    pub fn highlight_line(&mut self, line: &str) -> String {
        match &mut self.highlighter {
            Some(h) => {
                let ranges = h.highlight_line(line, &SYNTAX_SET).unwrap_or_default();
                let escaped = as_24_bit_terminal_escaped(&ranges[..], false);
                format!("{escaped}\x1b[0m")
            }
            None => line.to_string(),
        }
    }
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
