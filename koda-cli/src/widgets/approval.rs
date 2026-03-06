//! Inline approval widget for the TUI.
//!
//! Renders approval prompt + options above the viewport using
//! `insert_before()`, then handles key events in a sub-loop.
//! Stays in raw mode the entire time — no mode switching.

use crate::tui_output;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use koda_core::engine::ApprovalDecision;
use koda_core::preview::DiffPreview;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use std::io::Write;

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

/// Run an inline approval prompt. Returns the user's decision.
///
/// Renders the prompt above the viewport and handles arrow-key
/// navigation entirely within raw mode.
pub fn prompt_approval(
    terminal: &mut Term,
    _tool_name: &str,
    detail: &str,
    preview: Option<&DiffPreview>,
    whitelist_hint: Option<&str>,
) -> ApprovalDecision {
    // Show detail above viewport
    tui_output::emit_line(
        terminal,
        Line::from(vec![
            Span::raw("  "),
            Span::styled(detail.to_string(), Style::default().fg(Color::DarkGray)),
        ]),
    );

    // Show diff preview if available
    if let Some(preview) = preview {
        tui_output::emit_blank(terminal);
        let rendered = crate::diff_render::render(preview);
        for line in rendered.lines() {
            tui_output::emit_line(
                terminal,
                Line::from(vec![Span::raw("  "), Span::raw(line.to_string())]),
            );
        }
    }
    tui_output::emit_blank(terminal);

    // Build options
    let mut options = vec![
        ("\u{2713} Approve", "Execute this action"),
        ("\u{2717} Reject", "Skip this action"),
        ("\u{1f4ac} Feedback", "Reject and tell koda what to change"),
    ];
    if whitelist_hint.is_some() {
        options.push(("\u{1f513} Always allow", "Auto-approve from now on"));
    }

    // Render options and run selection loop
    let mut selected: usize = 0;
    render_options(terminal, &options, selected);

    loop {
        if let Ok(Event::Key(key)) = event::read() {
            match (key.code, key.modifiers) {
                (KeyCode::Up, _) => {
                    selected = selected.saturating_sub(1);
                    render_options(terminal, &options, selected);
                }
                (KeyCode::Down, _) => {
                    if selected + 1 < options.len() {
                        selected += 1;
                    }
                    render_options(terminal, &options, selected);
                }
                (KeyCode::Enter, _) => {
                    clear_options(terminal, &options);
                    return match selected {
                        0 => ApprovalDecision::Approve,
                        1 => ApprovalDecision::Reject,
                        2 => {
                            // Feedback: get text from user inline
                            let feedback = read_feedback_inline(terminal);
                            if feedback.is_empty() {
                                ApprovalDecision::Reject
                            } else {
                                ApprovalDecision::RejectWithFeedback { feedback }
                            }
                        }
                        3 => ApprovalDecision::AlwaysAllow,
                        _ => ApprovalDecision::Reject,
                    };
                }
                (KeyCode::Esc, _) => {
                    clear_options(terminal, &options);
                    return ApprovalDecision::Reject;
                }
                (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                    clear_options(terminal, &options);
                    return ApprovalDecision::Reject;
                }
                _ => {}
            }
        }
    }
}

