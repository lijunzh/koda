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

        // Non-bracketed paste-burst integration (#1186). Any modified key
        // (Ctrl/Alt + something, special keys, etc.) commits whatever's
        // accumulated so the user's modifier sequence acts on the full
        // pasted content rather than a stale prefix. Plain `Char` events
        // are routed through the burst detector by the `_` arm below.
        if !is_plain_insertable_char(key) {
            self.flush_paste_burst_before_modified_input();
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
                    && let Some(dd) = crate::composer::slash_popup::from_input(
                        crate::composer::slash_popup::SLASH_COMMANDS,
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
                if let Some(ch) = plain_char(key) {
                    // Non-bracketed paste-burst integration (#1186, Phase A).
                    // For plain typed chars we ask the burst detector to
                    // either swallow them into a buffer (when a paste-shaped
                    // burst is in progress) or pass them through unchanged.
                    // The buffer is committed in one shot by the timer arm
                    // in [`TuiContext::run`] once the inter-key timeout
                    // elapses, OR immediately when a non-Char key arrives
                    // (handled at the top of this function via
                    // `flush_paste_burst_before_modified_input`).
                    use crate::composer::paste_burst::CharDecision;
                    let now = std::time::Instant::now();
                    match self.paste_burst.on_plain_char_no_hold(now) {
                        Some(CharDecision::BeginBuffer { retro_chars }) => {
                            // Retroactively grab the most recent `retro_chars`
                            // already-inserted bytes from the textarea, hand
                            // them back to the burst buffer, and start the
                            // active accumulation with the current char.
                            self.retro_grab_into_paste_burst(retro_chars, ch, now);
                        }
                        Some(CharDecision::BufferAppend) => {
                            self.paste_burst.append_char_to_buffer(ch, now);
                        }
                        // RetainFirstChar / BeginBufferFromPending are only
                        // returned by `on_plain_char` (the held-first-char
                        // variant); `on_plain_char_no_hold` never produces
                        // them. Match exhaustively for clarity.
                        Some(CharDecision::RetainFirstChar)
                        | Some(CharDecision::BeginBufferFromPending) => {
                            self.textarea.input(key);
                        }
                        None => {
                            // Not bursting: insert normally.
                            self.textarea.input(key);
                        }
                    }
                } else {
                    self.textarea.input(key);
                }
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
        if let Some(idx) =
            crate::composer::history_nav::history_up_index(self.history_idx, self.history.len())
        {
            self.history_idx = Some(idx);
            self.textarea.set_text_clearing_elements("");
            self.textarea.insert_str(&self.history[idx]);
        }
    }

    fn history_down(&mut self) {
        let next =
            crate::composer::history_nav::history_down_index(self.history_idx, self.history.len());
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
            if let Some(dd) = crate::composer::slash_popup::from_input(
                crate::composer::slash_popup::SLASH_COMMANDS,
                trimmed,
            ) {
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

    // ── Paste-burst integration (#1186) ─────────────────────────

    /// Drain the paste-burst buffer when its inter-key timeout has elapsed.
    ///
    /// Called from the idle-loop's `tokio::select!` timer arm when
    /// `paste_burst.is_active()` is true. The flushed text is dropped into
    /// the textarea as one `insert_str` so the user sees the entire paste
    /// commit at once instead of character-by-character.
    pub(crate) fn flush_paste_burst_if_due(&mut self) {
        use crate::composer::paste_burst::FlushResult;
        let now = std::time::Instant::now();
        match self.paste_burst.flush_if_due(now) {
            FlushResult::Paste(s) => {
                if !s.is_empty() {
                    self.textarea.insert_str(&s);
                    self.update_reactive_menu();
                }
            }
            FlushResult::Typed(ch) => {
                // The detector was holding a single fast first char and no
                // burst followed before the timeout. Insert it as normal
                // typed input. (`on_plain_char_no_hold` never produces this
                // path, but defensive: another path may eventually call the
                // hold variant and we want the same flush handler to cope.)
                self.textarea.insert_str(&ch.to_string());
                self.update_reactive_menu();
            }
            FlushResult::None => {}
        }
    }

    /// Eagerly drain the paste-burst buffer before a non-Char key event
    /// (Ctrl-X, Enter, arrow keys, etc.) is dispatched.
    ///
    /// Without this, a paste followed immediately by a modifier sequence
    /// would lose the buffered text or have it inserted *after* the
    /// modifier action takes effect — both bad. Always commit pending
    /// burst content first.
    pub(crate) fn flush_paste_burst_before_modified_input(&mut self) {
        if let Some(pasted) = self.paste_burst.flush_before_modified_input()
            && !pasted.is_empty()
        {
            self.textarea.insert_str(&pasted);
            self.update_reactive_menu();
        }
    }

    /// Retroactively grab the most recent `retro_chars` already-inserted
    /// chars from the textarea and seed the paste-burst buffer with them
    /// (plus the current char `ch`). Mirrors codex's chat_composer.rs
    /// pattern for the BeginBuffer decision.
    fn retro_grab_into_paste_burst(&mut self, retro_chars: u16, ch: char, now: std::time::Instant) {
        let cur = self.textarea.cursor();
        let txt = self.textarea.text();
        let safe_cur = clamp_to_char_boundary(txt, cur);
        let before = &txt[..safe_cur];
        if let Some(grab) = self
            .paste_burst
            .decide_begin_buffer(now, before, retro_chars as usize)
        {
            if !grab.grabbed.is_empty() {
                self.textarea.replace_range(grab.start_byte..safe_cur, "");
            }
            // The paste detector is now active. Append the current
            // char (which never made it into the textarea) to the
            // buffer so it's part of the eventual paste commit.
            self.paste_burst.append_char_to_buffer(ch, now);
        } else {
            // Detector decided the prefix isn't paste-shaped enough
            // (no whitespace, < 16 chars). Insert the current char
            // as normal typed input so we don't lose it.
            self.textarea.input(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(ch),
                crossterm::event::KeyModifiers::NONE,
            ));
        }
    }
}

// ── Module-level helpers ───────────────────────────────────

/// Returns true when `key` is a plain insertable character (no Ctrl/Alt
/// modifiers, just `KeyCode::Char(_)` with at most Shift held). The
/// paste-burst detector only owns this slice of the keyspace; everything
/// else (Enter, arrows, Ctrl-X, etc.) flushes the burst eagerly.
fn is_plain_insertable_char(key: crossterm::event::KeyEvent) -> bool {
    plain_char(key).is_some()
}

/// Extract the plain `char` from a `KeyEvent` if it represents typed
/// text (not a modifier-augmented action). Mirrors `is_plain_insertable_char`
/// but returns the char itself, since the paste-burst path needs both the
/// classification and the value.
fn plain_char(key: crossterm::event::KeyEvent) -> Option<char> {
    use crossterm::event::{KeyCode, KeyModifiers};
    match (key.code, key.modifiers) {
        (KeyCode::Char(c), m)
            if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
        {
            Some(c)
        }
        _ => None,
    }
}

/// Clamp a byte offset down to the nearest char boundary in `s`. UTF-8
/// safety: `replace_range` panics if either bound is mid-codepoint, so we
/// always round the cursor down before slicing. Mirrors codex's
/// `Self::clamp_to_char_boundary` helper.
fn clamp_to_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx > s.len() {
        idx = s.len();
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

// ═══════════════════════════════════════════════════════════════
//  Tests
// ═══════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    // Bring file-private helpers (`plain_char`, `is_plain_insertable_char`,
    // `clamp_to_char_boundary`) into scope. Lost in the merge skew between
    // #1189 (added paste_burst tests using these helpers) and #1190
    // (composer extraction, which moved the history-nav tests out of
    // this module — carrying the `use super::*` with them).
    use super::*;

    // ── History DB persistence ───────────────────────────────────
    //
    // Pure index-math tests live in `crate::composer::history_nav` (#1187).
    // What stays here is the integration with `koda_core::db::Database`,
    // which the index helpers don't touch.

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

    // ── Paste-burst integration helpers (#1186) ────────────────────

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn plain_char_extracts_typed_char_with_no_modifiers() {
        assert_eq!(
            plain_char(key(KeyCode::Char('a'), KeyModifiers::NONE)),
            Some('a')
        );
        assert_eq!(
            plain_char(key(KeyCode::Char('A'), KeyModifiers::SHIFT)),
            Some('A')
        );
    }

    #[test]
    fn plain_char_rejects_modified_chars() {
        // Ctrl-c, Ctrl-d, etc. are session-control bindings; they MUST NOT
        // route through the paste-burst detector.
        assert_eq!(
            plain_char(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(
            plain_char(key(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(plain_char(key(KeyCode::Char('a'), KeyModifiers::ALT)), None);
    }

    #[test]
    fn plain_char_rejects_special_keys() {
        // Enter, arrows, function keys, etc. are non-Char codes; they
        // need their own match arms in handle_idle_key, not paste-burst.
        assert_eq!(plain_char(key(KeyCode::Enter, KeyModifiers::NONE)), None);
        assert_eq!(plain_char(key(KeyCode::Up, KeyModifiers::NONE)), None);
        assert_eq!(plain_char(key(KeyCode::Tab, KeyModifiers::NONE)), None);
        assert_eq!(plain_char(key(KeyCode::Esc, KeyModifiers::NONE)), None);
    }

    #[test]
    fn is_plain_insertable_char_agrees_with_plain_char() {
        // The classifier and the value-extractor must always agree, since
        // `handle_idle_key` uses the classifier as a guard for the
        // pre-flush call AND `_` arm uses the extractor for the routing
        // decision — a divergence would silently desync the two.
        let cases = [
            (key(KeyCode::Char('a'), KeyModifiers::NONE), true),
            (key(KeyCode::Char('A'), KeyModifiers::SHIFT), true),
            (key(KeyCode::Char('c'), KeyModifiers::CONTROL), false),
            (key(KeyCode::Enter, KeyModifiers::NONE), false),
            (key(KeyCode::Up, KeyModifiers::NONE), false),
        ];
        for (k, expected) in cases {
            assert_eq!(
                is_plain_insertable_char(k),
                expected,
                "classifier disagreed with extractor for {:?}",
                k
            );
            assert_eq!(plain_char(k).is_some(), expected);
        }
    }

    #[test]
    fn clamp_to_char_boundary_handles_ascii() {
        let s = "hello";
        assert_eq!(clamp_to_char_boundary(s, 0), 0);
        assert_eq!(clamp_to_char_boundary(s, 3), 3);
        assert_eq!(clamp_to_char_boundary(s, 5), 5);
        assert_eq!(
            clamp_to_char_boundary(s, 99),
            5,
            "out-of-bounds must clamp to s.len()"
        );
    }

    #[test]
    fn clamp_to_char_boundary_handles_multibyte() {
        // 'café' = c (1) + a (1) + f (1) + é (2 bytes) = 5 bytes.
        let s = "café";
        assert_eq!(s.len(), 5);
        // Position 4 is mid-codepoint (between the two bytes of é).
        // Must round DOWN to 3 (the byte just before é).
        assert_eq!(
            clamp_to_char_boundary(s, 4),
            3,
            "mid-codepoint cursor must round down to the previous boundary"
        );
        // Position 5 is end of string — already a boundary.
        assert_eq!(clamp_to_char_boundary(s, 5), 5);
    }
}
