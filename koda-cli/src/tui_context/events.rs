//! Input event handling — keyboard, mouse, paste, clipboard, history.
//!
//! All methods are `impl TuiContext` extensions for the idle-mode event loop.

use super::*;

impl TuiContext {
    pub(crate) async fn handle_idle_event(&mut self, ev: Event) -> anyhow::Result<bool> {
        match ev {
            Event::Resize(_, _) => {
                // Clamp scroll offset for the new terminal dimensions.
                let (w, h) = self.term_dims();
                self.scroll_buffer.clamp_offset(w, h);
            }
            Event::Mouse(mouse) => {
                self.handle_mouse(mouse);
            }
            Event::Paste(text) => {
                self.handle_idle_paste(&text);
            }
            Event::Key(key) => {
                return self.handle_idle_key(key).await;
            }
            _ => {}
        }
        Ok(true)
    }

    pub(crate) fn handle_idle_paste(&mut self, text: &str) {
        let char_count = text.chars().count();
        if matches!(self.prompt_mode, PromptMode::WizardInput { .. })
            || char_count < input::PASTE_BLOCK_THRESHOLD
        {
            self.textarea.insert_str(text);
        } else {
            self.paste_blocks.push(input::PasteBlock {
                content: text.to_string(),
                char_count,
            });
            let label = format!("\u{1f4cb} Pasted text ({char_count} chars)");
            self.scroll_buffer.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(label, Style::default().fg(Color::Yellow)),
            ]));
            let preview: String = text.chars().take(80).collect();
            let preview = preview.replace('\n', "\u{21b5}");
            let preview = if char_count > 80 {
                format!("{preview}\u{2026}")
            } else {
                preview
            };
            self.scroll_buffer.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(preview, Style::default().fg(Color::DarkGray)),
            ]));
        }
    }

    pub(crate) async fn handle_idle_key(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> anyhow::Result<bool> {
        if !self.menu.is_none()
            && let Some(consumed) = self.handle_menu_key(key).await
        {
            return Ok(consumed);
        }

        match (key.code, key.modifiers) {
            (KeyCode::Enter, m) if m.contains(KeyModifiers::ALT) => {
                self.textarea.insert_newline();
            }
            (KeyCode::Enter, KeyModifiers::NONE) => {
                return self.handle_idle_enter().await;
            }
            (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                self.history_up();
            }
            (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                let input = self.textarea.lines().join("\n");
                let trimmed = input.trim_end();
                // ↓ on a bare `/`-prefixed token opens the slash menu instead
                // of scrolling history — matches CC behaviour.
                if trimmed.starts_with('/')
                    && !trimmed.contains(' ')
                    && let Some(dd) = crate::widgets::slash_menu::from_input(
                        crate::completer::SLASH_COMMANDS,
                        trimmed,
                    )
                {
                    self.menu = MenuContent::Slash(dd);
                    return Ok(true);
                }
                self.history_down();
            }
            (KeyCode::Esc, _) => {
                self.textarea.select_all();
                self.textarea.cut();
                self.history_idx = None;
            }
            (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.textarea.select_all();
                self.textarea.cut();
                self.history_idx = None;
            }
            (KeyCode::Char('d'), m) if m.contains(KeyModifiers::CONTROL) => {
                if self.textarea.lines().join("").trim().is_empty() {
                    self.should_quit = true;
                }
            }
            (KeyCode::Char('l'), m) if m.contains(KeyModifiers::CONTROL) => {
                // Ctrl+L: jump to bottom (re-engage sticky)
                self.scroll_buffer.scroll_to_bottom();
            }
            (KeyCode::Char('r'), m) if m.contains(KeyModifiers::CONTROL) => {
                // Ctrl+R: reverse history search (closes any open menu first)
                self.open_history_search();
            }
            // Scroll keys
            (KeyCode::PageUp, _) => {
                let (w, h) = self.term_dims();
                self.scroll_buffer.scroll_up(20, w, h);
            }
            (KeyCode::PageDown, _) => {
                self.scroll_buffer.scroll_down(20);
            }
            (KeyCode::Home, _) => {
                let (w, h) = self.term_dims();
                self.scroll_buffer.scroll_to_top(w, h);
            }
            (KeyCode::End, _) => {
                self.scroll_buffer.scroll_to_bottom();
            }
            // Clipboard: Ctrl+Y = copy last code block, Ctrl+U = copy last response
            (KeyCode::Char('y'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.copy_to_clipboard(m.contains(KeyModifiers::SHIFT));
            }
            (KeyCode::Char('u'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.copy_to_clipboard(true);
            }
            (KeyCode::BackTab, _) => {
                let new_mode = approval::cycle_mode(&self.shared_mode);
                let _ = self
                    .session
                    .db
                    .set_session_mode(&self.session.id, new_mode.as_str())
                    .await;
            }
            (KeyCode::Tab, KeyModifiers::NONE) => {
                let current = self.textarea.lines().join("\n");
                if let Some(completed) = self.completer.complete(&current) {
                    self.textarea.select_all();
                    self.textarea.cut();
                    self.textarea.insert_str(&completed);
                    self.completer.reset();
                }
            }
            _ => {
                self.history_idx = None;
                self.completer.reset();
                self.textarea.input(Event::Key(key));
                self.update_reactive_menu();
            }
        }
        Ok(true)
    }

    pub(crate) async fn handle_idle_enter(&mut self) -> anyhow::Result<bool> {
        if matches!(self.prompt_mode, PromptMode::WizardInput { .. }) {
            self.handle_wizard_submit().await;
            return Ok(true);
        }

        let text = self.textarea.lines().join("\n");
        if !text.trim().is_empty() {
            self.textarea.select_all();
            self.textarea.cut();
            self.history.push(text.clone());
            save_history(&self.history);
            self.history_idx = None;
            let mode = approval::read_mode(&self.shared_mode);
            let icon = match mode {
                ApprovalMode::Confirm => "\u{1f512}",
                ApprovalMode::Auto => "\u{26a1}",
            };
            self.scroll_buffer.push(Line::from(vec![
                Span::styled(format!("{icon}> "), Style::default().fg(Color::Cyan)),
                Span::raw(text.clone()),
            ]));
            self.pending_command = Some(text);
        }
        Ok(true)
    }

    // ── History navigation ──────────────────────────────────────

    /// Terminal (width, height) for scroll math.
    pub(crate) fn term_dims(&self) -> (usize, usize) {
        let size = self.terminal.size().unwrap_or_default();
        (size.width as usize, size.height as usize)
    }

    // ── Mouse handling ────────────────────────────────────────

    fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent) {
        use crate::mouse_select::{Selection, VisualPos};
        use crossterm::event::{MouseButton, MouseEventKind};

        let (w, _) = self.term_dims();
        let hist_y = self.history_area_y;
        let hist_h = self.history_area_height;

        // Check if mouse is in the history area
        let in_history = mouse.row >= hist_y && mouse.row < hist_y + hist_h;

        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_buffer.scroll_up(3, w, hist_h as usize),
            MouseEventKind::ScrollDown => self.scroll_buffer.scroll_down(3),

            MouseEventKind::Down(MouseButton::Left) if in_history => {
                // Capture scroll position ONCE at click time; reuse for all
                // subsequent Drag events so coordinates stay stable even if
                // new lines are pushed during inference.
                let scroll_from_top = self.scroll_buffer.paragraph_scroll(hist_h as usize, w).0;
                let screen_row = mouse.row.saturating_sub(hist_y);
                let buffer_row = screen_row.saturating_add(scroll_from_top);
                self.mouse_selection = Some(Selection {
                    anchor: VisualPos {
                        row: buffer_row,
                        col: mouse.column,
                    },
                    cursor: VisualPos {
                        row: buffer_row,
                        col: mouse.column,
                    },
                    scroll_from_top,
                });
            }

            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(sel) = &mut self.mouse_selection {
                    // Auto-scroll when dragging above or below the history area
                    if mouse.row < hist_y {
                        self.scroll_buffer.scroll_up(1, w, hist_h as usize);
                    } else if mouse.row >= hist_y + hist_h {
                        self.scroll_buffer.scroll_down(1);
                    }

                    // Refresh scroll position after possible auto-scroll so
                    // the cursor tracks the new viewport, while the anchor
                    // (already in buffer space) stays pinned.
                    sel.scroll_from_top = self.scroll_buffer.paragraph_scroll(hist_h as usize, w).0;

                    // Clamp screen row to the history area bounds
                    let screen_row = mouse
                        .row
                        .max(hist_y)
                        .min(hist_y + hist_h.saturating_sub(1))
                        .saturating_sub(hist_y);
                    let buffer_row = screen_row.saturating_add(sel.scroll_from_top);
                    sel.cursor = VisualPos {
                        row: buffer_row,
                        col: mouse.column,
                    };
                }
            }

            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(sel) = self.mouse_selection.take() {
                    // Only copy if the selection spans more than a click
                    if sel.anchor != sel.cursor {
                        let lines: Vec<Line<'_>> =
                            self.scroll_buffer.all_lines().cloned().collect();
                        let gutter_ws: Vec<u16> =
                            self.scroll_buffer.gutter_widths().iter().copied().collect();
                        let (all_rows, all_gutters) =
                            crate::mouse_select::build_all_visual_rows(&lines, &gutter_ws, w);
                        let text = crate::mouse_select::extract_selected_text(
                            &all_rows,
                            &all_gutters,
                            &sel,
                        );
                        if !text.is_empty() {
                            match crate::mouse_select::copy_to_clipboard(&text) {
                                Ok(msg) => {
                                    self.scroll_buffer.push(Line::from(vec![
                                        Span::styled(
                                            "  \u{1f4cb} ",
                                            Style::default().fg(Color::Green),
                                        ),
                                        Span::styled(msg, Style::default().fg(Color::Green)),
                                    ]));
                                }
                                Err(e) => {
                                    tracing::warn!("clipboard copy failed: {e}");
                                }
                            }
                        }
                    }
                }
            }

            _ => {}
        }
    }

    // ── Clipboard ─────────────────────────────────────────────

    fn copy_to_clipboard(&mut self, shift: bool) {
        let text = if shift {
            self.scroll_buffer.last_response()
        } else {
            self.scroll_buffer.last_code_block()
        };
        match text {
            Some(content) => {
                match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(&content)) {
                    Ok(()) => {
                        let label = if shift { "response" } else { "code block" };
                        let preview: String = content.chars().take(60).collect();
                        self.scroll_buffer.push(Line::from(vec![
                            Span::styled("  \u{1f4cb} ", Style::default().fg(Color::Green)),
                            Span::styled(
                                format!("Copied {label} to clipboard"),
                                Style::default().fg(Color::Green),
                            ),
                            Span::styled(
                                format!(" ({preview}…)"),
                                Style::default().fg(Color::DarkGray),
                            ),
                        ]));
                    }
                    Err(e) => {
                        self.scroll_buffer.push(Line::styled(
                            format!("  \u{2717} Clipboard error: {e}"),
                            Style::default().fg(Color::Red),
                        ));
                    }
                }
            }
            None => {
                let label = if shift {
                    "No response to copy."
                } else {
                    "No code block to copy."
                };
                self.scroll_buffer.push(Line::styled(
                    format!("  {label}"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
        }
    }

    // ── History ────────────────────────────────────────────────

    fn history_up(&mut self) {
        if let Some(idx) = history_up_index(self.history_idx, self.history.len()) {
            self.history_idx = Some(idx);
            self.textarea.select_all();
            self.textarea.cut();
            self.textarea.insert_str(&self.history[idx]);
        }
    }

    fn history_down(&mut self) {
        let next = history_down_index(self.history_idx, self.history.len());
        self.history_idx = next;
        self.textarea.select_all();
        self.textarea.cut();
        if let Some(idx) = next {
            self.textarea.insert_str(&self.history[idx]);
        }
    }

    // ── Reactive menu updates ───────────────────────────────────

    fn update_reactive_menu(&mut self) {
        let after_input = self.textarea.lines().join("\n");
        let trimmed = after_input.trim_end();

        if trimmed.starts_with('/') && !trimmed.contains(' ') {
            if let Some(dd) =
                crate::widgets::slash_menu::from_input(crate::completer::SLASH_COMMANDS, trimmed)
            {
                self.menu = MenuContent::Slash(dd);
            } else if matches!(self.menu, MenuContent::Slash(_)) {
                self.menu = MenuContent::None;
            }
        } else if let Some(at_pos) = crate::completer::find_last_at_token(trimmed) {
            let partial = &trimmed[at_pos + 1..];
            let prefix = &trimmed[..at_pos];
            let matches = crate::completer::list_path_matches_public(&self.project_root, partial);
            if !matches.is_empty() {
                let items: Vec<crate::widgets::file_menu::FileItem> = matches
                    .iter()
                    .map(|p| crate::widgets::file_menu::FileItem {
                        path: p.clone(),
                        is_dir: p.ends_with('/'),
                    })
                    .collect();
                let dd = crate::widgets::dropdown::DropdownState::new(items, "\u{1f4c2} Files");
                self.menu = MenuContent::File {
                    dropdown: dd,
                    prefix: prefix.to_string(),
                };
            } else if matches!(self.menu, MenuContent::File { .. }) {
                self.menu = MenuContent::None;
            }
        } else if matches!(self.menu, MenuContent::Slash(_) | MenuContent::File { .. }) {
            self.menu = MenuContent::None;
        }
    }
}

// ---------------------------------------------------------------------------
// Command history persistence
// ---------------------------------------------------------------------------

const MAX_HISTORY: usize = 500;
fn history_file_path() -> std::path::PathBuf {
    let config_dir = std::env::var("XDG_CONFIG_HOME")
        .or_else(|_| std::env::var("HOME").map(|h| format!("{h}/.config")))
        .or_else(|_| std::env::var("USERPROFILE").map(|h| format!("{h}/.config")))
        .unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(config_dir)
        .join("koda")
        .join("history")
}

pub(crate) fn load_history() -> Vec<String> {
    load_history_from(&history_file_path())
}

fn load_history_from(path: &std::path::Path) -> Vec<String> {
    match std::fs::read_to_string(path) {
        Ok(content) => content
            .lines()
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect(),
        Err(_) => Vec::new(),
    }
}

pub(crate) fn save_history(history: &[String]) {
    save_history_to(history, &history_file_path());
}

fn save_history_to(history: &[String], path: &std::path::Path) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let start = history.len().saturating_sub(MAX_HISTORY);
    let content = history[start..].join("\n");
    let _ = std::fs::write(path, content);
}

// ---------------------------------------------------------------------------
// Pure helper: history index navigation
// ---------------------------------------------------------------------------

/// Compute the next history index when pressing Up (older).
///
/// Returns `None` if the history is empty.
pub(crate) fn history_up_index(current: Option<usize>, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match current {
        None => len - 1,
        Some(i) => i.saturating_sub(1),
    })
}

