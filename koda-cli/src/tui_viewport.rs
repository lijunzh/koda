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
use ratatui_textarea::TextArea;
use unicode_width::UnicodeWidthStr;

// ── Viewport drawing ────────────────────────────────────────

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
    frame.render_widget(textarea, text_area);

    // Horizontal overflow indicators (→ / ←)
    render_overflow_indicators(frame, textarea, text_area);

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
    frame.render_widget(sb, status_row);
}

/// Overlay `→` / `←` arrows on textarea lines that extend beyond the visible area.
///
/// The textarea widget scrolls horizontally but gives no visual cue that
/// content is hidden off-screen. We estimate the scroll offset from the
/// cursor position and paint dim arrows on the right/left edges of lines
/// whose display width exceeds the viewport.
fn render_overflow_indicators(
    frame: &mut ratatui::Frame,
    textarea: &TextArea,
    text_area: ratatui::layout::Rect,
) {
    let w = text_area.width as usize;
    let h = text_area.height as usize;
    if w == 0 || h == 0 {
        return;
    }

    let (cursor_row, cursor_col) = textarea.cursor();
    let lines = textarea.lines();

    // Estimate horizontal scroll offset.
    // ratatui-textarea keeps the cursor visible via next_scroll_top():
    //   if cursor < prev_top → prev_top = cursor
    //   if cursor >= prev_top + width → prev_top = cursor + 1 - width
    //   else → prev_top unchanged
    // Without access to the internal viewport we approximate assuming the
    // common steady-state where the cursor drives the scroll position.
    let col_scroll = cursor_col.saturating_sub(w.saturating_sub(1));

    // Estimate vertical scroll offset (same logic, row axis).
    let row_scroll = cursor_row.saturating_sub(h.saturating_sub(1));

    let visible_end = (row_scroll + h).min(lines.len());
    let arrow_style = Style::default().fg(Color::DarkGray);

    for (vi, line_idx) in (row_scroll..visible_end).enumerate() {
        let display_width = UnicodeWidthStr::width(lines[line_idx].as_str());
        let y = text_area.y + vi as u16;

        // Right overflow: content extends past the right edge
        if display_width > col_scroll + w {
            let x = text_area.x + text_area.width - 1;
            frame.buffer_mut().set_string(x, y, "\u{2192}", arrow_style);
        }

        // Left overflow: content is scrolled past the left edge
        if col_scroll > 0 && display_width > 0 {
            frame
                .buffer_mut()
                .set_string(text_area.x, y, "\u{2190}", arrow_style);
        }
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
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste);
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

pub(crate) fn restore_terminal(terminal: &mut Term, _height: u16) {
    // For inline viewport, clear() moves cursor to viewport_area.as_position()
    // (the viewport origin) and queues Clear(FromCursorDown). This erases the
    // viewport without touching scrollback above.
    //
    // Do NOT call draw() here — it triggers autoresize() which queries cursor
    // position (DSR), and do NOT add manual \x1b[{N}A — the cursor is already
    // at the viewport origin after clear(), not at the textarea row.
    let _ = terminal.clear();
    let _ = std::io::Write::flush(terminal.backend_mut());
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste);
    let _ = crossterm::terminal::disable_raw_mode();
}

pub(crate) fn reinit_viewport_in_place(
    terminal: &mut Term,
    _old_height: u16,
    new_height: u16,
) -> Result<()> {
    // For inline viewport, clear() moves cursor to viewport_area.as_position()
    // (the viewport origin tracked by ratatui) and queues Clear(FromCursorDown).
    // This erases the old viewport region using ratatui's stored coordinates.
    //
    // Do NOT call draw() — it triggers autoresize() which calls
    // get_cursor_position() (DSR query), unreliable during resize events.
    // Do NOT add manual \x1b[{N}A — clear() already positioned the cursor
    // at the viewport origin, not at the textarea row.
    let _ = terminal.clear();
    let _ = std::io::Write::flush(terminal.backend_mut());
    let _ = crossterm::terminal::disable_raw_mode();
    *terminal = init_terminal(new_height)?;
    Ok(())
}

/// Handle terminal resize by erasing the old viewport region and creating a fresh terminal.
///
/// After a column resize, the terminal reflows content, making ratatui's stored
/// `viewport_area.y` unreliable. Instead of using stale coordinates, we erase the
/// bottom `viewport_height` rows of the visible screen (where the inline viewport
/// approximately lives), then create a fresh terminal at that position.
///
/// This preserves scrollback above the viewport while cleaning up old render fragments.
pub(crate) fn scroll_past_and_reinit(
    terminal: &mut Term,
    events: &mut crossterm::event::EventStream,
    viewport_height: u16,
) -> Result<()> {
    // Drop old EventStream FIRST — its background stdin reader competes
    // with DSR cursor queries that init_terminal() needs.
    *events = crossterm::event::EventStream::new();

    // Ink/Claude Code approach: don't try to perfectly erase the old viewport.
    // After a column resize, content reflows unpredictably — separator lines wrap,
    // viewport_area.y becomes stale. Erasing a large region risks eating scrollback.
    //
    // Instead: scroll past the old viewport entirely by emitting newlines.
    // Old viewport fragments end up in scrollback (may look messy, but history is
    // preserved). Then create a fresh terminal at the new cursor position.
    //
    // Stay in raw mode so DSR response in init_terminal() is captured (not echoed).
    let mut stdout = std::io::stdout();
    // Scroll past: emit enough newlines to push old viewport into scrollback.
    for _ in 0..viewport_height {
        let _ = crossterm::execute!(stdout, crossterm::style::Print("\n"));
    }

    // Create fresh terminal. DSR query succeeds because:
    // - Old EventStream reader is stopped (dropped above)
    // - Raw mode is active (DSR response captured, not echoed)
    *terminal = init_terminal(viewport_height)?;
    Ok(())
}

/// Drain any queued `Event::Resize` events so we only reinit once per resize burst.
/// Returns the final terminal size if any resize events were consumed, or `None`.
pub(crate) fn drain_pending_resizes(
    events: &mut crossterm::event::EventStream,
) -> Option<(u16, u16)> {
    use crossterm::event::Event;
    use futures_util::Stream;
    let mut last_size = None;
    // Poll without awaiting — consume only events already buffered.
    let waker = futures_util::task::noop_waker();
    let mut cx = std::task::Context::from_waker(&waker);
    while let std::task::Poll::Ready(Some(Ok(Event::Resize(w, h)))) =
        std::pin::Pin::new(&mut *events).poll_next(&mut cx)
    {
        last_size = Some((w, h));
    }
    last_size
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
