//! Fullscreen viewport drawing and terminal lifecycle.
//!
//! Three-panel layout: History (scrollable) + Input + Status bar + Menu.
//! The history panel renders from the `ScrollBuffer` render cache.
//!
//! See #472 for the fullscreen migration RFC.

use crate::scroll_buffer::ScrollBuffer;
use crate::tui_types::{MenuContent, PromptMode, Term, TuiState};
use crate::widgets::status_bar::StatusBar;

use anyhow::Result;
use koda_core::approval::ApprovalMode;
use ratatui::{
    Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap},
};
use ratatui_textarea::TextArea;

// ── Three-panel viewport drawing ────────────────────────────

#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_viewport(
    frame: &mut ratatui::Frame,
    textarea: &TextArea,
    model: &str,
    mode: ApprovalMode,
    context_pct: u32,
    state: TuiState,
    prompt_mode: &PromptMode,
    queue_len: usize,
    elapsed_secs: u64,
    last_turn: Option<&crate::widgets::status_bar::TurnStats>,
    menu: &MenuContent,
    scroll_buffer: &ScrollBuffer,
    selection: Option<&crate::mouse_select::Selection>,
) -> ratatui::layout::Rect {
    let area = frame.area();

    // Compute wrapped input height (word-wrap aware, #517)
    let prompt_width_estimate = 4u16; // rough estimate for prompt chars
    let avail_input_width = area.width.saturating_sub(prompt_width_estimate) as usize;
    let input_height = crate::wrap_input::wrapped_height(textarea, avail_input_width).max(1) as u16;

    // Determine menu height (only when active)
    let menu_height = match menu {
        MenuContent::None => 0u16,
        MenuContent::Approval { .. }
        | MenuContent::LoopCap
        | MenuContent::PurgeConfirm { .. }
        | MenuContent::AskUser { .. } => 2,
        MenuContent::WizardTrail(trail) => (trail.len() as u16) + 1,
        MenuContent::Slash(dd) => dd.visible_count() as u16 + 1,
        MenuContent::Model(dd) => dd.visible_count() as u16 + 1,
        MenuContent::Provider(dd) => dd.visible_count() as u16 + 1,
        MenuContent::ProviderModels(dd, _) => dd.visible_count() as u16 + 1,
        MenuContent::Session(dd) => dd.visible_count() as u16 + 1,
        MenuContent::File { dropdown: dd, .. } => dd.visible_count() as u16 + 1,
        MenuContent::HistorySearch { matches, .. } => {
            // 1 header + up to 6 match rows
            (matches.len().min(6) as u16) + 1
        }
    };

    // Layout: History | Separator | Input | Separator | Status | Menu
    let [
        history_area,
        sep_row,
        input_rows,
        bot_sep_row,
        status_row,
        menu_area,
    ] = Layout::vertical([
        Constraint::Min(1),               // history: fill remaining space
        Constraint::Length(1),            // top separator
        Constraint::Length(input_height), // input textarea
        Constraint::Length(1),            // bottom separator
        Constraint::Length(1),            // status bar
        Constraint::Length(menu_height),  // dropdown menu (0 when inactive)
    ])
    .areas(area);

    // ── History panel (scrollable) ────────────────────
    render_history(frame, scroll_buffer, history_area, selection);

    // ── Top separator: ──────────── 🐻 ─ ─────────────────────
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

    // ── Input textarea ──────────────────────────────────
    let (prompt_text, color) = match prompt_mode {
        PromptMode::WizardInput { label, .. } => (format!("{label}: "), Color::Cyan),
        PromptMode::Chat => {
            let (icon, c) = match (state, mode) {
                (TuiState::Inferring, _) => ("\u{23f3}", Color::DarkGray),
                (_, ApprovalMode::Confirm) => ("\u{1f512}", Color::Cyan),
                (_, ApprovalMode::Auto) => ("\u{26a1}", Color::Green),
            };
            (format!("{icon}> "), c)
        }
    };
    let max_prompt = match prompt_mode {
        PromptMode::WizardInput { .. } => 60,
        PromptMode::Chat => 30,
    };
    let prompt_width: u16 =
        (prompt_text.chars().count().min(max_prompt) as u16).min(area.width.saturating_sub(4));
    let [prompt_area, text_area] =
        Layout::horizontal([Constraint::Length(prompt_width), Constraint::Fill(1)])
            .areas(input_rows);

    frame.render_widget(
        Paragraph::new(prompt_text).style(Style::default().fg(color)),
        prompt_area,
    );

    // Render input with word-wrapping (#517)
    let cursor_style = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::REVERSED);
    crate::wrap_input::render_wrapped_input(textarea, text_area, frame.buffer_mut(), cursor_style);

    // ── Bottom separator ────────────────────────────────
    let bot_width = bot_sep_row.width as usize;
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "\u{2500}".repeat(bot_width),
            Style::default().fg(Color::Rgb(124, 111, 100)),
        ))),
        bot_sep_row,
    );

    // ── Status bar ──────────────────────────────────────
    let mut sb = StatusBar::new(model, mode.label(), context_pct);
    if queue_len > 0 {
        sb = sb.with_queue(queue_len);
    }
    if elapsed_secs > 0 {
        sb = sb.with_elapsed(elapsed_secs);
    }
    if let Some(stats) = last_turn {
        sb = sb.with_last_turn(stats);
    }
    // Show scroll position indicator when not at bottom
    if !scroll_buffer.is_sticky() {
        sb = sb.with_scroll_info(scroll_buffer.offset(), scroll_buffer.len());
    }
    frame.render_widget(sb, status_row);

    // ── Menu overlay (below status bar) ───────────────
    render_menu(frame, menu, menu_area);

    history_area
}

