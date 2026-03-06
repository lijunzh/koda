//! Arrow-key interactive selection menus (standalone crossterm widget).
//!
//! Used by onboarding, commands, and slash commands for picking
//! from a list using \u{2191}/\u{2193} arrow keys and Enter.
//! Manages its own raw mode internally \u{2014} works both inside
//! and outside the TUI viewport.

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{self, ClearType},
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

/// Show an interactive arrow-key selection menu.
///
/// Returns `Some(index)` on Enter, `None` on Esc/Ctrl-C.
pub fn select(title: &str, options: &[SelectOption], initial: usize) -> io::Result<Option<usize>> {
    let mut selected = initial.min(options.len().saturating_sub(1));
    let mut stdout = io::stdout();

    // Enter raw mode for key-by-key input
    terminal::enable_raw_mode()?;

    // Render initial state
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
                    terminal::disable_raw_mode()?;
                    return Ok(Some(selected));
                }
                KeyCode::Esc => {
                    clear_menu(&mut stdout, lines_drawn)?;
                    terminal::disable_raw_mode()?;
                    return Ok(None);
                }
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                    clear_menu(&mut stdout, lines_drawn)?;
                    terminal::disable_raw_mode()?;
                    return Ok(None);
                }
                _ => {}
            }

            // Re-render
            clear_menu(&mut stdout, lines_drawn)?;
            render_menu(&mut stdout, title, options, selected)?;
        }
    }
}

/// Render the menu and return how many lines were drawn.
fn render_menu(
    stdout: &mut io::Stdout,
    title: &str,
    options: &[SelectOption],
    selected: usize,
) -> io::Result<usize> {
    let mut lines = 0;

    // Title
    write!(stdout, "\r\n  \x1b[1;36m{title}\x1b[0m\r\n")?;
    lines += 2;

    // Options
    for (i, opt) in options.iter().enumerate() {
        if i == selected {
            write!(stdout, "  \x1b[36m › \x1b[1m{}\x1b[0m", opt.label)?;
        } else {
            write!(stdout, "  \x1b[90m   {}\x1b[0m", opt.label)?;
        }

        if !opt.description.is_empty() {
            let desc_style = if i == selected {
                "\x1b[36m"
            } else {
                "\x1b[90m"
            };
            write!(stdout, "  {desc_style}{}\x1b[0m", opt.description)?;
        }

        write!(stdout, "\r\n")?;
        lines += 1;
    }

    // Footer hint
    write!(
        stdout,
        "\r\n  \x1b[90m↑/↓ navigate  enter select  esc cancel\x1b[0m\r\n"
    )?;
    lines += 2;

    stdout.flush()?;
    Ok(lines)
}

/// Clear the menu by moving cursor up and clearing lines.
fn clear_menu(stdout: &mut io::Stdout, lines: usize) -> io::Result<()> {
    for _ in 0..lines {
        execute!(
            stdout,
            cursor::MoveUp(1),
            terminal::Clear(ClearType::CurrentLine)
        )?;
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
