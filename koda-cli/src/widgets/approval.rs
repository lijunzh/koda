//! Inline approval widget for the TUI.
//!
//! Renders approval prompt + options above the viewport using
//! `insert_before()` for permanent content, then crossterm styled
//! output for the in-place selection menu.
//! Stays in raw mode the entire time — no mode switching.

use crate::tui_output;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use koda_core::engine::ApprovalDecision;
use koda_core::preview::DiffPreview;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    style::{Color, Style},
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
        let diff_lines = crate::diff_render::render_lines(preview);
        tui_output::emit_lines(terminal, &diff_lines);
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
    use crossterm::{
        cursor, execute,
        style::{Attribute, Color as CColor, Print, ResetColor, SetAttribute, SetForegroundColor},
        terminal::{Clear, ClearType},
    };
    let mut stdout = std::io::stdout();
    let height = (options.len() + 2) as u16; // title + options + hint

    // Title
    execute!(
        stdout,
        Clear(ClearType::CurrentLine),
        Print("\r  "),
        SetForegroundColor(CColor::Cyan),
        SetAttribute(Attribute::Bold),
        Print("\u{1f43b} Confirm action?"),
        SetAttribute(Attribute::Reset),
        cursor::MoveDown(1),
    )
    .ok();

    // Options
    for (i, (label, desc)) in options.iter().enumerate() {
        execute!(stdout, Clear(ClearType::CurrentLine)).ok();
        if i == selected {
            execute!(
                stdout,
                Print("\r  "),
                SetForegroundColor(CColor::Cyan),
                Print("\u{203a} "),
                SetAttribute(Attribute::Bold),
                Print(label),
                SetAttribute(Attribute::NoBold),
                Print(format!("  {desc}")),
                ResetColor,
                cursor::MoveDown(1),
            )
            .ok();
        } else {
            execute!(
                stdout,
                Print("\r    "),
                SetForegroundColor(CColor::DarkGrey),
                Print(format!("{label}  {desc}")),
                ResetColor,
                cursor::MoveDown(1),
            )
            .ok();
        }
    }

    // Hint
    execute!(
        stdout,
        Clear(ClearType::CurrentLine),
        Print("\r  "),
        SetForegroundColor(CColor::DarkGrey),
        Print("\u{2191}/\u{2193} navigate  enter select  esc cancel"),
        ResetColor,
        cursor::MoveUp(height),
    )
    .ok();
    stdout.flush().ok();
}

fn clear_options(_terminal: &mut Term, options: &[(&str, &str)]) {
    use crossterm::{
        cursor, execute,
        terminal::{Clear, ClearType},
    };
    let mut stdout = std::io::stdout();
    let total_lines = options.len() + 2;
    for _ in 0..total_lines {
        execute!(stdout, Clear(ClearType::CurrentLine), cursor::MoveDown(1)).ok();
    }
    execute!(stdout, cursor::MoveUp(total_lines as u16)).ok();
    stdout.flush().ok();
    // Use write_blank (crossterm) not emit_blank (ratatui) since we're
    // in the crossterm rendering path for the options menu.
    tui_output::write_blank();
}

/// Read feedback text inline (in raw mode).
fn read_feedback_inline(terminal: &mut Term) -> String {
    use crossterm::{cursor, style::Print};

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
    crossterm::execute!(stdout, Print("\r  ")).ok();
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
                        crossterm::execute!(
                            stdout,
                            cursor::MoveLeft(1),
                            Print(" "),
                            cursor::MoveLeft(1),
                        )
                        .ok();
                    }
                }
                KeyCode::Char(c) => {
                    buf.push(c);
                    crossterm::execute!(stdout, Print(c.to_string())).ok();
                }
                _ => {}
            }
        }
    }
    crossterm::execute!(stdout, Print("\r\n")).ok();
    stdout.flush().ok();
    buf.trim().to_string()
}
