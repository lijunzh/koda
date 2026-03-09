//! Viewport drawing and terminal lifecycle helpers.
//!
//! Extracted from `tui_app.rs`. See #209.
//!
//! # Rendering Architecture
//!
//! Two rendering systems coexist, bridged by terminal re-initialization:
//!
//! ## 1. Engine output — ratatui `insert_before()`
//!
//! LLM streaming, tool calls, diffs, and approval prompts render as
//! native `ratatui::text::Line`/`Span` via `tui_output::emit_line()`.
//! Content scrolls into terminal scrollback above the viewport.
//!
//! ## 2. Slash commands — crossterm direct writes
//!
//! `/model`, `/help`, `/provider`, etc. render via `tui_output::write_line()`
//! which writes styled content directly to stdout with `\r\n` line endings.
//! Interactive menus render in `menu_area` via ratatui.
//!
//! ## Bridge: `init_terminal()` resync
//!
//! After every slash command, we create a fresh `Terminal` with
//! `Viewport::Inline(height)`. This anchors the viewport at the current
//! cursor position — wherever crossterm left it — eliminating stale
//! cursor tracking.

use crate::tui_types::{
    MAX_VIEWPORT_HEIGHT, MIN_VIEWPORT_HEIGHT, MenuContent, PromptMode, Term, TuiState,
};
use crate::widgets::status_bar::StatusBar;

use anyhow::Result;
use koda_core::approval::ApprovalMode;
use ratatui::{
    TerminalOptions, Viewport,
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use tui_textarea::TextArea;

// ── Viewport drawing ────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_viewport(
    frame: &mut ratatui::Frame,
    textarea: &TextArea,
    model: &str,
    tier_label: &str,
    mode: ApprovalMode,
    context_pct: u32,
    state: TuiState,
    prompt_mode: &PromptMode,
    queue_len: usize,
    elapsed_secs: u64,
    last_turn: Option<&crate::widgets::status_bar::TurnStats>,
    menu: &MenuContent,
) {
    let area = frame.area();
    let input_height = textarea.lines().len().max(1) as u16;
    let [sep_row, input_rows, bot_sep_row, status_row, menu_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(input_height),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(area);

    // Separator line: ──────────── 🐻 ─
    let sep_width = sep_row.width.saturating_sub(5) as usize;
    let separator = Line::from(vec![
        Span::styled(
            "\u{2500}".repeat(sep_width),
            Style::default().fg(Color::Rgb(124, 111, 100)),
        ),
        Span::styled(
            " \u{1f43b} \u{2500}",
            Style::default().fg(Color::Rgb(124, 111, 100)),
        ),
    ]);
    frame.render_widget(separator, sep_row);

    // Menu overlay (below status bar)
    render_menu(frame, menu, menu_area);

    // Prompt icon + textarea
    let (prompt_text, color) = match prompt_mode {
        PromptMode::WizardInput { label, .. } => (format!("{label}: "), Color::Cyan),
        PromptMode::Chat => {
            let (icon, c) = match (state, mode) {
                (TuiState::Inferring, _) => ("\u{23f3}", Color::DarkGray),
                (_, ApprovalMode::Safe) => ("\u{1f50d}", Color::Yellow),
                (_, ApprovalMode::Strict) => ("\u{1f512}", Color::Cyan),
                (_, ApprovalMode::Auto) => ("\u{26a1}", Color::Green),
            };
            (format!("{icon}> "), c)
        }
    };
    let prompt_width: u16 = prompt_text.len().min(30) as u16;
    let [prompt_area, text_area] =
        Layout::horizontal([Constraint::Length(prompt_width), Constraint::Fill(1)])
            .areas(input_rows);

    frame.render_widget(
        Paragraph::new(prompt_text).style(Style::default().fg(color)),
        prompt_area,
    );
    frame.render_widget(textarea, text_area);

    // Bottom separator
    let bot_width = bot_sep_row.width as usize;
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "\u{2500}".repeat(bot_width),
            Style::default().fg(Color::Rgb(124, 111, 100)),
        ))),
        bot_sep_row,
    );

    // Status bar
    let mut sb = StatusBar::new(model, tier_label, mode.label(), context_pct);
    if queue_len > 0 {
        sb = sb.with_queue(queue_len);
    }
    if elapsed_secs > 0 {
        sb = sb.with_elapsed(elapsed_secs);
    }
    if let Some(stats) = last_turn {
        sb = sb.with_last_turn(stats);
    }
    frame.render_widget(sb, status_row);
}

