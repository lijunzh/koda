//! Fullscreen viewport drawing and terminal lifecycle.
//!
//! Three-panel layout: History (scrollable) + Input + Status bar + Menu.
//! The history panel renders from the `ScrollBuffer` render cache.
//!
//! See #472 for the fullscreen migration RFC.

use crate::scroll_buffer::ScrollBuffer;
use crate::tui_types::{MenuContent, PromptMode, Term, TuiState};
use crate::widgets::queue_preview::QueuePreview;
use crate::widgets::status_bar::StatusBar;
use koda_core::mcp::manager::McpStatusBarInfo;

use anyhow::Result;
use koda_core::trust::TrustMode;
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
    mode: TrustMode,
    context_pct: u32,
    state: TuiState,
    prompt_mode: &PromptMode,
    // Items to show in the queue preview (at most `QueuePreview::MAX_VISIBLE`).
    queue_items: &[String],
    // Total deferred queue length (may be > queue_items.len()).
    queue_total: usize,
    elapsed_secs: u64,
    last_turn: Option<&crate::widgets::status_bar::TurnStats>,
    menu: &MenuContent,
    scroll_buffer: &ScrollBuffer,
    selection: Option<&crate::mouse_select::Selection>,
    mcp_info: Option<McpStatusBarInfo>,
    // Background-task counts (running sub-agents, running shell
    // processes). Drives the status-bar pill added in #1158 (b);
    // both zero → segment hidden by `StatusBar::with_bg_counts`.
    bg_counts: (usize, usize),
    project_root: &std::path::Path,
) -> ratatui::layout::Rect {
    let area = frame.area();

    // Compute wrapped input height (word-wrap aware, #517)
    let prompt_width_estimate = 4u16; // rough estimate for prompt chars
    let avail_input_width = area.width.saturating_sub(prompt_width_estimate) as usize;
    let input_height = crate::wrap_input::wrapped_height(textarea, avail_input_width).max(1) as u16;

    // Determine menu height (only when active)
    let menu_height = match menu {
        MenuContent::None => 0u16,
        MenuContent::Approval { .. } | MenuContent::LoopCap | MenuContent::PurgeConfirm { .. } => 2,
        MenuContent::AskUser {
            question, options, ..
        } => ask_user_menu_height(question, options, area.width, area.height),
        MenuContent::WizardTrail(trail) => (trail.len() as u16) + 1,
        MenuContent::Slash(dd) => dd.visible_count() as u16 + 1,
        MenuContent::Model(dd) => dd.visible_count() as u16 + 1,
        MenuContent::Provider(dd) => dd.visible_count() as u16 + 1,
        MenuContent::ProviderModels(dd, _) => dd.visible_count() as u16 + 1,
        MenuContent::Key(dd) => dd.visible_count() as u16 + 1,
        MenuContent::Session(dd) => dd.visible_count() as u16 + 1,
        MenuContent::File { dropdown: dd, .. } => dd.visible_count() as u16 + 1,
        MenuContent::HistorySearch { matches, .. } => {
            // 1 header + up to 6 match rows
            (matches.len().min(6) as u16) + 1
        }
    };

    // Queue preview height: 0 when idle / queue empty.
    let queue_preview_height = QueuePreview::height_for(queue_total);

    // Layout: History | Sep | Input | Sep | Queue? | Status | Menu
    let [
        history_area,
        sep_row,
        input_rows,
        bot_sep_row,
        queue_preview_row,
        status_row,
        menu_area,
    ] = Layout::vertical([
        Constraint::Min(1),                       // history: fill remaining space
        Constraint::Length(1),                    // top separator
        Constraint::Length(input_height),         // input textarea
        Constraint::Length(1),                    // bottom separator
        Constraint::Length(queue_preview_height), // later_queue preview (0 when empty)
        Constraint::Length(1),                    // status bar
        Constraint::Length(menu_height),          // dropdown menu (0 when inactive)
    ])
    .areas(area);

    // ── History panel (scrollable) ────────────────────
    render_history(frame, scroll_buffer, history_area, selection, project_root);

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
                (_, TrustMode::Plan) => ("\u{1f4cb}", Color::DarkGray),
                (_, TrustMode::Safe) => ("\u{1f512}", Color::Cyan),
                (_, TrustMode::Auto) => ("\u{26a1}", Color::Green),
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

    // ── Queue preview (above status bar, hidden when empty) ─────────────
    if queue_preview_height > 0 {
        frame.render_widget(
            QueuePreview::new(queue_items, queue_total),
            queue_preview_row,
        );
    }

    // ── Status bar ────────────────────────────────────────────────
    // CWD displayed as the leftmost segment (#1105) — mirrors
    // shell-prompt convention so users always know where commands
    // will land. `project_root` is the canonical session cwd
    // (canonicalized at startup in app.rs, fixed for the session
    // since koda has no `/cd`-style mid-session command).
    let mut sb = StatusBar::new(model, mode.label(), context_pct).with_cwd(project_root);
    if queue_total > 0 {
        sb = sb.with_queue(queue_total);
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
    if let Some(mcp) = mcp_info {
        sb = sb.with_mcp_info(mcp);
    }
    let (bg_agents, bg_processes) = bg_counts;
    if bg_agents > 0 || bg_processes > 0 {
        sb = sb.with_bg_counts(bg_agents, bg_processes);
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
    project_root: &std::path::Path,
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

    // Post-render: turn cyan+underlined PATH cells into clickable OSC 8
    // hyperlinks. Pure cell-symbol mutation — zero impact on layout.
    // See `crate::hyperlink` for the why.
    crate::hyperlink::link_paths_in_buffer(frame.buffer_mut(), area, project_root);

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
        MenuContent::Key(dd) => {
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
            // `Wrap { trim: false }` mirrors the height calculation in
            // `ask_user_menu_height` so what we draw matches what we
            // reserved space for. See #1024.
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), menu_area);
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

// ── AskUser dynamic height (#1024) ─────────────────────

/// Build the hint text for an AskUser menu, matching the renderer.
fn build_ask_user_hint(options: &[String]) -> String {
    if options.is_empty() {
        "Type your answer and press Enter".to_string()
    } else {
        let choices = options
            .iter()
            .enumerate()
            .map(|(i, o)| format!("[{}] {}", i + 1, o))
            .collect::<Vec<_>>()
            .join("  ");
        format!("Choices: {choices}")
    }
}

/// Compute the dynamic height for an AskUser menu so the question (and
/// the hint with options) wraps instead of being truncated at the
/// screen edge. Capped at half the viewport so the menu can never
/// crowd out the history panel.
///
/// The two rendered lines are:
/// - `"  ❓ " + question` (prefix is 5 visual cols: 2 spaces + wide
///   emoji + 1 space).
/// - `"  " + hint + "  · Esc to skip"` (prefix is 2 visual cols).
///
/// We compute the wrap count of each by reusing
/// [`crate::wrap_util::visual_line_count`] \u2014 the same word-wrap
/// algorithm ratatui's `Paragraph::wrap(Wrap { trim: false })` uses,
/// so the rendered height matches the reserved menu_area height
/// exactly.
///
/// Regression coverage for #1024.
fn ask_user_menu_height(
    question: &str,
    options: &[String],
    viewport_width: u16,
    viewport_height: u16,
) -> u16 {
    use crate::wrap_util::visual_line_count;

    // The Paragraph wraps each `Line` independently, so we measure
    // each line's full text (prefix + content + suffix) at viewport
    // width \u2014 not at "width minus prefix". Continuation rows wrap to
    // column 0 just like ratatui does.
    let q_text = format!("  \u{2753} {question}");
    let q_rows = visual_line_count(&q_text, viewport_width as usize);

    let hint = build_ask_user_hint(options);
    let h_text = format!("  {hint}  \u{00b7} Esc to skip");
    let h_rows = visual_line_count(&h_text, viewport_width as usize);

    let total = (q_rows + h_rows) as u16;

    // Cap at half the viewport (min 2) so the menu can't eat the
    // history panel on a tiny terminal or with a giant question.
    let cap = (viewport_height / 2).max(2);
    total.clamp(2, cap)
}

// ── Terminal lifecycle ─────────────────────────────────

/// Initialize the terminal in fullscreen mode (alternate screen buffer).
///
/// No DSR queries, no cursor position tracking. The app owns every pixel.
///
/// Also installs a panic hook that restores the terminal before the panic
/// propagates — without it, a panic anywhere in the app (provider code, tool
/// dispatch, ratatui render, tokio task, JSON deserialization, …) leaves the
/// terminal in raw mode + alternate screen + mouse capture on, which makes
/// the user's shell unusable until they run `reset` or open a new session.
pub(crate) fn init_terminal() -> Result<Term> {
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableBracketedPaste,
        crossterm::event::EnableMouseCapture,
    )?;

    set_panic_hook();
    install_signal_handler();
    install_atexit_hook();

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

/// Disable raw mode + leave alternate screen + drop mouse/paste capture.
///
/// Writes directly to `stdout()` so it can run from anywhere — including
/// the panic hook, which has no access to the `Terminal` value.
pub(crate) fn restore_terminal_modes() {
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::DisableMouseCapture,
        crossterm::event::DisableBracketedPaste,
        crossterm::terminal::LeaveAlternateScreen,
    );
    let _ = crossterm::terminal::disable_raw_mode();
}

/// Restore the terminal: exit alternate screen, disable raw mode.
///
/// Convenience wrapper for the normal-shutdown path that already owns a
/// `Terminal`. Equivalent to [`restore_terminal_modes`].
pub(crate) fn restore_terminal(_terminal: &mut Term) {
    restore_terminal_modes();
}

/// Install a panic hook that restores the terminal before the original hook
/// runs (so the panic message + backtrace still surface to the user, but on a
/// sane TTY rather than a corrupted one).
///
/// Pattern lifted from `codex-rs/tui/src/tui.rs`; see issue #1119 for the
/// comparative analysis.
fn set_panic_hook() {
    // Production callers always restore the real terminal. The inner
    // helper takes the restore callback as a parameter so the regression
    // test in #1124 can substitute a spy without depending on a real TTY.
    install_panic_hook(restore_terminal_modes);
}

/// Install a panic hook that calls `restore` (best-effort) before chaining
/// to whatever hook was previously installed. Extracted from
/// [`set_panic_hook`] purely so the chain-integrity contract can be
/// asserted under `#[cfg(test)]` without touching real terminal state.
fn install_panic_hook(restore: fn()) {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Best-effort: ignore errors here, we're already failing.
        restore();
        // Forensic breadcrumb in the per-process tracing log
        // (`koda-{PID}.log`). The panic.log written below has the
        // multi-line record + backtrace; this single line is what
        // makes the panic correlatable with surrounding tracing
        // events when both files end up in a /debug-bundle
        // (RFC #1167 §D3). If the global subscriber isn't installed
        // (headless tests, very early panics), tracing::error! is a
        // no-op — safe.
        tracing::error!("{}", crate::panic_log::panic_breadcrumb(info));
        // Forensic record for post-mortem debugging (#1122). Wrapped
        // in a `let _ =` chain inside the helper so any I/O error here
        // cannot turn into panic-in-panic-hook.
        crate::panic_log::write_panic_log(info);
        original(info);
    }));
}

