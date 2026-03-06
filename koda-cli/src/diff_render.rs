//! Diff preview renderer — native ratatui `Line`/`Span` output.
//!
//! Takes structured [`DiffPreview`] data from koda-core and produces
//! `Vec<Line<'static>>` with:
//! - Dark background tint for line-level diffs (red removed, green added)
//! - Syntax highlighting (foreground colors via syntect)

use crate::highlight::CodeHighlighter;
use koda_core::preview::{
    DeleteDirPreview, DeleteFilePreview, DiffPreview, EditPreview, WriteOverwritePreview,
    WritePreview,
};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

// Background styles for diff lines
const LINE_RED_BG: Style = Style::new().bg(Color::Rgb(50, 0, 0));
const LINE_GREEN_BG: Style = Style::new().bg(Color::Rgb(0, 35, 0));
const DIM: Style = Style::new().fg(Color::DarkGray);

/// Render a [`DiffPreview`] as native ratatui `Line`s.
pub fn render_lines(preview: &DiffPreview) -> Vec<Line<'static>> {
    match preview {
        DiffPreview::Edit(edit) => render_edit(edit),
        DiffPreview::WriteNew(w) => render_write_new(w),
        DiffPreview::WriteOverwrite(w) => render_write_overwrite(w),
        DiffPreview::DeleteFile(d) => render_delete_file(d),
        DiffPreview::DeleteDir(d) => render_delete_dir(d),
        DiffPreview::FileNotYetExists => {
            vec![Line::styled("(file does not exist yet)", DIM)]
        }
        DiffPreview::PathNotFound => {
            vec![Line::styled("(path does not exist)", DIM)]
        }
    }
}

/// Legacy ANSI render — kept for `app.rs` / `confirm.rs` (legacy mode).
pub fn render(preview: &DiffPreview) -> String {
    // Delegate to the old ANSI rendering for backward compat
    render_ansi(preview)
}

fn render_edit(edit: &EditPreview) -> Vec<Line<'static>> {
    let ext = std::path::Path::new(&edit.path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    let mut lines = Vec::new();

    for r in &edit.replacements {
        if r.total > 1 {
            lines.push(Line::styled(
                format!(
                    "\u{2500}\u{2500} replacement {}/{} \u{2500}\u{2500}",
                    r.index + 1,
                    r.total
                ),
                DIM,
            ));
        }

        let mut hl_old = CodeHighlighter::new(ext);
        let mut hl_new = CodeHighlighter::new(ext);

        for (j, old_line) in r.old_lines.iter().enumerate() {
            let mut spans = vec![Span::styled(
                format!("{:>4} - ", r.start_line + j),
                Style::default().fg(Color::Red).add_modifier(Modifier::DIM),
            )];
            let highlighted = hl_old.highlight_spans(old_line);
            for mut s in highlighted {
                s.style = s.style.bg(Color::Rgb(50, 0, 0));
                spans.push(s);
            }
            lines.push(Line::from(spans));
        }

        for (j, new_line) in r.new_lines.iter().enumerate() {
            let mut spans = vec![Span::styled(
                format!("{:>4} + ", r.start_line + j),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::DIM),
            )];
            let highlighted = hl_new.highlight_spans(new_line);
            for mut s in highlighted {
                s.style = s.style.bg(Color::Rgb(0, 35, 0));
                spans.push(s);
            }
            lines.push(Line::from(spans));
        }
    }

    if edit.truncated_count > 0 {
        lines.push(Line::styled(
            format!("... and {} more replacement(s)", edit.truncated_count),
            DIM,
        ));
    }

    lines
}

fn render_write_new(w: &WritePreview) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled(
        format!("New file: {} lines ({} bytes)", w.line_count, w.byte_count),
        DIM,
    )];
    for line in &w.first_lines {
        lines.push(Line::from(vec![
            Span::styled("+ ", Style::default().fg(Color::Green)),
            Span::styled(line.clone(), LINE_GREEN_BG),
        ]));
    }
    if w.truncated {
        lines.push(Line::styled(
            format!("... +{} more lines", w.line_count - w.first_lines.len()),
            DIM,
        ));
    }
    lines
}

fn render_write_overwrite(w: &WriteOverwritePreview) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled(
        format!(
            "Overwriting {} lines ({} bytes) \u{2192} {} lines ({} bytes)",
            w.old_line_count, w.old_byte_count, w.new_line_count, w.new_byte_count
        ),
        DIM,
    )];
    for line in &w.first_lines {
        lines.push(Line::from(vec![
            Span::styled("+ ", Style::default().fg(Color::Green)),
            Span::styled(line.clone(), LINE_GREEN_BG),
        ]));
    }
    if w.truncated {
        lines.push(Line::styled(
            format!("... +{} more lines", w.new_line_count - w.first_lines.len()),
            DIM,
        ));
    }
    lines
}

fn render_delete_file(d: &DeleteFilePreview) -> Vec<Line<'static>> {
    vec![Line::styled(
        format!("Removing {} lines ({} bytes)", d.line_count, d.byte_count),
        LINE_RED_BG,
    )]
}