/// Compute the next history index when pressing Down (newer).
///
/// Returns `None` when moving past the most recent entry (back to
/// empty input).
pub(crate) fn history_down_index(current: Option<usize>, len: usize) -> Option<usize> {
    match current {
        Some(i) if i + 1 < len => Some(i + 1),
        _ => None,
    }
}

// ═══════════════════════════════════════════════════════════════
//  Tests
// ═══════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    // ── History index navigation ──────────────────────────────

    #[test]
    fn test_history_up_from_none() {
        assert_eq!(history_up_index(None, 5), Some(4));
    }

    #[test]
    fn test_history_up_from_middle() {
        assert_eq!(history_up_index(Some(3), 5), Some(2));
    }

    #[test]
    fn test_history_up_at_top() {
        // Saturates at 0
        assert_eq!(history_up_index(Some(0), 5), Some(0));
    }

    #[test]
    fn test_history_up_empty() {
        assert_eq!(history_up_index(None, 0), None);
    }

    #[test]
    fn test_history_down_from_middle() {
        assert_eq!(history_down_index(Some(2), 5), Some(3));
    }

    #[test]
    fn test_history_down_at_bottom() {
        // Past the last entry → back to empty input
        assert_eq!(history_down_index(Some(4), 5), None);
    }

    #[test]
    fn test_history_down_from_none() {
        assert_eq!(history_down_index(None, 5), None);
    }

    // ── History file persistence ──────────────────────────────

    #[test]
    fn test_history_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("koda").join("history");

        let input = vec!["hello".into(), "world".into(), "/model gpt-4".into()];
        save_history_to(&input, &path);

        let loaded = load_history_from(&path);
        assert_eq!(loaded, input);
        assert!(path.exists());
    }

    #[test]
    fn test_history_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("koda").join("history");

        let big: Vec<String> = (0..MAX_HISTORY + 100).map(|i| format!("cmd {i}")).collect();
        save_history_to(&big, &path);

        let loaded = load_history_from(&path);
        assert_eq!(loaded.len(), MAX_HISTORY);
        // Should keep the LATEST entries (tail)
        assert_eq!(loaded[0], format!("cmd {}", 100));
        assert_eq!(loaded[MAX_HISTORY - 1], format!("cmd {}", MAX_HISTORY + 99));
    }

    #[test]
    fn test_load_history_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent").join("history");

        let loaded = load_history_from(&path);
        assert!(loaded.is_empty());
    }
}