/// Install signal handlers (SIGTERM, SIGINT, SIGHUP) that restore the
/// terminal before the process dies (#1176).
///
/// Without this, `kill <pid>` from another shell or closing the iTerm tab
/// while koda is running leaves the terminal in raw mode + alt-screen +
/// mouse-capture mode — the user has to `printf '\x1b[?1003l\x1b[?1006l'`
/// or restart the shell to recover native text-selection.
///
/// Why all three signals:
/// - `SIGTERM`: standard "please exit gracefully", e.g. `kill <pid>`.
/// - `SIGINT`: Ctrl-C while raw mode is OFF (in raw mode the terminal
///   delivers Ctrl-C as a key event, not a signal, so this only fires when
///   another process sends `kill -INT` externally — still worth handling).
/// - `SIGHUP`: terminal disconnect (closing the iTerm tab, SSH session
///   drop, parent shell exit).
///
/// Note: this co-exists fine with crossterm's own `SIGWINCH` handler
/// (resize events). `signal-hook` is explicitly designed to allow multiple
/// consumers per signal; we use a disjoint signal set anyway.
///
/// Failure modes are best-effort: if `signal-hook::iterator::Signals::new`
/// returns an error (rare — typically only on signal-restricted sandboxes),
/// we silently skip handler installation. The terminal will still be
/// restored on normal exit and panic, just not on signal-induced exit.
fn install_signal_handler() {
    install_signal_handler_with(restore_terminal_modes);
}

