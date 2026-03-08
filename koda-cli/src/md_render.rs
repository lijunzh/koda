//! Streaming markdown → ratatui `Line` renderer.
//!
//! Converts raw markdown text (line by line) into styled ratatui
//! `Line`s with headers, bold, italic, inline code, fenced code
//! blocks (with syntax highlighting), lists, blockquotes, and HRs.
//!
//! This replaces the old ANSI-based `markdown.rs` with native ratatui types.

use crate::highlight::CodeHighlighter;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

const INDENT: &str = "  ";

// ── Styles ──────────────────────────────────────────────────

const HEADING_STYLE: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
const CODE_STYLE: Style = Style::new().fg(Color::Yellow);
const DIM_STYLE: Style = Style::new().fg(Color::DarkGray);
const BLOCKQUOTE_STYLE: Style = Style::new().fg(Color::DarkGray);
const HR_STYLE: Style = Style::new().fg(Color::DarkGray);

// ── State machine ───────────────────────────────────────────

/// Streaming markdown renderer that tracks fenced code block state.
pub struct MarkdownRenderer {
    /// Inside a fenced code block?
    in_code_block: bool,
    /// Syntax highlighter for the current code block.
    highlighter: Option<CodeHighlighter>,
}

impl MarkdownRenderer {
    pub fn new() -> Self {
        Self {
            in_code_block: false,
            highlighter: None,
        }
    }

    /// Render a single raw markdown line into a styled `Line`.
    pub fn render_line(&mut self, raw: &str) -> Line<'static> {
        // ── Code block fence ────────────────────────────────
        if raw.starts_with("```") {
            if self.in_code_block {
                // Closing fence
                self.in_code_block = false;
                self.highlighter = None;
                return Line::from(vec![Span::raw(INDENT), Span::styled("```", DIM_STYLE)]);
            } else {
                // Opening fence — extract lang hint
                let lang = raw.trim_start_matches('`').trim();
                self.in_code_block = true;
                self.highlighter = if lang.is_empty() {
                    None
                } else {
                    Some(CodeHighlighter::new(lang))
                };
                return Line::from(vec![
                    Span::raw(INDENT),
                    Span::styled(raw.to_string(), DIM_STYLE),
                ]);
            }
        }

        // ── Inside code block: syntax highlight ─────────────
        if self.in_code_block {
            let spans = match &mut self.highlighter {
                Some(h) => {
                    let mut s = vec![Span::raw(format!("{INDENT}  "))];
                    s.extend(h.highlight_spans(raw));
                    s
                }
                None => vec![
                    Span::raw(format!("{INDENT}  ")),
                    Span::styled(raw.to_string(), CODE_STYLE),
                ],
            };
            return Line::from(spans);
        }

        // ── Horizontal rule ─────────────────────────────────
        if is_horizontal_rule(raw) {
            return Line::from(vec![
                Span::raw(INDENT),
                Span::styled("─".repeat(60), HR_STYLE),
            ]);
        }

        // ── Heading ─────────────────────────────────────────
        if let Some((level, text)) = parse_heading(raw) {
            let prefix = match level {
                1 => "■ ",
                2 => "▸ ",
                3 => "• ",
                _ => "  ",
            };
            return Line::from(vec![
                Span::raw(INDENT),
                Span::styled(format!("{prefix}{text}"), HEADING_STYLE),
            ]);
        }

        // ── Blockquote ──────────────────────────────────────
        if let Some(text) = raw.strip_prefix('>') {
            let text = text.strip_prefix(' ').unwrap_or(text);
            let mut spans = vec![Span::raw(INDENT), Span::styled("│ ", BLOCKQUOTE_STYLE)];
            spans.extend(render_inline(text, BLOCKQUOTE_STYLE));
            return Line::from(spans);
        }

        // ── Unordered list ──────────────────────────────────
        if let Some((indent_level, text)) = parse_list_item(raw) {
            let bullet_indent = " ".repeat(indent_level * 2);
            let mut spans = vec![Span::raw(format!("{INDENT}{bullet_indent}• "))];
            spans.extend(render_inline(text, Style::default()));
            return Line::from(spans);
        }

        // ── Ordered list ────────────────────────────────────
        if let Some((num, text)) = parse_ordered_item(raw) {
            let mut spans = vec![Span::raw(format!("{INDENT}{num}. "))];
            spans.extend(render_inline(text, Style::default()));
            return Line::from(spans);
        }

