//! A tool call's file changes as ratatui `Line`s.
//!
//! Drawn from the `changes` the protocol carries on an approval request and
//! on a tool-call item (`FileUpdateChange`): a unified diff for an edit, the
//! file itself for a new one. Codex's TUI draws its `fileChange` the same
//! way, and so do these:
//! - the unified text is read with `diffy`;
//! - each hunk is highlighted as one block, so a construct that spans its
//!   lines colors correctly, with no state carried from hunk to hunk;
//! - the whole diff is shown, never cut at a line count;
//! - a change too large to highlight cheaply is drawn plain
//!   ([`highlight::pre_highlight`]'s limits, Codex's).
//!
//! Each diff line's gutter (line number and sign) is [`GUTTER_WIDTH`]
//! columns wide, which the scroll buffer leaves out when text is copied.

use crate::highlight;
use koda_client::protocol::item::{FileUpdateChange, PatchChangeKind};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

// ── Styles ────────────────────────────────────────────────────

const LINE_RED_BG: Color = Color::Rgb(50, 0, 0);
const LINE_GREEN_BG: Color = Color::Rgb(0, 35, 0);
const DIM: Style = Style::new().fg(Color::DarkGray);
const HUNK_HEADER: Style = Style::new().fg(Color::Cyan);

/// Width of the gutter: 4-digit line number + space + sigil + space = 7.
/// Used by NoSelect to know how many leading columns to skip on copy.
pub const GUTTER_WIDTH: u16 = 7;

/// What a diff line does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sign {
    Context,
    Delete,
    Insert,
}

// ── Public API ────────────────────────────────────────────────

/// Draw a call's changes, one file after another.
pub fn render_changes(changes: &[FileUpdateChange]) -> Vec<Line<'static>> {
    changes.iter().flat_map(render_change).collect()
}

fn render_change(change: &FileUpdateChange) -> Vec<Line<'static>> {
    match &change.kind {
        PatchChangeKind::Add => render_whole_file(&change.path, &change.diff, Sign::Insert),
        PatchChangeKind::Delete => render_whole_file(&change.path, &change.diff, Sign::Delete),
        PatchChangeKind::Update { move_path } => {
            let path = match move_path {
                Some(to) => format!("{} \u{2192} {to}", change.path),
                None => change.path.clone(),
            };
            // The language comes from the name the file ends up with.
            let ext = extension(move_path.as_deref().unwrap_or(&change.path));
            render_unified(&path, ext, &change.diff)
        }
    }
}

/// A file's extension, for the highlighter.
fn extension(path: &str) -> &str {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
}

// ── Unified diff ──────────────────────────────────────────────

fn render_unified(path: &str, ext: &str, diff: &str) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled(format!("╭─── {path} ───╮"), DIM)];
    match diffy::Patch::from_str(diff) {
        Ok(patch) => {
            for (i, hunk) in patch.hunks().iter().enumerate() {
                // Between hunks, not before the first.
                if i > 0 {
                    lines.push(Line::styled("  ⋯", DIM));
                }
                let (old, new) = (hunk.old_range(), hunk.new_range());
                lines.push(Line::styled(
                    format!(
                        "@@ -{},{} +{},{} @@",
                        old.start(),
                        old.len(),
                        new.start(),
                        new.len()
                    ),
                    HUNK_HEADER,
                ));
                let parts: Vec<(Sign, &str)> = hunk
                    .lines()
                    .iter()
                    .map(|line| match line {
                        diffy::Line::Context(text) => (Sign::Context, *text),
                        diffy::Line::Delete(text) => (Sign::Delete, *text),
                        diffy::Line::Insert(text) => (Sign::Insert, *text),
                    })
                    .collect();
                // The hunk highlighted as one block, as Codex does.
                let block: String = parts.iter().map(|(_, text)| *text).collect();
                let spans = highlight::pre_highlight(&block, ext);
                let spans = (spans.len() == parts.len()).then_some(spans);
                let (mut old_line, mut new_line) = (old.start(), new.start());
                for (n, (sign, text)) in parts.iter().enumerate() {
                    let number = match sign {
                        Sign::Context => {
                            old_line += 1;
                            new_line += 1;
                            old_line - 1
                        }
                        Sign::Delete => {
                            old_line += 1;
                            old_line - 1
                        }
                        Sign::Insert => {
                            new_line += 1;
                            new_line - 1
                        }
                    };
                    let text = text.trim_end_matches('\n');
                    lines.push(diff_line(
                        number,
                        *sign,
                        text,
                        spans.as_ref().map(|s| s[n].as_slice()),
                    ));
                }
            }
        }
        // Not a diff diffy can read: its lines as they are, colored by sign.
        Err(_) => {
            for text in diff.lines() {
                let style = match text.chars().next() {
                    Some('+') => Style::default().fg(Color::Green),
                    Some('-') => Style::default().fg(Color::Red),
                    Some('@') => HUNK_HEADER,
                    _ => Style::default(),
                };
                lines.push(Line::styled(text.to_string(), style));
            }
        }
    }
    lines.push(Line::styled(format!("╰─── {path} ───╯"), DIM));
    lines
}

// ── A whole file, added or deleted ────────────────────────────