/// Render the history panel from the scroll buffer.
///
/// Passes **all** buffer lines to `Paragraph::wrap().scroll()` so ratatui
/// handles visual-line math for wrapped content. Scroll offset is in
/// visual lines (not logical lines), ensuring consistent behavior
/// regardless of line wrapping.
fn render_history(
    frame: &mut ratatui::Frame,
    buffer: &ScrollBuffer,
    area: ratatui::layout::Rect,
    selection: Option<&crate::mouse_select::Selection>,
) {
    let height = area.height as usize;
    let width = area.width as usize;

    // Collect all lines and let Paragraph handle wrapping + scrolling
    let mut lines: Vec<Line<'_>> = buffer.all_lines().cloned().collect();
    let scroll_pos = buffer.paragraph_scroll(height, width);

    // Apply selection highlighting if active
    if let Some(sel) = selection {
        lines =
            crate::mouse_select::apply_selection_highlight(lines, sel, scroll_pos.0, width, area.y);
    }

    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll(scroll_pos);
    frame.render_widget(paragraph, area);

    // Scrollbar — uses visual line counts for accurate thumb position
    let total_visual = buffer.total_visual_lines(width);
    if total_visual > height {
        let scrollable = total_visual.saturating_sub(height);
        let position = scrollable.saturating_sub(buffer.offset());
        let mut scrollbar_state = ScrollbarState::new(scrollable).position(position);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("\u{2502}"))
                .thumb_symbol("\u{2588}"),
            area,
            &mut scrollbar_state,
        );
    }
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
        MenuContent::ProviderModels(dd, _) => {
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
        MenuContent::PurgeConfirm { detail, .. } => {
            let lines = vec![
                Line::from(vec![
                    Span::styled("  \u{1f9f9} ", Style::default().fg(Color::Yellow)),
                    Span::styled(
                        format!("Permanently delete? {detail}"),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("  [y]", Style::default().fg(Color::Green)),
                    Span::styled(" confirm  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("[n]", Style::default().fg(Color::Red)),
                    Span::styled(" cancel", Style::default().fg(Color::DarkGray)),
                ]),
            ];
            frame.render_widget(Paragraph::new(lines), menu_area);
        }
        MenuContent::AskUser {
            question, options, ..
        } => {
            let hint = if options.is_empty() {
                "Type your answer and press Enter".to_string()
            } else {
                let choices = options
                    .iter()
                    .enumerate()
                    .map(|(i, o)| format!("[{}] {}", i + 1, o))
                    .collect::<Vec<_>>()
                    .join("  ");
                format!("Choices: {choices}")
            };
            let lines = vec![
                Line::from(vec![
                    Span::styled("  \u{2753} ", Style::default().fg(Color::Cyan)),
                    Span::styled(question.clone(), Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(hint, Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        "  · Esc to skip",
                        Style::default().fg(Color::Rgb(80, 80, 80)),
                    ),
                ]),
            ];
            frame.render_widget(Paragraph::new(lines), menu_area);
        }
        MenuContent::HistorySearch {
            query,
            matches,
            selected,
        } => {
            let header = Line::from(vec![
                Span::styled(
                    "  \u{1f50d} (reverse-i-search) ",
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(query.as_str(), Style::default().fg(Color::White)),
                if matches.is_empty() {
                    Span::styled(": (no match)", Style::default().fg(Color::DarkGray))
                } else {
                    Span::styled(
                        "  \u{2191}\u{2193} navigate \u{00b7} Enter accept \u{00b7} Esc cancel",
                        Style::default().fg(Color::DarkGray),
                    )
                },
            ]);
            let mut lines = vec![header];
            for (i, m) in matches.iter().take(6).enumerate() {
                let snippet: String = m
                    .chars()
                    .take(menu_area.width.saturating_sub(4) as usize)
                    .collect();
                let style = if i == *selected {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                };
                lines.push(Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(snippet, style),
                ]));
            }
            frame.render_widget(Paragraph::new(lines), menu_area);
        }
        MenuContent::None => {}
    }
}

// ── Terminal lifecycle ─────────────────────────────────

/// Initialize the terminal in fullscreen mode (alternate screen buffer).
///
/// No DSR queries, no cursor position tracking. The app owns every pixel.
pub(crate) fn init_terminal() -> Result<Term> {
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableBracketedPaste,
        crossterm::event::EnableMouseCapture,
    )?;

    let stdout = std::io::stdout();
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Fullscreen,
        },
    )?;

    Ok(terminal)
}

/// Restore the terminal: exit alternate screen, disable raw mode.
pub(crate) fn restore_terminal(terminal: &mut Term) {
    let _ = crossterm::execute!(
        terminal.backend_mut(),
        crossterm::event::DisableMouseCapture,
        crossterm::event::DisableBracketedPaste,
        crossterm::terminal::LeaveAlternateScreen,
    );
    let _ = crossterm::terminal::disable_raw_mode();
}
