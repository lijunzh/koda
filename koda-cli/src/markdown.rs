//! Streaming markdown renderer for terminal output.
//!
//! Renders markdown line-by-line as tokens arrive from the LLM,
//! handling headers, bold, italic, inline code, code blocks,
//! lists, blockquotes, and horizontal rules.

use crate::highlight::CodeHighlighter;

// ── ANSI codes ─────────────────────────────────────────────────

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const ITALIC: &str = "\x1b[3m";
const DIM: &str = "\x1b[90m";
const CYAN: &str = "\x1b[36m";
const BOLD_CYAN: &str = "\x1b[1;36m";

/// Content indent — markdown output sits under the `● Response` banner.
const INDENT: &str = "  ";

/// Get current terminal width, defaulting to 80 if unknown.
fn term_width() -> usize {
    crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80)
}

/// Count visible characters (strip ANSI escape sequences).
fn visible_len(s: &str) -> usize {
    let mut len = 0;
    let mut in_escape = false;
    for c in s.chars() {
        if c == '\x1b' {
            in_escape = true;
        } else if in_escape {
            if c == 'm' {
                in_escape = false;
            }
        } else {
            len += if c > '\u{FFFF}' { 2 } else { 1 };
        }
    }
    len
}

/// Word-wrap a line to fit within the terminal, preserving ANSI styles.
/// Continuation lines get the given `cont_indent` prefix.
/// Only used for prose text (not code blocks).
fn word_wrap(text: &str, max_width: usize, cont_indent: &str) -> String {
    if max_width == 0 || visible_len(text) <= max_width {
        return text.to_string();
    }

    let mut result = String::new();
    let mut col = 0;
    let mut word = String::new();
    let mut word_visible = 0;
    let mut in_escape = false;

    for ch in text.chars() {
        if ch == '\x1b' {
            in_escape = true;
            word.push(ch);
            continue;
        }
        if in_escape {
            word.push(ch);
            if ch == 'm' {
                in_escape = false;
            }
            continue;
        }

        if ch == ' ' {
            // Flush current word
            if col + word_visible > max_width {
                result.push('\n');
                result.push_str(cont_indent);
                col = visible_len(cont_indent);
            }
            result.push_str(&word);
            col += word_visible;
            word.clear();
            word_visible = 0;

            // Add the space
            if col < max_width {
                result.push(' ');
                col += 1;
            }
        } else {
            word.push(ch);
            word_visible += if ch > '\u{FFFF}' { 2 } else { 1 };
        }
    }

    // Flush last word
    if !word.is_empty() {
        if col + word_visible > max_width {
            result.push('\n');
            result.push_str(cont_indent);
        }
        result.push_str(&word);
    }

    result
}

/// A streaming markdown renderer that processes tokens and renders
/// complete lines with ANSI formatting.
pub struct MarkdownStreamer {
    /// Buffer for incomplete lines.
    buffer: String,
    /// Whether we're inside a fenced code block.
    in_code_block: bool,
    /// Syntax highlighter for the current code block (if any).
    highlighter: Option<CodeHighlighter>,
    /// Cached terminal width (avoids repeated ioctl syscalls).
    cached_width: usize,
}