fn render_whole_file(path: &str, content: &str, sign: Sign) -> Vec<Line<'static>> {
    let count = content.lines().count();
    let what = if sign == Sign::Insert {
        "new file"
    } else {
        "deleted"
    };
    let mut lines = vec![Line::styled(
        format!(
            "╭─── {path} ({what}: {count} {}, {} bytes) ───╮",
            if count == 1 { "line" } else { "lines" },
            content.len()
        ),
        DIM,
    )];
    // The whole file highlighted at once, as Codex does.
    let spans = highlight::pre_highlight(content, extension(path));
    for (i, text) in content.lines().enumerate() {
        lines.push(diff_line(
            i + 1,
            sign,
            text,
            spans.get(i).map(|s| s.as_slice()),
        ));
    }
    lines.push(Line::styled(format!("╰─── {path} ───╯"), DIM));
    lines
}

/// One diff line: the gutter (number and sign), then the text, highlighted
/// when there are spans for it, on a red or green ground.
fn diff_line(
    number: usize,
    sign: Sign,
    text: &str,
    highlighted: Option<&[Span<'static>]>,
) -> Line<'static> {
    let (sigil, sigil_color, bg) = match sign {
        Sign::Context => (' ', Color::DarkGray, None),
        Sign::Delete => ('-', Color::Red, Some(LINE_RED_BG)),
        Sign::Insert => ('+', Color::Green, Some(LINE_GREEN_BG)),
    };
    let mut spans = vec![Span::styled(
        format!("{number:>4} {sigil} "),
        Style::default().fg(sigil_color).add_modifier(Modifier::DIM),
    )];
    let ground = |style: Style| match bg {
        Some(bg) => style.bg(bg),
        None => style,
    };
    match highlighted {
        Some(highlighted) => spans.extend(
            highlighted
                .iter()
                .map(|s| Span::styled(s.content.clone(), ground(s.style))),
        ),
        None => spans.push(Span::styled(text.to_string(), ground(Style::default()))),
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn update(path: &str, diff: &str) -> FileUpdateChange {
        FileUpdateChange {
            path: path.into(),
            kind: PatchChangeKind::Update { move_path: None },
            diff: diff.into(),
        }
    }

    #[test]
    fn an_edit_shows_its_hunk_with_line_numbers() {
        // What `similar` writes, as the host sends it: no file headers.
        let diff = "@@ -1,3 +1,3 @@\n fn main() {\n-    println!(\"hello\");\n+    println!(\"world\");\n }\n";
        let lines = text(&render_changes(&[update("test.rs", diff)]));
        assert!(lines[0].contains("test.rs"), "header: {}", lines[0]);
        assert_eq!(lines[1], "@@ -1,3 +1,3 @@");
        assert!(lines[2].starts_with("   1   fn main()"), "{}", lines[2]);
        assert!(lines[3].starts_with("   2 - "), "{}", lines[3]);
        assert!(lines[3].contains("hello"));
        assert!(lines[4].starts_with("   2 + "), "{}", lines[4]);
        assert!(lines[4].contains("world"));
        assert!(lines[5].starts_with("   3   }"), "{}", lines[5]);
        assert!(lines[6].contains("test.rs"));
    }

    #[test]
    fn hunks_are_separated_and_the_whole_diff_is_shown() {
        let mut diff = String::from("@@ -1,1 +1,1 @@\n-a\n+b\n@@ -50,200 +50,200 @@\n");
        for i in 0..200 {
            diff.push_str(&format!("-old {i}\n"));
        }
        for i in 0..200 {
            diff.push_str(&format!("+new {i}\n"));
        }
        let lines = text(&render_changes(&[update("big.txt", &diff)]));
        assert!(lines.iter().any(|l| l.contains('⋯')), "hunk separator");
        assert!(
            lines.iter().any(|l| l.contains("new 199")),
            "nothing is cut: {}",
            lines.len()
        );
        assert!(lines.iter().any(|l| l.starts_with(" 249 + new 199")));
    }

    #[test]
    fn a_new_file_shows_every_line() {
        let content: String = (1..=100).map(|i| format!("line {i}\n")).collect();
        let lines = text(&render_changes(&[FileUpdateChange {
            path: "new.txt".into(),
            kind: PatchChangeKind::Add,
            diff: content,
        }]));
        assert!(
            lines[0].contains("new.txt (new file: 100 lines"),
            "{}",
            lines[0]
        );
        assert!(lines[100].starts_with(" 100 + line 100"), "{}", lines[100]);
        assert!(lines[101].contains("new.txt"));
    }

    #[test]
    fn a_moved_file_names_both_paths() {
        let change = FileUpdateChange {
            path: "a.rs".into(),
            kind: PatchChangeKind::Update {
                move_path: Some("b.rs".into()),
            },
            diff: "@@ -1,1 +1,1 @@\n-x\n+y\n".into(),
        };
        let lines = text(&render_changes(&[change]));
        assert!(lines[0].contains("a.rs \u{2192} b.rs"), "{}", lines[0]);
    }

    #[test]
    fn text_diffy_cannot_read_is_shown_as_it_is() {
        let lines = text(&render_changes(&[update("x", "@@ nonsense\n+added\n")]));
        assert!(lines.iter().any(|l| l == "+added"), "{lines:?}");
    }
}