/// Inner helper that takes the restore callback as a parameter so unit
/// tests can substitute a spy without sending real signals or touching a
/// real TTY (mirroring the [`install_panic_hook`] testability pattern).
fn install_signal_handler_with(restore: fn()) {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;

    let mut signals = match Signals::new([SIGTERM, SIGINT, SIGHUP]) {
        Ok(s) => s,
        Err(_) => {
            // Signal-restricted environment (some sandboxes, some CI). The
            // panic hook + atexit hook still cover most cleanup paths.
            return;
        }
    };

    std::thread::Builder::new()
        .name("koda-signal-cleanup".into())
        .spawn(move || {
            // Block until any of the registered signals arrives. The
            // iterator yields one signal per occurrence; we only need to
            // restore + exit once, so we take the first.
            if let Some(sig) = signals.forever().next() {
                restore();
                // Exit with the conventional 128 + signal number so that
                // shell scripts can detect signal-induced termination.
                // We deliberately do NOT re-raise the signal with default
                // disposition: that requires re-arming and avoids a race
                // where the process exits via two different paths
                // simultaneously (the signal default and our exit). The
                // 128+sig convention is what bash uses for `$?` after
                // signal death and is unambiguous to scripts.
                std::process::exit(128 + sig);
            }
        })
        .ok();
}

