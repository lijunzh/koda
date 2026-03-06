//! Arrow-key interactive selection menus (standalone crossterm widget).
//!
//! Two entry points:
//! - `select()` — manages raw mode internally (onboarding, commands)
//! - `select_raw()` — assumes raw mode already active (TUI slash commands)

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{self, Clear, ClearType},
};
use std::io::{self, Write};

/// A selectable option with a label and optional description.
pub struct SelectOption {
    pub label: String,
    pub description: String,
}

impl SelectOption {
    pub fn new(label: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            description: description.into(),
        }
    }
}

/// Show a selection menu, managing raw mode internally.
///
/// Returns `Some(index)` on Enter, `None` on Esc/Ctrl-C.
pub fn select(title: &str, options: &[SelectOption], initial: usize) -> io::Result<Option<usize>> {
    terminal::enable_raw_mode()?;
    let result = run_select_loop(title, options, initial);
    terminal::disable_raw_mode()?;
    result
}

/// Show a selection menu, assuming raw mode is already active.
///
/// Does NOT toggle raw mode — safe to call from the TUI event loop.
pub fn select_raw(
    title: &str,
    options: &[SelectOption],
    initial: usize,
) -> io::Result<Option<usize>> {
    run_select_loop(title, options, initial)
}

fn run_select_loop(
    title: &str,
    options: &[SelectOption],
    initial: usize,
) -> io::Result<Option<usize>> {
    let mut selected = initial.min(options.len().saturating_sub(1));
    let mut stdout = io::stdout();
    let lines_drawn = render_menu(&mut stdout, title, options, selected)?;

    loop {
        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = event::read()?
        {
            match code {
                KeyCode::Up => {
                    selected = selected.saturating_sub(1);
                }
                KeyCode::Down => {
                    if selected + 1 < options.len() {
                        selected += 1;
                    }
                }
                KeyCode::Enter => {
                    clear_menu(&mut stdout, lines_drawn)?;
                    return Ok(Some(selected));
                }
                KeyCode::Esc => {
                    clear_menu(&mut stdout, lines_drawn)?;
                    return Ok(None);
                }
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                    clear_menu(&mut stdout, lines_drawn)?;
                    return Ok(None);
                }
                _ => {}
            }

            clear_menu(&mut stdout, lines_drawn)?;
            render_menu(&mut stdout, title, options, selected)?;
        }
    }
}

fn render_menu(
    stdout: &mut io::Stdout,
    title: &str,
    options: &[SelectOption],
    selected: usize,
) -> io::Result<usize> {
    let mut lines = 0;

    // Title
    execute!(
        stdout,
        Print("\r\n  "),
        SetForegroundColor(Color::Cyan),
        SetAttribute(Attribute::Bold),
        Print(title),
        SetAttribute(Attribute::Reset),
        Print("\r\n"),
    )?;
    lines += 2;

    // Options
    for (i, opt) in options.iter().enumerate() {
        if i == selected {
            execute!(
                stdout,
                Print("  "),
                SetForegroundColor(Color::Cyan),
                Print("\u{203a} "),
                SetAttribute(Attribute::Bold),
                Print(&opt.label),
                SetAttribute(Attribute::NoBold),
            )?;
        } else {
            execute!(
                stdout,
                Print("  "),
                SetForegroundColor(Color::DarkGrey),
                Print("   "),
                Print(&opt.label),
            )?;
        }

        if !opt.description.is_empty() {
            execute!(stdout, Print(format!("  {}", opt.description)))?;
        }

        execute!(stdout, ResetColor, Print("\r\n"))?;
        lines += 1;
    }

    // Footer hint
    execute!(
        stdout,
        Print("\r\n  "),
        SetForegroundColor(Color::DarkGrey),
        Print("\u{2191}/\u{2193} navigate  enter select  esc cancel"),
        ResetColor,
        Print("\r\n"),
    )?;
    lines += 2;

    stdout.flush()?;
    Ok(lines)
}

fn clear_menu(stdout: &mut io::Stdout, lines: usize) -> io::Result<()> {
    for _ in 0..lines {
        execute!(stdout, cursor::MoveUp(1), Clear(ClearType::CurrentLine))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_select_option_new() {
        let opt = SelectOption::new("hello", "world");
        assert_eq!(opt.label, "hello");
        assert_eq!(opt.description, "world");
    }
}
