//! Inline approval widget for the TUI.
//!
//! Renders the entire approval prompt using crossterm direct writes
//! (`write_line()`), avoiding ratatui `insert_before()`. This keeps
//! cursor tracking consistent so the interactive options menu renders
//! at the correct position. The caller must reinitialize the terminal
//! after this widget returns to resync ratatui's viewport.
//!
//! Stays in raw mode the entire time — no mode switching.

use crate::tui_output;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use koda_core::engine::ApprovalDecision;
use koda_core::preview::DiffPreview;
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use std::io::Write;

/// Run an inline approval prompt. Returns the user's decision.
///
/// Uses crossterm direct writes for all output (detail, diff, options).
/// The caller must reinitialize the terminal afterwards to resync
/// ratatui's viewport tracking.
pub fn prompt_approval(
    _tool_name: &str,
    detail: &str,
    preview: Option<&DiffPreview>,
    whitelist_hint: Option<&str>,
) -> ApprovalDecision {
    // Show detail via crossterm (not ratatui insert_before)
    tui_output::write_line(&Line::from(vec![
        Span::raw("  "),
        Span::styled(detail.to_string(), Style::default().fg(Color::DarkGray)),
    ]));

    // Show diff preview if available
    if let Some(preview) = preview {
        tui_output::write_blank();
        let diff_lines = crate::diff_render::render_lines(preview);
        for line in &diff_lines {
            tui_output::write_line(line);
        }
    }
    tui_output::write_blank();

    // Build options
    let mut options = vec![
        ("\u{2713} Approve", "Execute this action"),
        ("\u{2717} Reject", "Skip this action"),
        ("\u{1f4ac} Feedback", "Reject and tell koda what to change"),
    ];
    if whitelist_hint.is_some() {
        options.push(("\u{1f513} Always allow", "Auto-approve from now on"));
    }

    // Scroll terminal to make room for the options menu.
    // The cursor is at a known position after write_line(). We print
    // newlines to push content up, then move back so the crossterm
    // options render in the space we just created.
    {
        let total_lines = (options.len() + 2) as u16; // title + options + hint
        let mut stdout = std::io::stdout();
        for _ in 0..total_lines {
            crossterm::execute!(stdout, crossterm::style::Print("\n")).ok();
        }
        crossterm::execute!(stdout, crossterm::cursor::MoveUp(total_lines)).ok();
        stdout.flush().ok();
    }

    // Render options and run selection loop
    let mut selected: usize = 0;
    render_options(&options, selected);

    loop {
        if let Ok(Event::Key(key)) = event::read() {
            match (key.code, key.modifiers) {
                (KeyCode::Up, _) => {
                    selected = selected.saturating_sub(1);
                    render_options(&options, selected);
                }
                (KeyCode::Down, _) => {
                    if selected + 1 < options.len() {
                        selected += 1;
                    }
                    render_options(&options, selected);
                }
                (KeyCode::Enter, _) => {
                    clear_options(&options);
                    return match selected {
                        0 => ApprovalDecision::Approve,
                        1 => ApprovalDecision::Reject,
                        2 => {
                            // Feedback: get text from user inline
                            let feedback = read_feedback_inline();
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
                    clear_options(&options);
                    return ApprovalDecision::Reject;
                }
                (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                    clear_options(&options);
                    return ApprovalDecision::Reject;
                }
                _ => {}
            }
        }
    }
}

fn render_options(options: &[(&str, &str)], selected: usize) {
    use crossterm::{
        cursor, execute,
        style::{Attribute, Color as CColor, Print, ResetColor, SetAttribute, SetForegroundColor},
        terminal::{Clear, ClearType},
    };
    let mut stdout = std::io::stdout();
    // After rendering: title MoveDown + N option MoveDowns = N+1 moves.
    // Hint has no MoveDown. To return cursor to the title line: MoveUp(N+1).
    let move_back = (options.len() + 1) as u16;

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

    // Hint (no MoveDown — cursor stays on this line)
    execute!(
        stdout,
        Clear(ClearType::CurrentLine),
        Print("\r  "),
        SetForegroundColor(CColor::DarkGrey),
        Print("\u{2191}/\u{2193} navigate  enter select  esc cancel"),
        ResetColor,
        cursor::MoveUp(move_back),
    )
    .ok();
    stdout.flush().ok();
}

fn clear_options(options: &[(&str, &str)]) {
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
    tui_output::write_blank();
}

/// Read feedback text inline (in raw mode).
fn read_feedback_inline() -> String {
    use crossterm::{cursor, style::Print};

    tui_output::write_line(&Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "\u{276f} Tell koda what to change: ",
            Style::default().fg(Color::Green),
        ),
    ]));

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