/// Register an `atexit` hook as defense-in-depth: any `std::process::exit`
/// call (including library-internal ones we don't control) flushes through
/// libc's `atexit` chain, which gives us a last-chance restore.
///
/// Today's audit (`rg "process::exit"`) shows zero such calls happen *after*
/// `init_terminal` runs (the three call-sites in `app.rs` are all on the
/// pre-TUI command-dispatch path). This hook costs ~10 lines and protects
/// against future regressions where someone adds `process::exit` to a code
/// path that runs while the terminal is in raw mode.
///
/// Idempotent: registers the cleanup function exactly once via `Once`,
/// even if `init_terminal` is called multiple times in the same process
/// (which never happens in production but happens in tests).
fn install_atexit_hook() {
    static REGISTERED: std::sync::Once = std::sync::Once::new();
    REGISTERED.call_once(|| {
        // SAFETY: `libc::atexit` registers a `extern "C" fn()` to be
        // called at normal program termination (before `_exit`). The
        // function takes no arguments and returns no value. It must not
        // panic across the FFI boundary; `restore_terminal_modes` is
        // wrapped in `let _ =` patterns internally so any I/O failure
        // is swallowed rather than panicking.
        unsafe {
            libc::atexit(atexit_cleanup);
        }
    });
}

/// `extern "C"` shim suitable for passing to `libc::atexit`.
///
/// Kept as a free function (not a closure) because `atexit` requires a
/// plain function pointer, not a Rust trait object.
extern "C" fn atexit_cleanup() {
    restore_terminal_modes();
}

#[cfg(test)]
mod signal_handler_tests {
    //! Regression tests for the signal handler installed by
    //! [`super::install_signal_handler`] (issue #1176).
    //!
    //! ## What we can and cannot test in-process
    //!
    //! We deliberately do NOT send real signals to the test process.
    //! Doing so would either:
    //!   - Kill the test runner (if `restore` calls `process::exit` as it
    //!     does in production), or
    //!   - Race with `cargo test`'s own signal handling.
    //!
    //! Instead we verify the contracts that are testable without firing
    //! a real signal:
    //!
    //! 1. `install_signal_handler_with` returns without panicking even
    //!    when invoked multiple times in a row (relevant because
    //!    `init_terminal` may be re-entered in some test paths).
    //! 2. The atexit hook registers exactly once across N calls (the
    //!    `Once` guard works as intended).
    //!
    //! Real-signal coverage lives in the manual smoke list in #1176's
    //! acceptance criteria — verified by `kill <pid>` and tab-close
    //! on a real terminal.

    use super::{install_atexit_hook, install_signal_handler_with};
    use serial_test::serial;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SPY_RESTORE_CALLS: AtomicUsize = AtomicUsize::new(0);