/// Render the active menu content into the menu area.
fn render_menu(frame: &mut ratatui::Frame, menu: &MenuContent, menu_area: ratatui::layout::Rect) {
    match menu {
        MenuContent::Slash(dd) => {
            let lines = crate::widgets::slash_menu::build_menu_lines(dd);
            frame.render_widget(Paragraph::new(lines), menu_area);
        }
        MenuContent::Model(dd) => {
            let lines = crate::widgets::dropdown::build_dropdown_lines(dd);
            frame.render_widget(Paragraph::new(lines), menu_area);
        }
        MenuContent::Provider(dd) => {
            let lines = crate::widgets::dropdown::build_dropdown_lines(dd);
            frame.render_widget(Paragraph::new(lines), menu_area);
        }
        MenuContent::Session(dd) => {
            let lines = crate::widgets::dropdown::build_dropdown_lines(dd);
            frame.render_widget(Paragraph::new(lines), menu_area);
        }
        MenuContent::File { dropdown: dd, .. } => {
            let lines = crate::widgets::dropdown::build_dropdown_lines(dd);
            frame.render_widget(Paragraph::new(lines), menu_area);
        }
        MenuContent::WizardTrail(trail) => {
            let mut lines: Vec<Line> = trail
                .iter()
                .map(|(label, value)| {
                    Line::from(vec![
                        Span::styled(
                            format!("  {label}: "),
                            Style::default().fg(Color::Rgb(124, 111, 100)),
                        ),
                        Span::styled(
                            value.clone(),
                            Style::default().fg(Color::Rgb(198, 165, 106)),
                        ),
                    ])
                })
                .collect();
            lines.push(Line::from(Span::styled(
                "  enter to confirm \u{00b7} esc to cancel",
                Style::default().fg(Color::Rgb(124, 111, 100)),
            )));
            frame.render_widget(Paragraph::new(lines), menu_area);
        }
        MenuContent::Approval {
            tool_name, detail, ..
        } => {
            let lines = vec![
                Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(
                        tool_name.clone(),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("  {detail}"), Style::default().fg(Color::DarkGray)),
                ]),
                Line::from(vec![
                    Span::styled("  [y]", Style::default().fg(Color::Green)),
                    Span::styled(" approve  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("[n]", Style::default().fg(Color::Red)),
                    Span::styled(" reject  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("[f]", Style::default().fg(Color::Yellow)),
                    Span::styled(" feedback  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("[a]", Style::default().fg(Color::Rgb(124, 111, 100))),
                    Span::styled(" always", Style::default().fg(Color::DarkGray)),
                ]),
            ];
            frame.render_widget(Paragraph::new(lines), menu_area);
        }
        MenuContent::LoopCap => {
            let lines = vec![
                Line::from(vec![
                    Span::styled("  \u{26a0} ", Style::default().fg(Color::Yellow)),
                    Span::styled(
                        "Hard cap reached. Continue?",
                        Style::default().fg(Color::DarkGray),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("  [y]", Style::default().fg(Color::Green)),
                    Span::styled(" continue  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("[n]", Style::default().fg(Color::Red)),
                    Span::styled(" stop", Style::default().fg(Color::DarkGray)),
                ]),
            ];
            frame.render_widget(Paragraph::new(lines), menu_area);
        }
        MenuContent::None => {}
    }
}

// ── Terminal lifecycle ───────────────────────────────────────

pub(crate) fn init_terminal(height: u16) -> Result<Term> {
    crossterm::terminal::enable_raw_mode()?;
    let _ = std::io::Write::flush(&mut std::io::stdout());

    let mut last_err = None;
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let stdout = std::io::stdout();
        let backend = CrosstermBackend::new(stdout);
        match ratatui::Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(height),
            },
        ) {
            Ok(t) => return Ok(t),
            Err(e) => {
                tracing::debug!("init_terminal attempt {}: {e}", attempt + 1);
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap().into())
}

pub(crate) fn restore_terminal(terminal: &mut Term, height: u16) {
    let _ = terminal.clear();
    let _ = crossterm::terminal::disable_raw_mode();
    print!("\x1b[{}A\x1b[J", height);
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

pub(crate) fn reinit_viewport_in_place(
    terminal: &mut Term,
    old_height: u16,
    new_height: u16,
) -> Result<()> {
    let _ = terminal.clear();
    let _ = crossterm::terminal::disable_raw_mode();
    print!("\x1b[{}A\x1b[J", old_height);
    let _ = std::io::Write::flush(&mut std::io::stdout());
    *terminal = init_terminal(new_height)?;
    Ok(())
}

pub(crate) fn maybe_resize_viewport(
    terminal: &mut Term,
    textarea: &TextArea,
    current_height: &mut u16,
) -> Result<()> {
    let input_lines = textarea.lines().len().max(1) as u16;
    let desired = (input_lines + 1).clamp(MIN_VIEWPORT_HEIGHT, MAX_VIEWPORT_HEIGHT);
    if desired == *current_height {
        return Ok(());
    }
    reinit_viewport_in_place(terminal, *current_height, desired)?;
    *current_height = desired;
    Ok(())
}

// ── Output helper ───────────────────────────────────────────

/// Write a message line above the viewport.
pub(crate) fn emit_above(terminal: &mut Term, line: ratatui::text::Line<'_>) {
    crate::tui_output::emit_line(terminal, line);
}