fn render_options(_terminal: &mut Term, options: &[(&str, &str)], selected: usize) {
    // We re-render by clearing previous options and redrawing.
    // Since insert_before scrolls content up, we use a simple approach:
    // render all options as a batch via insert_before on first call,
    // then update by clearing and re-inserting.
    //
    // For simplicity, we render the options + hint as a single insert_before block.
    let total_lines = options.len() + 2; // options + title + hint
    let mut lines = Vec::with_capacity(total_lines);

    lines.push(Line::from(vec![Span::styled(
        "  \u{1f43b} Confirm action?",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )]));

    for (i, (label, desc)) in options.iter().enumerate() {
        if i == selected {
            lines.push(Line::from(vec![
                Span::styled("  \u{203a} ", Style::default().fg(Color::Cyan)),
                Span::styled(
                    label.to_string(),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  {desc}"), Style::default().fg(Color::Cyan)),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(label.to_string(), Style::default().fg(Color::DarkGray)),
                Span::styled(format!("  {desc}"), Style::default().fg(Color::DarkGray)),
            ]));
        }
    }

    lines.push(Line::styled(
        "  \u{2191}/\u{2193} navigate  enter select  esc cancel",
        Style::default().fg(Color::DarkGray),
    ));

    // Clear previous render and redraw
    // Move cursor up by total_lines, clear, then insert
    let height = lines.len() as u16;
    // Use crossterm to clear the area first
    use crossterm::{cursor, execute, terminal::Clear, terminal::ClearType};
    let mut stdout = std::io::stdout();
    // We can't easily clear previous insert_before content.
    // Instead, use a simpler approach: render into the viewport area temporarily
    // by clearing existing option lines and re-inserting.
    //
    // Workaround: On first render, insert. On re-render, move up and overwrite.
    // For simplicity, we'll just always insert fresh lines.
    // This means each key press adds more lines above — not ideal.
    // Better approach: use the terminal's cursor to overwrite in place.

    // Simple approach: overwrite in place using cursor movement
    for line in &lines {
        let rendered = line
            .spans
            .iter()
            .map(|s| format_span(s))
            .collect::<String>();
        execute!(stdout, Clear(ClearType::CurrentLine)).ok();
        write!(stdout, "\r{rendered}").ok();
        execute!(stdout, cursor::MoveDown(1)).ok();
    }
    // Move back up to start position
    execute!(stdout, cursor::MoveUp(height)).ok();
    stdout.flush().ok();
}

fn clear_options(terminal: &mut Term, options: &[(&str, &str)]) {
    use crossterm::{cursor, execute, terminal::Clear, terminal::ClearType};
    let mut stdout = std::io::stdout();
    let total_lines = options.len() + 2;
    for _ in 0..total_lines {
        execute!(stdout, Clear(ClearType::CurrentLine), cursor::MoveDown(1)).ok();
    }
    execute!(stdout, cursor::MoveUp(total_lines as u16)).ok();
    stdout.flush().ok();
    // Also insert blank lines to push content into scrollback properly
    tui_output::emit_blank(terminal);
}

/// Format a Span into ANSI for direct stdout writing.
fn format_span(span: &Span) -> String {
    let mut result = String::new();
    let style = span.style;

    if style.add_modifier.contains(Modifier::BOLD) {
        result.push_str("\x1b[1m");
    }

    if let Some(fg) = style.fg {
        match fg {
            Color::Cyan => result.push_str("\x1b[36m"),
            Color::DarkGray => result.push_str("\x1b[90m"),
            Color::Red => result.push_str("\x1b[31m"),
            Color::Green => result.push_str("\x1b[32m"),
            Color::Yellow => result.push_str("\x1b[33m"),
            _ => {}
        }
    }

    result.push_str(&span.content);

    if style.fg.is_some() || !style.add_modifier.is_empty() {
        result.push_str("\x1b[0m");
    }

    result
}

/// Read feedback text inline (in raw mode).
fn read_feedback_inline(terminal: &mut Term) -> String {
    tui_output::emit_line(
        terminal,
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "\u{276f} Tell koda what to change: ",
                Style::default().fg(Color::Green),
            ),
        ]),
    );

    let mut buf = String::new();
    let mut stdout = std::io::stdout();
    write!(stdout, "\r  ").ok();
    stdout.flush().ok();

    loop {
        if let Ok(Event::Key(key)) = event::read() {
            match key.code {
                KeyCode::Enter => break,
                KeyCode::Esc => {
                    buf.clear();
                    break;
                }
                KeyCode::Backspace => {
                    if buf.pop().is_some() {
                        write!(stdout, "\x1b[D \x1b[D").ok();
                        stdout.flush().ok();
                    }
                }
                KeyCode::Char(c) => {
                    buf.push(c);
                    write!(stdout, "{c}").ok();
                    stdout.flush().ok();
                }
                _ => {}
            }
        }
    }
    write!(stdout, "\r\n").ok();
    stdout.flush().ok();
    buf.trim().to_string()
}
