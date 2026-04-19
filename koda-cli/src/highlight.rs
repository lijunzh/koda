//! Syntax highlighting for code blocks using syntect.
//!
//! Provides terminal-colored syntax highlighting for code in
//! fenced markdown code blocks. Uses the same engine as `bat`.
//!
//! ## Architecture
//!
//! | Type | Role |
//! |------|------|
//! | [`CodeHighlighter`] | Stateful per-file highlighter — carries parse state across lines |
//! | [`pre_highlight`] | Convenience: highlight all lines of a file in one call |
//!
//! ## Stateful vs. stateless
//!
//! Syntax highlighting is **stateful**: a multiline string that starts on
//! line 3 affects how line 4 is colored. `CodeHighlighter` preserves this
//! state across `highlight_spans()` calls. Always call lines in order.
//!
//! ## Language lookup
//!
//! Languages are found by name token (`"rust"`, `"python"`) or by file
//! extension (`"rs"`, `"py"`). Unknown languages fall back to unstyled
//! passthrough — no panic.
//!
//! Syntaxes and themes are loaded once at first use via [`once_cell::sync::Lazy`].
//! Theme: `base16-ocean.dark` (matches popular terminal palettes).

use once_cell::sync::Lazy;
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
#[cfg(test)]
use syntect::util::as_24_bit_terminal_escaped;

/// Lazily loaded syntax definitions and theme.
static SYNTAX_SET: Lazy<SyntaxSet> = Lazy::new(SyntaxSet::load_defaults_newlines);
static THEME_SET: Lazy<ThemeSet> = Lazy::new(ThemeSet::load_defaults);

// Guardrails so a Read of a 50 MB minified bundle doesn't peg syntect.
// Borrowed from codex's `exceeds_highlight_limits` — same numbers, same
// reason: above these sizes the wall-clock cost of highlighting starts
// to dominate the render frame.
//
/// Skip syntax highlighting for files larger than this many bytes.
pub const MAX_HIGHLIGHT_BYTES: usize = 512 * 1024;
/// Skip syntax highlighting for files with more than this many lines.
pub const MAX_HIGHLIGHT_LINES: usize = 10_000;

/// Returns true when input is too large to syntax-highlight without
/// noticeable lag. Callers fall back to plain rendering.
pub fn exceeds_highlight_limits(total_bytes: usize, total_lines: usize) -> bool {
    total_bytes > MAX_HIGHLIGHT_BYTES || total_lines > MAX_HIGHLIGHT_LINES
}

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
    ///
    /// Honors `KODA_SYNTAX_HIGHLIGHT=off` — when disabled the returned
    /// highlighter is a no-op (every line passes through as plain text).
    pub fn new(lang: &str) -> Self {
        if !crate::theme::syntax_highlight_enabled() {
            return Self { state: None };
        }
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
///
/// Returns one `Vec<Span>` per **source line** (trailing newlines stripped).
/// Empty input returns an empty vec.
///
/// # Example
///
/// ```ignore
/// use koda_cli::highlight::pre_highlight;
///
/// // Two Rust lines → two span vecs
/// let lines = pre_highlight("fn main() {}\nlet x = 42;", "rs");
/// assert_eq!(lines.len(), 2);
/// // Each vec contains at least one span
/// for span_vec in &lines {
///     assert!(!span_vec.is_empty());
/// }
///
/// // Unknown extension falls back to a single unstyled span per line
/// let plain = pre_highlight("hello\nworld", "xyz_unknown");
/// assert_eq!(plain.len(), 2);
/// assert_eq!(plain[0][0].content.as_ref(), "hello");
/// ```
pub fn pre_highlight(content: &str, ext: &str) -> Vec<Vec<ratatui::text::Span<'static>>> {
    // Guardrail: massive files (e.g. minified JS bundles) burn syntect
    // wall-clock without giving the user useful color information — fall
    // back to plain spans per line. Same thresholds as codex.
    let line_count = content.lines().count();
    if exceeds_highlight_limits(content.len(), line_count) {
        return content
            .lines()
            .map(|line| vec![ratatui::text::Span::raw(line.to_string())])
            .collect();
    }
    let mut hl = CodeHighlighter::new(ext);
    content
        .lines()
        .map(|line| hl.highlight_spans(line))
        .collect()
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

    #[test]
    fn test_highlight_spans_rust() {
        let mut h = CodeHighlighter::new("rust");
        let spans = h.highlight_spans("fn main() {}");
        assert!(!spans.is_empty(), "should produce at least one span");
        // Spans should contain the full text
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("fn"));
        assert!(text.contains("main"));
    }

    #[test]
    fn test_highlight_spans_unknown_lang_passthrough() {
        let mut h = CodeHighlighter::new("notalang");
        let spans = h.highlight_spans("hello world");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.as_ref(), "hello world");
    }

    #[test]
    fn test_pre_highlight_produces_per_line_spans() {
        let content = "fn main() {}\nlet x = 42;\n";
        let lines = pre_highlight(content, "rs");
        assert_eq!(lines.len(), 2, "should produce one Vec<Span> per line");
        for line_spans in &lines {
            assert!(!line_spans.is_empty());
        }
    }

    #[test]
    fn test_pre_highlight_empty_content() {
        let lines = pre_highlight("", "rs");
        assert!(lines.is_empty());
    }

    #[test]
    fn test_stateful_multiline_string() {
        // Highlighting should carry state across lines
        let mut h = CodeHighlighter::new("rust");
        let _line1 = h.highlight_spans("let s = \"");
        let line2 = h.highlight_spans("hello\"");
        // line2 should still produce spans (stateful parsing)
        assert!(!line2.is_empty());
    }

    #[test]
    fn test_size_guardrail_below_threshold() {
        // 1KB / 5 lines is well under the cap — no degradation.
        assert!(!exceeds_highlight_limits(1024, 5));
    }

    #[test]
    fn test_size_guardrail_byte_cap() {
        assert!(exceeds_highlight_limits(MAX_HIGHLIGHT_BYTES + 1, 1));
    }

    #[test]
    fn test_size_guardrail_line_cap() {
        assert!(exceeds_highlight_limits(1, MAX_HIGHLIGHT_LINES + 1));
    }

    #[test]
    fn test_pre_highlight_falls_back_for_huge_input() {
        // Generate input that trips the line-count cap — must produce
        // exactly one Span per line and skip syntect entirely.
        let big = "x\n".repeat(MAX_HIGHLIGHT_LINES + 5);
        let lines = pre_highlight(&big, "rs");
        assert_eq!(lines.len(), MAX_HIGHLIGHT_LINES + 5);
        // Each line should be a single plain span (no syntect coloring).
        for spans in &lines {
            assert_eq!(spans.len(), 1, "expected plain fallback, got highlighted");
        }
    }
}
