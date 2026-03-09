//! Output bridge for the TUI.
//!
//! Two rendering paths (see `tui_app.rs` for full architecture):
//!
//! - **`emit_line()`** — ratatui `insert_before()` for engine output
//!   (LLM streaming, tool calls, diffs). Managed by ratatui.
//!
//! - **`write_line()`** — crossterm direct writes for slash commands.
//!   Uses `\r\n` line endings in raw mode. After slash commands,
//!   `tui_app` calls `init_terminal()` to resync the viewport.

use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget, Wrap},
};

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

/// Write `Line`s above the inline viewport via `insert_before()`.
///
/// Long lines are soft-wrapped to the terminal width so content is
/// never silently truncated. The `insert_before` height is calculated
/// to account for the extra rows that wrapped lines occupy.
pub fn emit_lines(terminal: &mut Term, lines: &[Line<'_>]) {
    if lines.is_empty() {
        return;
    }
    let term_width = terminal.size().map(|s| s.width as usize).unwrap_or(80).max(1);
    // Each line may wrap to multiple rows — calculate the true height.
    let height: u16 = lines
        .iter()
        .map(|l| {
            let w = l.width();
            if w == 0 {
                1u16
            } else {
                ((w + term_width - 1) / term_width) as u16 // ceil division
            }
        })
        .sum();
    let owned: Vec<Line<'static>> = lines
        .iter()
        .map(|l| {
            Line::from(
                l.spans
                    .iter()
                    .map(|s| Span::styled(s.content.to_string(), s.style))
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    let _ = terminal.insert_before(height, |buf| {
        Paragraph::new(owned)
            .wrap(Wrap { trim: false })
            .render(buf.area, buf);
    });
}

/// Write a single `Line` above the viewport.
pub fn emit_line(terminal: &mut Term, line: Line<'_>) {
    emit_lines(terminal, &[line]);
}

// ── Style constants ─────────────────────────────────────────
// Centralized color palette for the TUI renderer.

pub const DIM: Style = Style::new().fg(Color::DarkGray);
pub const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);
pub const CYAN: Style = Style::new().fg(Color::Cyan);
pub const YELLOW: Style = Style::new().fg(Color::Yellow);
pub const RED: Style = Style::new().fg(Color::Red);
pub const GREEN: Style = Style::new().fg(Color::Green);
pub const MAGENTA: Style = Style::new().fg(Color::Magenta);
pub const ORANGE: Style = Style::new().fg(Color::Rgb(255, 165, 0));
pub const AMBER: Style = Style::new().fg(Color::Rgb(255, 191, 0));

// Warm palette — earthy tones for koda's bear identity.
pub const WARM_TITLE: Style = Style::new()
    .fg(Color::Rgb(229, 192, 123)) // soft gold #e5c07b
    .add_modifier(Modifier::BOLD);
pub const WARM_ACCENT: Style = Style::new().fg(Color::Rgb(209, 154, 102)); // amber #d19a66
pub const WARM_MUTED: Style = Style::new().fg(Color::Rgb(124, 111, 100)); // brown #7c6f64
pub const WARM_INFO: Style = Style::new().fg(Color::Rgb(198, 165, 106)); // soft gold #c6a56a

// ── Direct crossterm output (for slash commands) ───────────────
//
// Slash commands use these instead of `emit_line()` to avoid mixing
// `insert_before()` with crossterm cursor management (select menus).
// After a slash command completes, `terminal.draw()` resyncs the viewport.

/// Write a styled `Line` directly to stdout via crossterm.
///
/// Uses `\r\n` for line endings (raw mode). No `insert_before()` —
/// all slash command output should use this to stay in one rendering system.
pub fn write_line(line: &Line<'_>) {
    use crossterm::{
        execute,
        style::{Attribute, Print, ResetColor, SetAttribute, SetForegroundColor},
        terminal::{Clear, ClearType},
    };
    use std::io::Write;

    let mut stdout = std::io::stdout();
    // Clear the current line first — prevents stale viewport content
    // (e.g. the status bar) from showing through when slash commands
    // write over the old viewport area.
    execute!(stdout, Clear(ClearType::CurrentLine), Print("\r")).ok();
    for span in &line.spans {
        // Apply foreground color
        if let Some(cc) = span.style.fg.and_then(ratatui_to_crossterm_color) {
            execute!(stdout, SetForegroundColor(cc)).ok();
        }
        // Apply bold
        if span.style.add_modifier.contains(Modifier::BOLD) {
            execute!(stdout, SetAttribute(Attribute::Bold)).ok();
        }
        execute!(stdout, Print(&*span.content)).ok();
        // Reset after each span
        if span.style.fg.is_some() || !span.style.add_modifier.is_empty() {
            execute!(stdout, ResetColor, SetAttribute(Attribute::Reset)).ok();
        }
    }
    execute!(stdout, Print("\r\n")).ok();
    stdout.flush().ok();
}

/// Write a blank line directly to stdout.
pub fn write_blank() {
    use crossterm::{
        execute,
        style::Print,
        terminal::{Clear, ClearType},
    };
    let mut stdout = std::io::stdout();
    execute!(stdout, Clear(ClearType::CurrentLine), Print("\r\n")).ok();
}

fn ratatui_to_crossterm_color(c: Color) -> Option<crossterm::style::Color> {
    use crossterm::style::Color as CC;
    Some(match c {
        Color::Black => CC::Black,
        Color::Red => CC::DarkRed,
        Color::Green => CC::DarkGreen,
        Color::Yellow => CC::DarkYellow,
        Color::Blue => CC::DarkBlue,
        Color::Magenta => CC::DarkMagenta,
        Color::Cyan => CC::DarkCyan,
        Color::Gray => CC::Grey,
        Color::DarkGray => CC::DarkGrey,
        Color::LightRed => CC::Red,
        Color::LightGreen => CC::Green,
        Color::LightYellow => CC::Yellow,
        Color::LightBlue => CC::Blue,
        Color::LightMagenta => CC::Magenta,
        Color::LightCyan => CC::Cyan,
        Color::White => CC::White,
        Color::Rgb(r, g, b) => CC::Rgb { r, g, b },
        _ => return None,
    })
}

// ── Shared message helpers (crossterm path) ──────────────
// Used by tui_commands.rs and tui_wizards.rs for consistent output.

/// Print a success message: " ✓ {msg}"
pub fn ok_msg(msg: String) {
    write_line(&Line::from(vec![
        Span::styled("  \u{2713} ", GREEN),
        Span::raw(msg),
    ]));
}

/// Print an error message: " ✗ {msg}"
pub fn err_msg(msg: String) {
    write_line(&Line::from(vec![
        Span::styled("  \u{2717} ", RED),
        Span::styled(msg, RED),
    ]));
}

/// Print a dim message: "  {msg}"
pub fn dim_msg(msg: String) {
    write_line(&Line::styled(format!("  {msg}"), DIM));
}

/// Print a warning message: " ⚠ {msg}"
pub fn warn_msg(msg: String) {
    write_line(&Line::from(vec![
        Span::styled("  \u{26a0} ", YELLOW),
        Span::styled(msg, YELLOW),
    ]));
}