    fn spy_restore() {
        SPY_RESTORE_CALLS.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    #[serial]
    fn install_signal_handler_does_not_panic_on_repeated_calls() {
        // The handler-installation path spawns a thread per call. This
        // is wasteful but not broken — each thread blocks on its own
        // signal-hook iterator. Calling install N times must not panic
        // (e.g. via thread-name collision or signal-hook double-register
        // errors); it should just become a no-op for the second+ caller
        // in practice.
        SPY_RESTORE_CALLS.store(0, Ordering::SeqCst);
        for _ in 0..3 {
            install_signal_handler_with(spy_restore);
        }
        // We have no way to verify the handlers fire without sending a
        // real signal, but we can verify nothing panicked and the
        // installation completed.
        assert_eq!(
            SPY_RESTORE_CALLS.load(Ordering::SeqCst),
            0,
            "spy should not have fired without a signal"
        );
    }

    #[test]
    #[serial]
    fn install_atexit_hook_is_idempotent() {
        // The function uses `std::sync::Once` internally; calling it
        // many times must register the cleanup function exactly once,
        // not N times. We can't directly inspect libc's atexit chain
        // from Rust, but we can verify the function returns without
        // panicking on repeat calls (a `Once` violation would manifest
        // as a panic in `call_once`).
        for _ in 0..5 {
            install_atexit_hook();
        }
    }
}

#[cfg(test)]
mod panic_hook_tests {
    //! Regression tests for the panic hook installed by
    //! [`super::set_panic_hook`] (issue #1120).
    //!
    //! The contract under test:
    //!
    //! 1. The injected `restore` callback must run on panic (otherwise
    //!    the user's terminal stays in raw mode + alternate screen after
    //!    a crash — the original UX bug from #1119).
    //! 2. The previously-installed panic hook must still run afterwards
    //!    (so the panic message + backtrace still surface). A naive
    //!    refactor that drops the chain via `set_hook(Box::new(...))`
    //!    without first capturing `take_hook()` would silently break
    //!    this and we'd never know until the next user crash.
    //!
    //! `serial_test` is load-bearing: the panic hook is global mutable
    //! state and Rust runs tests in parallel by default. Without
    //! `#[serial]`, two tests calling `panic::set_hook` would race.

    use super::install_panic_hook;
    use serial_test::serial;
    use std::panic;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    // The panic-hook closure must be `'static`, so the spy state has to
    // live in `static`s rather than locals. `#[serial]` keeps these
    // single-writer-at-a-time across the whole crate's test suite.
    static RESTORE_CALLED: AtomicBool = AtomicBool::new(false);
    static ORIGINAL_RAN: AtomicUsize = AtomicUsize::new(0);

    fn spy_restore() {
        RESTORE_CALLED.store(true, Ordering::SeqCst);
    }

    #[test]
    #[serial]
    fn panic_hook_restores_terminal_then_chains_to_original() {
        // Reset spy state from any previous test run.
        RESTORE_CALLED.store(false, Ordering::SeqCst);
        ORIGINAL_RAN.store(0, Ordering::SeqCst);

        // Snapshot whatever hook the test harness installed so we can
        // restore it on the way out and not poison sibling tests.
        let saved = panic::take_hook();

        // Install a no-op "original" that just records it ran. Using a
        // silent hook also keeps the panic message out of test output.
        panic::set_hook(Box::new(|_| {
            ORIGINAL_RAN.fetch_add(1, Ordering::SeqCst);
        }));

        // Now layer the chain under test on top.
        install_panic_hook(spy_restore);

        // Trigger a panic; `catch_unwind` swallows it but the hook still
        // fires synchronously before the unwind propagates.
        let result = panic::catch_unwind(|| {
            panic!("intentional panic for #1124 regression test");
        });
        assert!(
            result.is_err(),
            "catch_unwind should have caught the deliberate panic"
        );

        // Contract 1: the restore callback ran.
        assert!(
            RESTORE_CALLED.load(Ordering::SeqCst),
            "spy_restore should have been invoked by the panic hook"
        );

        // Contract 2: the previously-installed hook ran exactly once.
        // Two invocations would mean we accidentally double-chained;
        // zero would mean we clobbered the chain (the actual bug we're
        // guarding against).
        assert_eq!(
            ORIGINAL_RAN.load(Ordering::SeqCst),
            1,
            "the previously-installed hook should be invoked exactly once"
        );

        // Cleanup: drop the chained hook and reinstate the test
        // harness's hook so we leave global state exactly as we found it.
        let _ = panic::take_hook();
        panic::set_hook(saved);
    }
}

#[cfg(test)]
mod ask_user_height_tests {
    use super::ask_user_menu_height;