impl MarkdownStreamer {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            in_code_block: false,
            highlighter: None,
            cached_width: term_width(),
        }
    }

    /// Feed a text delta (token) into the streamer.
    /// Complete lines are rendered immediately.
    pub fn push(&mut self, delta: &str) {
        self.buffer.push_str(delta);

        // Render all complete lines
        while let Some(newline_pos) = self.buffer.find('\n') {
            let line = self.buffer[..newline_pos].to_string();
            self.buffer = self.buffer[newline_pos + 1..].to_string();
            self.render_line(&line);
        }
    }

    /// Flush any remaining buffered text (call when stream ends).
    pub fn flush(&mut self) {
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            self.render_line(&line);
        }
        // Reset any open code block state
        if self.in_code_block {
            println!("{RESET}");
            self.in_code_block = false;
            self.highlighter = None;
        }
    }

    /// Render a single complete line with markdown formatting.
    /// All output is indented under the banner.
    /// Prose is word-wrapped to terminal width; code blocks are not.
    fn render_line(&mut self, line: &str) {
        let trimmed = line.trim();
        let width = self.cached_width;

        // ── Code block fences ─────────────────────────────────
        if trimmed.starts_with("```") {
            if self.in_code_block {
                // Closing fence
                println!("{INDENT}{DIM}└───{RESET}");
                self.in_code_block = false;
                self.highlighter = None;
            } else {
                // Opening fence — show language hint if present
                let lang = trimmed.strip_prefix("```").unwrap_or("").trim();
                if lang.is_empty() {
                    println!("{INDENT}{DIM}┌───{RESET}");
                    self.highlighter = Some(CodeHighlighter::new(""));
                } else {
                    println!("{INDENT}{DIM}┌── {lang} ──{RESET}");
                    self.highlighter = Some(CodeHighlighter::new(lang));
                }
                self.in_code_block = true;
            }
            return;
        }

        // ── Inside code block: syntax highlight ─────────────────
        if self.in_code_block {
            let highlighted = match &mut self.highlighter {
                Some(h) => h.highlight_line(line),
                None => line.to_string(),
            };
            println!("{INDENT}{DIM}│{RESET} {highlighted}");
            return;
        }

        // ── Empty lines ───────────────────────────────────────
        if trimmed.is_empty() {
            println!();
            return;
        }

        // ── Horizontal rules ─────────────────────────────────
        if is_horizontal_rule(trimmed) {
            println!("{INDENT}{DIM}{}{RESET}", "─".repeat(40));
            return;
        }

        // ── Headers (#, ##, ###) ──────────────────────────────
        if let Some(header_text) = parse_header(trimmed) {
            let full = format!("{INDENT}{BOLD_CYAN}{header_text}{RESET}");
            println!("{}", word_wrap(&full, width, INDENT));
            return;
        }

        // ── Blockquotes (>) ──────────────────────────────────
        if let Some(quote_text) = trimmed.strip_prefix("> ") {
            let formatted = render_inline(quote_text);
            println!("{INDENT}{DIM}│{RESET} {ITALIC}{formatted}{RESET}");
            return;
        }
        if trimmed == ">" {
            println!("{INDENT}{DIM}│{RESET}");
            return;
        }

        // ── Unordered lists (-, *, •) ─────────────────────────
        if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| trimmed.strip_prefix("• "))
        {
            let extra = leading_spaces(line);
            let formatted = render_inline(rest);
            let prefix = format!("{INDENT}{}{CYAN}•{RESET} ", " ".repeat(extra));
            let full = format!("{prefix}{formatted}");
            let cont = format!("{INDENT}{}  ", " ".repeat(extra));
            println!("{}", word_wrap(&full, width, &cont));
            return;
        }

        // ── Ordered lists (1., 2., etc.) ─────────────────────
        if let Some((num, rest)) = parse_numbered_list(trimmed) {
            let extra = leading_spaces(line);
            let formatted = render_inline(rest);
            let prefix = format!("{INDENT}{}{CYAN}{num}.{RESET} ", " ".repeat(extra));
            let full = format!("{prefix}{formatted}");
            let cont = format!("{INDENT}{}   ", " ".repeat(extra));
            println!("{}", word_wrap(&full, width, &cont));
            return;
        }

        // ── Tables (| col | col |) ───────────────────────────
        if trimmed.starts_with('|') && trimmed.ends_with('|') {
            // Separator row (|---|---| or |:---:|)
            if is_table_separator(trimmed) {
                let col_count = trimmed.matches('|').count() - 1;
                let sep = format!(
                    "{INDENT}{DIM}{}{RESET}",
                    "─".repeat(col_count * 12 + col_count + 1)
                );
                println!("{sep}");
            } else {
                // Data/header row
                let cells: Vec<&str> = trimmed
                    .trim_matches('|')
                    .split('|')
                    .map(|c| c.trim())
                    .collect();
                let formatted_cells: Vec<String> = cells
                    .iter()
                    .map(|c| {
                        let rendered = render_inline(c);
                        format!("{rendered:<12}")
                    })
                    .collect();
                println!(
                    "{INDENT}{DIM}|{RESET} {} {DIM}|{RESET}",
                    formatted_cells.join(&format!(" {DIM}|{RESET} "))
                );
            }
            return;
        }

        // ── Normal paragraph text with inline formatting ───────
        let formatted = render_inline(trimmed);
        let full = format!("{INDENT}{formatted}");
        println!("{}", word_wrap(&full, width, INDENT));
    }
}

