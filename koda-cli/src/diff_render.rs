//! Diff preview renderer with syntax highlighting and word-level diffs.
//!
//! Takes structured [`DiffPreview`] data from koda-core and produces
//! ANSI-colored terminal output with:
//! - Dark background tint for line-level diffs (red removed, green added)
//! - Brighter background for the changed words within a line
//! - Syntax highlighting (foreground colors via syntect)

use crate::highlight::CodeHighlighter;
use koda_core::preview::{
    DeleteDirPreview, DeleteFilePreview, DiffPreview, EditPreview, WriteOverwritePreview,
    WritePreview,
};

const LINE_RED: &str = "\x1b[48;2;80;0;0m"; // dark red bg — removed line
const LINE_GREEN: &str = "\x1b[48;2;0;60;0m"; // dark green bg — added line
const WORD_RED: &str = "\x1b[48;2;130;15;15m"; // brighter red — changed word
const WORD_GREEN: &str = "\x1b[48;2;15;100;15m"; // brighter green — changed word
const DIM: &str = "\x1b[90m";
const RESET: &str = "\x1b[0m";

/// Render a [`DiffPreview`] as ANSI-colored terminal output.
pub fn render(preview: &DiffPreview) -> String {
    match preview {
        DiffPreview::Edit(edit) => render_edit(edit),
        DiffPreview::WriteNew(w) => render_write_new(w),
        DiffPreview::WriteOverwrite(w) => render_write_overwrite(w),
        DiffPreview::DeleteFile(d) => render_delete_file(d),
        DiffPreview::DeleteDir(d) => render_delete_dir(d),
        DiffPreview::FileNotYetExists => format!("{DIM}(file does not exist yet){RESET}"),
        DiffPreview::PathNotFound => format!("{DIM}(path does not exist){RESET}"),
    }
}

