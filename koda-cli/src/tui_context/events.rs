//! Input event handling — keyboard, mouse, paste, clipboard, history.
//!
//! All methods are `impl TuiContext` extensions for the idle-mode event loop.

use super::*;

impl TuiContext {
    pub(crate) async fn handle_idle_event(&mut self, ev: Event) -> anyhow::Result<bool> {
        match ev {
            Event::Resize(_, _) => {
                // Clamp scroll offset for the new terminal dimensions.
                // Use history panel height, not full terminal height.
                let (w, _) = self.term_dims();
                let vh = (self.history_area_height as usize).max(1);
                self.scroll_buffer.clamp_offset(w, vh);
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

        // Vim insert-mode Escape: route to the textarea so it transitions to
        // NORMAL mode. Without this short-circuit, the bare-Esc match arm
        // below (or the inference-loop's Esc-cancels-inference handler)
        // would consume the keystroke and the textarea would stay in INSERT,
        // making vim mode feel broken. PR 3 of #1178; the textarea owns the
        // "is this an insert-mode Esc that I should handle?" predicate so
        // we never need to mirror the vim_enabled/vim_mode state outside
        // the textarea.
        if self.textarea.should_handle_vim_insert_escape(key) {
            self.textarea.input(key);
            return Ok(true);
        }

        match (key.code, key.modifiers) {
            (KeyCode::Enter, m) if m.contains(KeyModifiers::ALT) => {
                self.textarea.insert_str("\n");
            }
            (KeyCode::Enter, KeyModifiers::NONE) => {
                return self.handle_idle_enter().await;
            }
            (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                self.history_up();
            }
            (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                let input = self.textarea.text().to_string();
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
                self.textarea.set_text_clearing_elements("");
                self.history_idx = None;
            }
            (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.textarea.set_text_clearing_elements("");
                self.history_idx = None;
            }
            (KeyCode::Char('d'), m) if m.contains(KeyModifiers::CONTROL) => {
                if self.textarea.text().to_string().trim().is_empty() {
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
            (KeyCode::BackTab, _) => {
                let new_mode = trust::cycle_trust(&self.shared_mode);
                let _ = self
                    .session
                    .db
                    .set_session_mode(&self.session.id, new_mode.as_str())
                    .await;
            }
            (KeyCode::Tab, KeyModifiers::NONE) => {
                let current = self.textarea.text().to_string();
                if let Some(completed) = self.completer.complete(&current) {
                    // Detect @-mention completion (PR 5 of #1178). The
                    // completer's `find_last_at_token` rule matches an `@`
                    // at start-of-input or after whitespace, and the
                    // returned text is `"{prefix}@{path}"` with the
                    // mention always reaching the end (no trailing space).
                    // When that pattern fires, insert the mention as a
                    // codex-port named element so:
                    //   - it renders cyan (textarea.render_lines overlays
                    //     element ranges automatically),
                    //   - one Backspace deletes the entire path atomically
                    //     (textarea.delete_backward snaps to atomic
                    //     boundaries),
                    //   - cycling Tab presses re-create the element with
                    //     the next match (set_text_clearing_elements +
                    //     insert_element naturally replaces the previous
                    //     element — no manual cleanup needed).
                    // Falls back to the plain insert_str path for slash
                    // commands and `/model <name>` completions.
                    if let Some(at_pos) = crate::completer::find_last_at_token(&completed) {
                        let (prefix, mention) = completed.split_at(at_pos);
                        self.textarea.set_text_clearing_elements(prefix);
                        // Defensive: place cursor at end of prefix so the
                        // mention is appended (not inserted at the textarea's
                        // last cursor position, which `set_text_inner` only
                        // clamps to text length — it never moves backward to
                        // the end on its own when the previous cursor was
                        // already inside the new shorter prefix).
                        self.textarea.set_cursor(prefix.len());
                        self.textarea.insert_element(mention);
                    } else {
                        self.textarea.set_text_clearing_elements("");
                        self.textarea.insert_str(&completed);
                    }
                    self.completer.reset();
                }
            }
            _ => {
                self.history_idx = None;
                self.completer.reset();
                self.textarea.input(key);
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

        let text = self.textarea.text().to_string();
        if !text.trim().is_empty() {
            self.textarea.set_text_clearing_elements("");
            self.history.push(text.clone());
            // Persist to DB (fire-and-forget)
            let _ = self.session.db.history_push(&text).await;
            self.history_idx = None;
            let mode = trust::read_trust(&self.shared_mode);
            let icon = match mode {
                TrustMode::Plan => "\u{1f4cb}",
                TrustMode::Safe => "\u{1f512}",
                TrustMode::Auto => "\u{26a1}",
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

    // ── History ────────────────────────────────────────────────

    fn history_up(&mut self) {
        if let Some(idx) = history_up_index(self.history_idx, self.history.len()) {
            self.history_idx = Some(idx);
            self.textarea.set_text_clearing_elements("");
            self.textarea.insert_str(&self.history[idx]);
        }
    }

    fn history_down(&mut self) {
        let next = history_down_index(self.history_idx, self.history.len());
        self.history_idx = next;
        self.textarea.set_text_clearing_elements("");
        if let Some(idx) = next {
            self.textarea.insert_str(&self.history[idx]);
        }
    }

    // ── Reactive menu updates ───────────────────────────────────

    fn update_reactive_menu(&mut self) {
        let after_input = self.textarea.text().to_string();
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

    // ── History DB persistence ────────────────────────────────

    #[tokio::test]
    async fn test_history_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let db = koda_core::db::Database::init(tmp.path()).await.unwrap();

        db.history_push("hello").await.unwrap();
        db.history_push("world").await.unwrap();
        db.history_push("/model gpt-4").await.unwrap();

        let loaded = db.history_load().await.unwrap();
        assert_eq!(loaded, vec!["hello", "world", "/model gpt-4"]);
    }

    #[tokio::test]
    async fn test_history_empty_db() {
        let tmp = tempfile::tempdir().unwrap();
        let db = koda_core::db::Database::init(tmp.path()).await.unwrap();

        let loaded = db.history_load().await.unwrap();
        assert!(loaded.is_empty());
    }

    // ── @-mention named-element wiring (PR 5 of #1178) ───────────────────
    //
    // Tests the textarea-side behavior the Tab handler relies on without
    // spinning up a full `TuiContext`. The handler does:
    //   if let Some(at_pos) = find_last_at_token(&completed) {
    //       let (prefix, mention) = completed.split_at(at_pos);
    //       textarea.set_text_clearing_elements(prefix);
    //       textarea.insert_element(mention);
    //   }
    // Each test exercises a different invariant of that flow.

    use crate::completer::find_last_at_token;
    use crate::composer::textarea::TextArea;

    /// Simulates the Tab handler's @-mention branch end-to-end, returning
    /// the resulting textarea so tests can assert on text + element state.
    fn simulate_at_mention_completion(prefix: &str, mention: &str) -> TextArea {
        let completed = format!("{prefix}{mention}");
        let at_pos = find_last_at_token(&completed)
            .expect("test fixture must include an @-token in the completed text");
        let (split_prefix, split_mention) = completed.split_at(at_pos);
        let mut ta = TextArea::new();
        ta.set_text_clearing_elements(split_prefix);
        ta.set_cursor(split_prefix.len());
        ta.insert_element(split_mention);
        ta
    }

    /// After Tab on `explain @src/m\t` the textarea text equals the full
    /// completion AND the `@src/main.rs` portion is registered as exactly
    /// one named element. This is the core PR 5 invariant.
    #[test]
    fn at_mention_completion_creates_one_element_for_the_path() {
        let ta = simulate_at_mention_completion("explain ", "@src/main.rs");
        assert_eq!(ta.text(), "explain @src/main.rs");
        let elements = ta.text_elements();
        assert_eq!(
            elements.len(),
            1,
            "exactly one element must wrap the @-mention"
        );
        let elem = &elements[0];
        assert_eq!(
            &ta.text()[elem.byte_range.start..elem.byte_range.end],
            "@src/main.rs",
            "element range must cover the full @-mention including the @"
        );
    }

    /// Cycling Tab (e.g. `main.rs` → `lib.rs`) replaces the previous
    /// element rather than accumulating two. The handler clears the
    /// textarea via `set_text_clearing_elements` before inserting the new
    /// mention, which by design drops all prior elements.
    #[test]
    fn cycling_at_mention_completion_replaces_previous_element() {
        let mut ta = simulate_at_mention_completion("explain ", "@src/main.rs");
        // Second Tab cycles to a different file — simulate by re-running the
        // same handler logic on the fresh "completed" string.
        let completed = "explain @src/lib.rs";
        let at_pos = find_last_at_token(completed).unwrap();
        let (prefix, mention) = completed.split_at(at_pos);
        ta.set_text_clearing_elements(prefix);
        ta.set_cursor(prefix.len());
        ta.insert_element(mention);

        assert_eq!(ta.text(), "explain @src/lib.rs");
        let elements = ta.text_elements();
        assert_eq!(elements.len(), 1, "cycling must replace, not accumulate");
        assert_eq!(
            &ta.text()[elements[0].byte_range.start..elements[0].byte_range.end],
            "@src/lib.rs"
        );
    }

    /// One Backspace at end-of-element deletes the entire `@path` span
    /// atomically (the codex-port `delete_backward` snaps to atomic
    /// boundaries, of which an element start is one). This is the
    /// user-visible UX win that motivated PR 5: no more 12 backspaces
    /// to undo a long file path.
    #[test]
    fn one_backspace_deletes_entire_at_mention_atomically() {
        let mut ta = simulate_at_mention_completion("explain ", "@src/main.rs");
        ta.delete_backward(1);
        assert_eq!(
            ta.text(),
            "explain ",
            "single backspace must wipe the whole @-mention element"
        );
        assert!(
            ta.text_elements().is_empty(),
            "deleting the element must also remove it from the element list"
        );
    }
}