// ── Inline formatting ─────────────────────────────────────────

/// Render inline markdown formatting: **bold**, *italic*, `code`.
fn render_inline(text: &str) -> String {
    let mut result = String::with_capacity(text.len() + 64);
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // Markdown links: [text](url) → OSC 8 hyperlink
        if chars[i] == '['
            && let Some((link_text, url, end_pos)) = parse_link(&chars, i)
        {
            // OSC 8 hyperlink: \e]8;;url\e\\text\e]8;;\e\\
            result.push_str(&format!(
                "\x1b]8;;{url}\x1b\\{CYAN}{link_text}{RESET}\x1b]8;;\x1b\\"
            ));
            i = end_pos;
            continue;
        }

        // Inline code: `...`
        if chars[i] == '`'
            && let Some(end) = find_closing(&chars, i + 1, '`')
        {
            result.push_str(CYAN);
            result.push('`');
            for c in &chars[i + 1..end] {
                result.push(*c);
            }
            result.push('`');
            result.push_str(RESET);
            i = end + 1;
            continue;
        }

        // Bold: **...**
        if i + 1 < len
            && chars[i] == '*'
            && chars[i + 1] == '*'
            && let Some(end) = find_double_closing(&chars, i + 2, '*')
        {
            result.push_str(BOLD);
            for c in &chars[i + 2..end] {
                result.push(*c);
            }
            result.push_str(RESET);
            i = end + 2;
            continue;
        }

        // Italic: *...*  (single asterisk, not at word boundary issues)
        if chars[i] == '*'
            && (i + 1 < len && chars[i + 1] != '*' && chars[i + 1] != ' ')
            && let Some(end) = find_closing(&chars, i + 1, '*')
        {
            result.push_str(ITALIC);
            for c in &chars[i + 1..end] {
                result.push(*c);
            }
            result.push_str(RESET);
            i = end + 1;
            continue;
        }

        result.push(chars[i]);
        i += 1;
    }

    result
}

// ── Parsing helpers ───────────────────────────────────────────

/// Find the closing delimiter starting from position `from`.
fn find_closing(chars: &[char], from: usize, delim: char) -> Option<usize> {
    (from..chars.len()).find(|&j| chars[j] == delim)
}

/// Find closing double delimiter (e.g., **).
fn find_double_closing(chars: &[char], from: usize, delim: char) -> Option<usize> {
    (from..chars.len().saturating_sub(1)).find(|&j| chars[j] == delim && chars[j + 1] == delim)
}

/// Parse a markdown link `[text](url)` starting at position `from`.
/// Returns (text, url, end_position) or None.
fn parse_link(chars: &[char], from: usize) -> Option<(String, String, usize)> {
    if chars[from] != '[' {
        return None;
    }
    // Find closing ]
    let close_bracket = find_closing(chars, from + 1, ']')?;
    // Must be immediately followed by (
    if close_bracket + 1 >= chars.len() || chars[close_bracket + 1] != '(' {
        return None;
    }
    // Find closing )
    let close_paren = find_closing(chars, close_bracket + 2, ')')?;

    let text: String = chars[from + 1..close_bracket].iter().collect();
    let url: String = chars[close_bracket + 2..close_paren].iter().collect();

    Some((text, url, close_paren + 1))
}

/// Parse a markdown header line, returning the text without `#` prefix.
fn parse_header(line: &str) -> Option<&str> {
    let trimmed = line.trim_start_matches('#');
    // Must have at least one # and a space after
    if trimmed.len() < line.len() && trimmed.starts_with(' ') {
        Some(trimmed.trim())
    } else {
        None
    }
}

/// Parse a numbered list item, returning (number, rest).
fn parse_numbered_list(line: &str) -> Option<(&str, &str)> {
    let dot_pos = line.find('.')?;
    let num = &line[..dot_pos];
    if num.chars().all(|c| c.is_ascii_digit()) && line.get(dot_pos + 1..dot_pos + 2) == Some(" ") {
        Some((num, &line[dot_pos + 2..]))
    } else {
        None
    }
}

/// Check if a line is a markdown table separator (|---|---|).
fn is_table_separator(line: &str) -> bool {
    let inner = line.trim_matches('|').trim();
    !inner.is_empty()
        && inner.split('|').all(|cell| {
            cell.trim()
                .chars()
                .all(|c| c == '-' || c == ':' || c == ' ')
        })
}