    /// Short question fits on one line → 2 rows total (question + hint).
    /// Same as the legacy hardcoded value, so existing layouts don't shift.
    #[test]
    fn short_question_yields_two_rows() {
        let h = ask_user_menu_height("Continue?", &[], 80, 24);
        assert_eq!(h, 2);
    }

    /// Long question wraps to multiple rows. Regression for #1024.
    /// Without the fix, the question would be truncated at column 80.
    #[test]
    fn long_question_wraps_and_grows_menu() {
        // ~200-char question — must wrap to 3+ rows at width 80.
        let q = "Should we proceed with the migration of all the legacy \
                 modules to the new architecture, including the \
                 deprecated bits and the experimental ones nobody \
                 remembers writing in the first place?";
        let h = ask_user_menu_height(q, &[], 80, 24);
        assert!(
            h > 2,
            "long question should grow the menu beyond 2 rows, got {h}"
        );
    }

    /// Width 80 with a 200-char question wraps to a known row count.
    /// Pin a tight bound so regressions in the wrap math get caught.
    #[test]
    fn long_question_height_is_bounded_by_wrap_count() {
        let q = "x".repeat(200);
        // Question line text = "  \u{2753} " (5 cols: 2 spaces + 2-col emoji
        // + 1 space) + 200 x's = 205 cols. At width 80 with word-wrap,
        // the long single-word x-run wraps *before* the word when it
        // doesn't fit (matching ratatui's `Wrap { trim: false }`):
        //   Row 1: "  \u{2753} " + 75 x's (cols 0..79)
        //   Row 2: 80 x's   (chars 76..155)
        //   Row 3: 80 x's   (chars 156..200, force-break) — actually only 45
        //   Row 4: trailing — depends on word-wrap re-flow accounting
        // We don't pin the exact integer here (the algorithm has subtle
        // word-relocation semantics); we just bound it.
        let h = ask_user_menu_height(&q, &[], 80, 24);
        assert!(
            (4..=6).contains(&h),
            "expected 4..=6 rows for 200-char question at width 80, got {h}"
        );
    }

    /// Many options make the hint line wrap too — both lines contribute
    /// to the total height.
    #[test]
    fn many_options_grow_the_hint_row() {
        let opts: Vec<String> = (0..12).map(|i| format!("option-{i}")).collect();
        let h = ask_user_menu_height("Pick one:", &opts, 80, 24);
        // Hint becomes "Choices: [1] option-0  [2] option-1  …" which is
        // > 80 chars → at least 2 hint rows + 1 question row.
        assert!(h >= 3, "many options must wrap the hint row, got {h}");
    }

    /// Hard cap: menu can never exceed half the viewport height, even
    /// for absurdly long questions. Protects the history panel.
    #[test]
    fn height_is_capped_to_half_viewport() {
        let q = "x".repeat(10_000);
        let h = ask_user_menu_height(&q, &[], 80, 20);
        assert!(h <= 10, "must not exceed half viewport, got {h} > 10");
    }

    /// Cap floor: even on tiny terminals the menu is at least 2 rows
    /// (question + hint), matching the legacy minimum.
    #[test]
    fn minimum_height_is_two_rows() {
        let h = ask_user_menu_height("Q", &[], 80, 4);
        assert_eq!(h, 2);
    }

    /// Narrow terminal: question with 60 chars at width 30 wraps to
    /// 2+ rows. Catches off-by-one in the prefix-width accounting.
    #[test]
    fn narrow_terminal_wraps_short_question() {
        let q = "a".repeat(60);
        let h = ask_user_menu_height(&q, &[], 30, 24);
        assert!(
            h >= 3,
            "60 chars at width 30 should be ≥2 q-rows + 1 hint, got {h}"
        );
    }
}
