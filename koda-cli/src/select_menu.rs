//! Arrow-key interactive selection menus (standalone crossterm widget).
//!
//! Two entry points:
//! - `select()` — manages raw mode internally (onboarding, commands)
//! - `select_inline()` — renders above the ratatui viewport (TUI slash commands)

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{self, Clear, ClearType},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::io::{self, Write};

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

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
/// For use OUTSIDE the TUI (onboarding, commands).
pub fn select(title: &str, options: &[SelectOption], initial: usize) -> io::Result<Option<usize>> {
    terminal::enable_raw_mode()?;
    let result = run_select_loop(title, options, initial);
    terminal::disable_raw_mode()?;
    result
}

/// Show a selection menu inline above the ratatui viewport.
///
/// Renders at the current cursor position using crossterm (same
/// pattern as the approval widget). Overwrites the viewport
/// temporarily; viewport redraws after selection.
pub fn select_inline(
    _terminal: &mut Term,
    title: &str,
    options: &[SelectOption],
    initial: usize,
) -> io::Result<Option<usize>> {
    let total_lines = menu_height(options);
    let mut selected = initial.min(options.len().saturating_sub(1));
    let mut stdout = io::stdout();

    render_inline(&mut stdout, title, options, selected)?;

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
                    clear_inline(&mut stdout, total_lines)?;
                    return Ok(Some(selected));
                }
                KeyCode::Esc => {
                    clear_inline(&mut stdout, total_lines)?;
                    return Ok(None);
                }
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                    clear_inline(&mut stdout, total_lines)?;
                    return Ok(None);
                }
                _ => {}
            }

            render_inline(&mut stdout, title, options, selected)?;
        }
    }
}

// ── Standalone mode (manages own raw mode) ────────────────────

fn run_select_loop(
    title: &str,
    options: &[SelectOption],
    initial: usize,
) -> io::Result<Option<usize>> {
    let mut selected = initial.min(options.len().saturating_sub(1));
    let mut stdout = io::stdout();
    let lines_drawn = render_standalone(&mut stdout, title, options, selected)?;

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
                    clear_lines(&mut stdout, lines_drawn)?;
                    return Ok(Some(selected));
                }
                KeyCode::Esc => {
                    clear_lines(&mut stdout, lines_drawn)?;
                    return Ok(None);
                }
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                    clear_lines(&mut stdout, lines_drawn)?;
                    return Ok(None);
                }
                _ => {}
            }

            clear_lines(&mut stdout, lines_drawn)?;
            render_standalone(&mut stdout, title, options, selected)?;
        }
    }
}

// ── Standalone renderer (uses \r\n, for pre-raw-mode) ───────────

fn render_standalone(
    stdout: &mut io::Stdout,
    title: &str,
    options: &[SelectOption],
    selected: usize,
) -> io::Result<usize> {
    let mut lines = 0;

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

    for (i, opt) in options.iter().enumerate() {
        render_option_line(stdout, opt, i == selected)?;
        execute!(stdout, Print("\r\n"))?;
        lines += 1;
    }

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

fn clear_lines(stdout: &mut io::Stdout, lines: usize) -> io::Result<()> {
    for _ in 0..lines {
        execute!(stdout, cursor::MoveUp(1), Clear(ClearType::CurrentLine))?;
    }
    Ok(())
}

// ── Inline renderer (cursor movement, no \n) ─────────────────

fn clear_inline(stdout: &mut io::Stdout, total_lines: usize) -> io::Result<()> {
    for _ in 0..total_lines {
        execute!(stdout, Clear(ClearType::CurrentLine), cursor::MoveDown(1))?;
    }
    execute!(stdout, cursor::MoveUp(total_lines as u16))?;
    stdout.flush()?;
    Ok(())
}

fn render_inline(
    stdout: &mut io::Stdout,
    title: &str,
    options: &[SelectOption],
    selected: usize,
) -> io::Result<()> {
    // Title
    execute!(
        stdout,
        Clear(ClearType::CurrentLine),
        Print("\r  "),
        SetForegroundColor(Color::Cyan),
        SetAttribute(Attribute::Bold),
        Print(title),
        SetAttribute(Attribute::Reset),
        cursor::MoveDown(1),
    )?;

    // Options
    for (i, opt) in options.iter().enumerate() {
        execute!(stdout, Clear(ClearType::CurrentLine))?;
        render_option_line(stdout, opt, i == selected)?;
        execute!(stdout, cursor::MoveDown(1))?;
    }

    // Hint
    execute!(
        stdout,
        Clear(ClearType::CurrentLine),
        Print("\r  "),
        SetForegroundColor(Color::DarkGrey),
        Print("\u{2191}/\u{2193} navigate  enter select  esc cancel"),
        ResetColor,
    )?;

    // Move cursor back to top of menu for next re-render
    let height = menu_height(options);
    execute!(stdout, cursor::MoveUp(height as u16 - 1))?;
    stdout.flush()?;
    Ok(())
}

// ── Shared rendering ─────────────────────────────────────

fn render_option_line(
    stdout: &mut io::Stdout,
    opt: &SelectOption,
    is_selected: bool,
) -> io::Result<()> {
    if is_selected {
        execute!(
            stdout,
            Print("\r  "),
            SetForegroundColor(Color::Cyan),
            Print("\u{203a} "),
            SetAttribute(Attribute::Bold),
            Print(&opt.label),
            SetAttribute(Attribute::NoBold),
        )?;
    } else {
        execute!(
            stdout,
            Print("\r    "),
            SetForegroundColor(Color::DarkGrey),
            Print(&opt.label),
        )?;
    }
    if !opt.description.is_empty() {
        execute!(stdout, Print(format!("  {}", opt.description)))?;
    }
    execute!(stdout, ResetColor)?;
    Ok(())
}

fn menu_height(options: &[SelectOption]) -> usize {
    options.len() + 2 // title + options + hint
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

    #[test]
    fn test_menu_height() {
        let opts = vec![SelectOption::new("a", ""), SelectOption::new("b", "")];
        assert_eq!(menu_height(&opts), 4); // title + 2 opts + hint
    }
}