/// Check if a line is a horizontal rule (---, ***, ___).
fn is_horizontal_rule(line: &str) -> bool {
    let chars: Vec<char> = line.chars().filter(|c| !c.is_whitespace()).collect();
    chars.len() >= 3
        && chars.iter().all(|&c| c == '-' || c == '*' || c == '_')
        && chars.windows(2).all(|w| w[0] == w[1])
}

/// Count leading spaces.
fn leading_spaces(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_inline_bold() {
        let result = render_inline("hello **world** end");
        assert!(result.contains(BOLD));
        assert!(result.contains("world"));
        assert!(result.contains(RESET));
        assert!(!result.contains("**"));
    }

    #[test]
    fn test_render_inline_italic() {
        let result = render_inline("hello *world* end");
        assert!(result.contains(ITALIC));
        assert!(result.contains("world"));
    }

    #[test]
    fn test_render_inline_code() {
        let result = render_inline("use `cargo build` here");
        assert!(result.contains(CYAN));
        assert!(result.contains("`cargo build`"));
    }

    #[test]
    fn test_render_inline_mixed() {
        let result = render_inline("**bold** and `code` and *italic*");
        assert!(result.contains(BOLD));
        assert!(result.contains(CYAN));
        assert!(result.contains(ITALIC));
    }

    #[test]
    fn test_render_inline_no_formatting() {
        let result = render_inline("just plain text");
        assert_eq!(result, "just plain text");
    }

    #[test]
    fn test_parse_header() {
        assert_eq!(parse_header("# Hello"), Some("Hello"));
        assert_eq!(parse_header("## Sub"), Some("Sub"));
        assert_eq!(parse_header("### Deep"), Some("Deep"));
        assert_eq!(parse_header("Not a header"), None);
        assert_eq!(parse_header("#NoSpace"), None);
    }

    #[test]
    fn test_parse_numbered_list() {
        assert_eq!(parse_numbered_list("1. First"), Some(("1", "First")));
        assert_eq!(parse_numbered_list("12. Twelfth"), Some(("12", "Twelfth")));
        assert_eq!(parse_numbered_list("Not a list"), None);
    }

    #[test]
    fn test_is_horizontal_rule() {
        assert!(is_horizontal_rule("---"));
        assert!(is_horizontal_rule("***"));
        assert!(is_horizontal_rule("___"));
        assert!(is_horizontal_rule("- - -"));
        assert!(!is_horizontal_rule("--"));
        assert!(!is_horizontal_rule("hello"));
    }

    #[test]
    fn test_streamer_buffers_incomplete_lines() {
        let mut s = MarkdownStreamer::new();
        s.push("hel");
        assert_eq!(s.buffer, "hel");
        s.push("lo\n");
        assert!(s.buffer.is_empty()); // line was rendered
    }

    #[test]
    fn test_streamer_code_block_state() {
        let mut s = MarkdownStreamer::new();
        assert!(!s.in_code_block);
        s.push("```rust\n");
        assert!(s.in_code_block);
        s.push("let x = 1;\n");
        assert!(s.in_code_block);
        s.push("```\n");
        assert!(!s.in_code_block);
    }

    #[test]
    fn test_parse_link() {
        let chars: Vec<char> = "[click here](https://example.com)".chars().collect();
        let (text, url, end) = parse_link(&chars, 0).unwrap();
        assert_eq!(text, "click here");
        assert_eq!(url, "https://example.com");
        assert_eq!(end, chars.len());
    }

    #[test]
    fn test_parse_link_no_match() {
        let chars: Vec<char> = "not a link".chars().collect();
        assert!(parse_link(&chars, 0).is_none());
    }

    #[test]
    fn test_render_inline_link() {
        let result = render_inline("see [docs](https://example.com) here");
        assert!(result.contains("docs"));
        assert!(result.contains("example.com"));
        assert!(result.contains(CYAN));
    }

    #[test]
    fn test_is_table_separator() {
        assert!(is_table_separator("|---|---|"));
        assert!(is_table_separator("|:---:|:---:|"));
        assert!(is_table_separator("| --- | --- |"));
        assert!(!is_table_separator("| data | data |"));
        assert!(!is_table_separator("not a table"));
    }
}