fn render_edit(edit: &EditPreview) -> String {
    let ext = std::path::Path::new(&edit.path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    let mut lines = Vec::new();

    for r in &edit.replacements {
        if r.total > 1 {
            lines.push(format!(
                "{DIM}── replacement {}/{} ──{RESET}",
                r.index + 1,
                r.total
            ));
        }

        let pair_count = r.old_lines.len().min(r.new_lines.len());
        let mut hl_old = CodeHighlighter::new(ext);
        let mut hl_new = CodeHighlighter::new(ext);

        // Paired lines: intra-line word highlighting + syntax
        for j in 0..pair_count {
            let (pre_bytes, suf_bytes_old, suf_bytes_new) =
                common_prefix_suffix(&r.old_lines[j], &r.new_lines[j]);

            let old_hl = highlight_for_diff(&mut hl_old, &r.old_lines[j]);
            let new_hl = highlight_for_diff(&mut hl_new, &r.new_lines[j]);

            let changed_end_old = r.old_lines[j].len() - suf_bytes_old;
            let changed_end_new = r.new_lines[j].len() - suf_bytes_new;

            let old_rendered =
                inject_word_highlight(&old_hl, pre_bytes, changed_end_old, LINE_RED, WORD_RED);
            let new_rendered =
                inject_word_highlight(&new_hl, pre_bytes, changed_end_new, LINE_GREEN, WORD_GREEN);

            lines.push(format!(
                "{LINE_RED}{:>4} -\t{old_rendered}{RESET}",
                r.start_line + j
            ));
            lines.push(format!(
                "{LINE_GREEN}{:>4} +\t{new_rendered}{RESET}",
                r.start_line + j
            ));
        }

        // Remaining old lines (pure deletions)
        for (j, line) in r.old_lines.iter().enumerate().skip(pair_count) {
            let hl = highlight_for_diff(&mut hl_old, line);
            lines.push(format!("{LINE_RED}{:>4} -\t{hl}{RESET}", r.start_line + j,));
        }

        // Remaining new lines (pure additions)
        for (j, line) in r.new_lines.iter().enumerate().skip(pair_count) {
            let hl = highlight_for_diff(&mut hl_new, line);
            lines.push(format!(
                "{LINE_GREEN}{:>4} +\t{hl}{RESET}",
                r.start_line + j,
            ));
        }
    }

    if edit.truncated_count > 0 {
        lines.push(format!(
            "{DIM}... and {} more replacement(s){RESET}",
            edit.truncated_count
        ));
    }

    lines.join("\n")
}

fn render_write_new(w: &WritePreview) -> String {
    let mut lines = vec![format!(
        "{DIM}New file: {} lines ({} bytes){RESET}",
        w.line_count, w.byte_count
    )];
    for line in &w.first_lines {
        lines.push(format!("{LINE_GREEN}+\t{line}{RESET}"));
    }
    if w.truncated {
        lines.push(format!(
            "{DIM}... +{} more lines{RESET}",
            w.line_count - w.first_lines.len()
        ));
    }
    lines.join("\n")
}

fn render_write_overwrite(w: &WriteOverwritePreview) -> String {
    let mut lines = vec![format!(
        "{DIM}Overwriting {} lines ({} bytes) → {} lines ({} bytes){RESET}",
        w.old_line_count, w.old_byte_count, w.new_line_count, w.new_byte_count
    )];
    for line in &w.first_lines {
        lines.push(format!("{LINE_GREEN}+\t{line}{RESET}"));
    }
    if w.truncated {
        lines.push(format!(
            "{DIM}... +{} more lines{RESET}",
            w.new_line_count - w.first_lines.len()
        ));
    }
    lines.join("\n")
}

fn render_delete_file(d: &DeleteFilePreview) -> String {
    format!(
        "{LINE_RED}Removing {} lines ({} bytes){RESET}",
        d.line_count, d.byte_count
    )
}

fn render_delete_dir(d: &DeleteDirPreview) -> String {
    if d.recursive {
        format!("{LINE_RED}Removing directory and all contents{RESET}")
    } else {
        format!("{LINE_RED}Removing empty directory{RESET}")
    }
}

// ── Syntax highlighting helpers ───────────────────────────────

/// Syntax-highlight a line for diff display.
///
/// Strips the trailing `\x1b[0m` that [`CodeHighlighter::highlight_line`]
/// appends, because the caller manages reset/background itself.
fn highlight_for_diff(hl: &mut CodeHighlighter, line: &str) -> String {
    let output = hl.highlight_line(line);
    output
        .strip_suffix("\x1b[0m")
        .unwrap_or(&output)
        .to_string()
}

// ── Intra-line word diff ──────────────────────────────────────

/// Walk `syntect_output` (foreground ANSI codes interleaved with raw text)
/// and inject word-level background at the raw byte offsets
/// `changed_start..changed_end`.
///
/// syntect only emits `\x1b[38;2;…m` (foreground), so our background
/// codes persist through them. We switch from `line_bg` to `word_bg` at
/// the changed region and back.
fn inject_word_highlight(
    syntect_output: &str,
    changed_start: usize,
    changed_end: usize,
    line_bg: &str,
    word_bg: &str,
) -> String {
    if changed_start >= changed_end {
        return syntect_output.to_string();
    }

    let mut result = String::with_capacity(syntect_output.len() + 64);
    let mut raw_bytes = 0usize;
    let mut in_word = false;
    let mut chars = syntect_output.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            // Consume the ANSI escape sequence without advancing raw_bytes
            result.push(ch);
            for ec in chars.by_ref() {
                result.push(ec);
                if ec == 'm' {
                    break;
                }
            }
        } else {
            if !in_word && raw_bytes == changed_start {
                result.push_str(word_bg);
                in_word = true;
            }
            if in_word && raw_bytes == changed_end {
                result.push_str(line_bg);
                in_word = false;
            }
            result.push(ch);
            raw_bytes += ch.len_utf8();
        }
    }

    if in_word {
        result.push_str(line_bg);
    }

    result
}