        // ── Regular prose ───────────────────────────────────
        let mut spans = vec![Span::raw(INDENT.to_string())];
        spans.extend(render_inline(raw, Style::default()));
        Line::from(spans)
    }
}

// ── Inline formatting parser ────────────────────────────────

/// Parse inline markdown: **bold**, *italic*, `code`, and plain text.
fn render_inline(text: &str, base: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut chars = text.char_indices().peekable();
    let mut plain_start = 0;

    while let Some(&(i, c)) = chars.peek() {
        match c {
            '`' => {
                // Flush plain text before this marker
                if i > plain_start {
                    spans.push(Span::styled(text[plain_start..i].to_string(), base));
                }
                chars.next();
                // Find closing backtick
                let code_start = i + 1;
                let mut found = false;
                while let Some(&(j, c2)) = chars.peek() {
                    chars.next();
                    if c2 == '`' {
                        spans.push(Span::styled(text[code_start..j].to_string(), CODE_STYLE));
                        plain_start = j + 1;
                        found = true;
                        break;
                    }
                }
                if !found {
                    // No closing backtick — treat as plain
                    spans.push(Span::styled(text[i..].to_string(), base));
                    return spans;
                }
            }
            '*' => {
                // Check for ** (bold) or * (italic)
                let next_char = text.get(i + 1..i + 2);
                if next_char == Some("*") {
                    // Bold: **text**
                    if i > plain_start {
                        spans.push(Span::styled(text[plain_start..i].to_string(), base));
                    }
                    chars.next(); // consume first *
                    chars.next(); // consume second *
                    let bold_start = i + 2;
                    if let Some(end) = text[bold_start..].find("**") {
                        let end_abs = bold_start + end;
                        spans.push(Span::styled(
                            text[bold_start..end_abs].to_string(),
                            base.add_modifier(Modifier::BOLD),
                        ));
                        // Skip past closing **
                        plain_start = end_abs + 2;
                        // Advance chars iterator past the closing **
                        while let Some(&(j, _)) = chars.peek() {
                            if j >= plain_start {
                                break;
                            }
                            chars.next();
                        }
                    } else {
                        // No closing ** — treat as plain
                        spans.push(Span::styled(text[i..].to_string(), base));
                        return spans;
                    }
                } else {
                    // Italic: *text*
                    if i > plain_start {
                        spans.push(Span::styled(text[plain_start..i].to_string(), base));
                    }
                    chars.next(); // consume *
                    let italic_start = i + 1;
                    if let Some(end) = text[italic_start..].find('*') {
                        let end_abs = italic_start + end;
                        spans.push(Span::styled(
                            text[italic_start..end_abs].to_string(),
                            base.add_modifier(Modifier::ITALIC),
                        ));
                        plain_start = end_abs + 1;
                        while let Some(&(j, _)) = chars.peek() {
                            if j >= plain_start {
                                break;
                            }
                            chars.next();
                        }
                    } else {
                        spans.push(Span::styled(text[i..].to_string(), base));
                        return spans;
                    }
                }
            }
            _ => {
                chars.next();
            }
        }
    }

    // Flush remaining plain text
    if plain_start < text.len() {
        spans.push(Span::styled(text[plain_start..].to_string(), base));
    }

    spans
}

// ── Helpers ─────────────────────────────────────────────────

fn parse_heading(line: &str) -> Option<(usize, &str)> {
    let trimmed = line.trim_start();
    let level = trimmed.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&level) {
        let rest = trimmed[level..].strip_prefix(' ')?;
        Some((level, rest))
    } else {
        None
    }
}

fn parse_list_item(line: &str) -> Option<(usize, &str)> {
    let indent = line.bytes().take_while(|&b| b == b' ').count();
    let after_indent = &line[indent..];
    if let Some(rest) = after_indent
        .strip_prefix("- ")
        .or_else(|| after_indent.strip_prefix("* "))
        .or_else(|| after_indent.strip_prefix("+ "))
    {
        Some((indent / 2, rest))
    } else {
        None
    }
}

fn parse_ordered_item(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim_start();
    let num_end = trimmed.bytes().take_while(|b| b.is_ascii_digit()).count();
    if num_end > 0 {
        let rest = &trimmed[num_end..];
        if let Some(text) = rest.strip_prefix(". ") {
            return Some((&trimmed[..num_end], text));
        }
    }
    None
}

fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    (trimmed.starts_with("---") && trimmed.chars().all(|c| c == '-' || c == ' '))
        || (trimmed.starts_with("***") && trimmed.chars().all(|c| c == '*' || c == ' '))
        || (trimmed.starts_with("___") && trimmed.chars().all(|c| c == '_' || c == ' '))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_heading_parsing() {
        assert_eq!(parse_heading("# Hello"), Some((1, "Hello")));
        assert_eq!(parse_heading("## Sub"), Some((2, "Sub")));
        assert_eq!(parse_heading("### Third"), Some((3, "Third")));
        assert_eq!(parse_heading("Not a heading"), None);
    }

    #[test]
    fn test_list_parsing() {
        assert_eq!(parse_list_item("- item"), Some((0, "item")));
        assert_eq!(parse_list_item("  - nested"), Some((1, "nested")));
        assert_eq!(parse_list_item("    - deep"), Some((2, "deep")));
        assert_eq!(parse_list_item("* star"), Some((0, "star")));
    }

    #[test]
    fn test_ordered_list() {
        assert_eq!(parse_ordered_item("1. First"), Some(("1", "First")));
        assert_eq!(parse_ordered_item("42. Answer"), Some(("42", "Answer")));
        assert_eq!(parse_ordered_item("Not ordered"), None);
    }

    #[test]
    fn test_horizontal_rule() {
        assert!(is_horizontal_rule("---"));
        assert!(is_horizontal_rule("***"));
        assert!(is_horizontal_rule("___"));
        assert!(!is_horizontal_rule("--"));
    }

    #[test]
    fn test_inline_bold() {
        let spans = render_inline("hello **world** end", Style::default());
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content, "hello ");
        assert_eq!(spans[1].content, "world");
        assert!(spans[1].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(spans[2].content, " end");
    }

    #[test]
    fn test_inline_code() {
        let spans = render_inline("use `foo` here", Style::default());
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[1].content, "foo");
        assert_eq!(spans[1].style.fg, Some(Color::Yellow));
    }

    #[test]
    fn test_inline_italic() {
        let spans = render_inline("hello *world* end", Style::default());
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[1].content, "world");
        assert!(spans[1].style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn test_code_block_toggle() {
        let mut r = MarkdownRenderer::new();
        assert!(!r.in_code_block);
        r.render_line("```rust");
        assert!(r.in_code_block);
        r.render_line("fn main() {}");
        assert!(r.in_code_block);
        r.render_line("```");
        assert!(!r.in_code_block);
    }

    #[test]
    fn test_unclosed_bold() {
        let spans = render_inline("**unclosed bold", Style::default());
        // Should fall back to plain text, not panic
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "**unclosed bold");
    }

    #[test]
    fn test_unclosed_backtick() {
        let spans = render_inline("`unclosed code", Style::default());
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "`unclosed code");
    }

    #[test]
    fn test_unclosed_italic() {
        let spans = render_inline("*unclosed italic", Style::default());
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "*unclosed italic");
    }

    #[test]
    fn test_empty_line() {
        let mut r = MarkdownRenderer::new();
        let line = r.render_line("");
        assert!(!line.spans.is_empty());
    }

    #[test]
    fn test_heading_is_bold() {
        let mut r = MarkdownRenderer::new();
        let line = r.render_line("# Hello World");
        assert!(
            line.spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::BOLD)),
            "Heading should have bold span"
        );
    }

    #[test]
    fn test_heading_levels() {
        let mut r = MarkdownRenderer::new();
        for h in ["# H1", "## H2", "### H3"] {
            let line = r.render_line(h);
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(!text.is_empty(), "Heading '{h}' should render");
        }
    }

    #[test]
    fn test_list_item_renders() {
        let mut r = MarkdownRenderer::new();
        let line = r.render_line("- item one");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("item one"));
    }

    #[test]
    fn test_blockquote_renders() {
        let mut r = MarkdownRenderer::new();
        let line = r.render_line("> quoted text");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("quoted text"));
    }

    #[test]
    fn test_plain_text_passthrough() {
        let mut r = MarkdownRenderer::new();
        let line = r.render_line("Just plain text here");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("Just plain text here"));
    }

    #[test]
    fn test_hr_renders() {
        let mut r = MarkdownRenderer::new();
        let line = r.render_line("---");
        // HR should produce a styled line
        assert!(!line.spans.is_empty());
    }
}