fn render_delete_dir(d: &DeleteDirPreview) -> Vec<Line<'static>> {
    if d.recursive {
        vec![Line::styled(
            "Removing directory and all contents",
            LINE_RED_BG,
        )]
    } else {
        vec![Line::styled("Removing empty directory", LINE_RED_BG)]
    }
}

// ── Legacy ANSI rendering (kept for app.rs / confirm.rs) ────────

const ANSI_LINE_RED: &str = "\x1b[48;2;50;0;0m";
const ANSI_LINE_GREEN: &str = "\x1b[48;2;0;35;0m";
const ANSI_DIM: &str = "\x1b[90m";
const ANSI_RESET: &str = "\x1b[0m";

fn render_ansi(preview: &DiffPreview) -> String {
    match preview {
        DiffPreview::Edit(edit) => {
            let ext = std::path::Path::new(&edit.path)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            let mut lines = Vec::new();
            for r in &edit.replacements {
                if r.total > 1 {
                    lines.push(format!(
                        "{ANSI_DIM}\u{2500}\u{2500} replacement {}/{} \u{2500}\u{2500}{ANSI_RESET}",
                        r.index + 1,
                        r.total
                    ));
                }
                let mut hl = CodeHighlighter::new(ext);
                for (j, line) in r.old_lines.iter().enumerate() {
                    let h = hl.highlight_line(line).replace("\x1b[0m", "");
                    lines.push(format!(
                        "{ANSI_LINE_RED}{:>4} -\t{h}{ANSI_RESET}",
                        r.start_line + j
                    ));
                }
                let mut hl = CodeHighlighter::new(ext);
                for (j, line) in r.new_lines.iter().enumerate() {
                    let h = hl.highlight_line(line).replace("\x1b[0m", "");
                    lines.push(format!(
                        "{ANSI_LINE_GREEN}{:>4} +\t{h}{ANSI_RESET}",
                        r.start_line + j
                    ));
                }
            }
            if edit.truncated_count > 0 {
                lines.push(format!(
                    "{ANSI_DIM}... and {} more replacement(s){ANSI_RESET}",
                    edit.truncated_count
                ));
            }
            lines.join("\n")
        }
        DiffPreview::WriteNew(w) => {
            let mut lines = vec![format!(
                "{ANSI_DIM}New file: {} lines ({} bytes){ANSI_RESET}",
                w.line_count, w.byte_count
            )];
            for line in &w.first_lines {
                lines.push(format!("{ANSI_LINE_GREEN}+\t{line}{ANSI_RESET}"));
            }
            if w.truncated {
                lines.push(format!(
                    "{ANSI_DIM}... +{} more lines{ANSI_RESET}",
                    w.line_count - w.first_lines.len()
                ));
            }
            lines.join("\n")
        }
        DiffPreview::WriteOverwrite(w) => {
            let mut lines = vec![format!(
                "{ANSI_DIM}Overwriting {} lines → {} lines{ANSI_RESET}",
                w.old_line_count, w.new_line_count
            )];
            for line in &w.first_lines {
                lines.push(format!("{ANSI_LINE_GREEN}+\t{line}{ANSI_RESET}"));
            }
            if w.truncated {
                lines.push(format!(
                    "{ANSI_DIM}... +{} more lines{ANSI_RESET}",
                    w.new_line_count - w.first_lines.len()
                ));
            }
            lines.join("\n")
        }
        DiffPreview::DeleteFile(d) => format!(
            "{ANSI_LINE_RED}Removing {} lines ({} bytes){ANSI_RESET}",
            d.line_count, d.byte_count
        ),
        DiffPreview::DeleteDir(d) => {
            if d.recursive {
                format!("{ANSI_LINE_RED}Removing directory and all contents{ANSI_RESET}")
            } else {
                format!("{ANSI_LINE_RED}Removing empty directory{ANSI_RESET}")
            }
        }
        DiffPreview::FileNotYetExists => format!("{ANSI_DIM}(file does not exist yet){ANSI_RESET}"),
        DiffPreview::PathNotFound => format!("{ANSI_DIM}(path does not exist){ANSI_RESET}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use koda_core::preview::ReplacementPreview;

    #[test]
    fn test_render_lines_edit_has_line_numbers() {
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
        let lines = render_lines(&preview);
        assert_eq!(lines.len(), 2); // one removed + one added
        // Check line numbers in first span
        let first_span = &lines[0].spans[0];
        assert!(first_span.content.contains("2 -"));
        let second_span = &lines[1].spans[0];
        assert!(second_span.content.contains("2 +"));
    }

    #[test]
    fn test_render_lines_write_new() {
        let preview = DiffPreview::WriteNew(WritePreview {
            line_count: 10,
            byte_count: 200,
            first_lines: vec!["line 1".into(), "line 2".into()],
            truncated: true,
        });
        let lines = render_lines(&preview);
        assert!(lines.len() >= 3); // header + 2 lines + truncation
    }

    #[test]
    fn test_legacy_render_still_works() {
        let preview = DiffPreview::DeleteFile(DeleteFilePreview {
            line_count: 5,
            byte_count: 100,
        });
        let ansi = render(&preview);
        assert!(ansi.contains("\x1b["));
        assert!(ansi.contains("Removing"));
    }
}