/// Find byte lengths of the common prefix and suffix between two strings.
///
/// Returns `(prefix_bytes, suffix_bytes_old, suffix_bytes_new)`.
fn common_prefix_suffix(old: &str, new: &str) -> (usize, usize, usize) {
    let prefix_chars = old
        .chars()
        .zip(new.chars())
        .take_while(|(a, b)| a == b)
        .count();

    let prefix_bytes = old
        .char_indices()
        .nth(prefix_chars)
        .map(|(i, _)| i)
        .unwrap_or(old.len());

    let old_rest: Vec<char> = old[prefix_bytes..].chars().collect();
    let new_rest: Vec<char> = new[prefix_bytes..].chars().collect();

    let suffix_chars = old_rest
        .iter()
        .rev()
        .zip(new_rest.iter().rev())
        .take_while(|(a, b)| a == b)
        .count();

    let suffix_bytes_old: usize = old_rest
        .iter()
        .rev()
        .take(suffix_chars)
        .map(|c| c.len_utf8())
        .sum();
    let suffix_bytes_new: usize = new_rest
        .iter()
        .rev()
        .take(suffix_chars)
        .map(|c| c.len_utf8())
        .sum();

    (prefix_bytes, suffix_bytes_old, suffix_bytes_new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use koda_core::preview::{EditPreview, ReplacementPreview};

    #[test]
    fn test_render_edit_has_syntax_and_word_highlight() {
        let preview = DiffPreview::Edit(EditPreview {
            path: "test.rs".into(),
            replacements: vec![ReplacementPreview {
                index: 0,
                total: 1,
                start_line: 2,
                old_lines: vec!["println!(\"hello\");".into()],
                new_lines: vec!["println!(\"world\");".into()],
            }],
            truncated_count: 0,
        });
        let rendered = render(&preview);

        // Line numbers present
        assert!(
            rendered.contains("2 -\t"),
            "should have line number for removed"
        );
        assert!(
            rendered.contains("2 +\t"),
            "should have line number for added"
        );

        // Line-level backgrounds
        assert!(rendered.contains(LINE_RED), "removed line background");
        assert!(rendered.contains(LINE_GREEN), "added line background");

        // Word-level highlights for the changed region
        assert!(
            rendered.contains(WORD_RED),
            "changed word highlight in removed"
        );
        assert!(
            rendered.contains(WORD_GREEN),
            "changed word highlight in added"
        );

        // Syntax highlighting: .rs extension → syntect should add foreground codes
        assert!(
            rendered.contains("\x1b[38;2;"),
            "should have syntect foreground colors"
        );
    }

    #[test]
    fn test_inject_word_highlight_no_change() {
        let result = inject_word_highlight("hello world", 5, 5, LINE_RED, WORD_RED);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_inject_word_highlight_with_change() {
        let result = inject_word_highlight("hello world", 0, 5, LINE_RED, WORD_RED);
        assert!(result.contains(WORD_RED));
        assert!(result.contains(LINE_RED));
        assert!(result.contains("hello"));
    }

    #[test]
    fn test_common_prefix_suffix_basic() {
        let (pre, suf_old, suf_new) = common_prefix_suffix("hello world", "hello earth");
        assert_eq!(pre, 6); // "hello " is common
        assert_eq!(suf_old, 0);
        assert_eq!(suf_new, 0);
    }

    #[test]
    fn test_common_prefix_suffix_with_suffix() {
        let (pre, suf_old, suf_new) =
            common_prefix_suffix("println!(\"hello\");", "println!(\"world\");");
        // Common prefix: "println!("
        assert_eq!(pre, 10);
        // Common suffix: ");"  — wait, actually `");` (3 bytes)
        assert_eq!(suf_old, 3);
        assert_eq!(suf_new, 3);
    }
}
