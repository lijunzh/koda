//! Idle-mode event handling — keyboard, paste, menu navigation, wizard submit.
//!
//! Extracted from `TuiContext::run_event_loop()` (Step 3a, #447).
//! Handles all crossterm events received while `tui_state == Idle`.

use crate::input;
use crate::tui_context::{TuiContext, save_history};
use crate::tui_types::{MenuContent, PromptMode, ProviderWizard};
use crate::tui_viewport::{drain_pending_resizes, emit_above, scroll_past_and_reinit};

use crossterm::event::{Event, KeyCode, KeyModifiers};
use koda_core::approval::{self, ApprovalMode};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

impl TuiContext {
    /// Handle a single crossterm event while idle.
    ///
    /// Returns `Ok(true)` to continue the event loop (skip to next
    /// iteration), `Ok(false)` to fall through to normal flow.
    pub(crate) async fn handle_idle_event(&mut self, ev: Event) -> anyhow::Result<bool> {
        match ev {
            Event::Resize(_, _) => {
                let _ = drain_pending_resizes(&mut self.crossterm_events);
                scroll_past_and_reinit(
                    &mut self.terminal,
                    &mut self.crossterm_events,
                    self.viewport_height,
                )?;
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

    // ── Paste handling ───────────────────────────────────────────

    fn handle_idle_paste(&mut self, text: &str) {
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
            emit_above(
                &mut self.terminal,
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(label, Style::default().fg(Color::Yellow)),
                ]),
            );
            let preview: String = text.chars().take(80).collect();
            let preview = preview.replace('\n', "\u{21b5}");
            let preview = if char_count > 80 {
                format!("{preview}\u{2026}")
            } else {
                preview
            };
            emit_above(
                &mut self.terminal,
                Line::from(vec![
                    Span::raw("    "),
                    Span::styled(preview, Style::default().fg(Color::DarkGray)),
                ]),
            );
        }
    }

    // ── Key handling (idle) ─────────────────────────────────────

    async fn handle_idle_key(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> anyhow::Result<bool> {
        // Menu interception (when a dropdown is active)
        if !self.menu.is_none() {
            if let Some(consumed) = self.handle_menu_key(key).await {
                return Ok(consumed);
            }
            // Fall through to normal key handling
        }

        match (key.code, key.modifiers) {
            // Shift+Enter or Alt+Enter → insert newline
            (KeyCode::Enter, m)
                if m.contains(KeyModifiers::SHIFT) || m.contains(KeyModifiers::ALT) =>
            {
                self.textarea.insert_newline();
            }
            (KeyCode::Enter, KeyModifiers::NONE) => {
                return self.handle_idle_enter().await;
            }
            (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                self.history_up();
            }
            (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
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
                scroll_past_and_reinit(
                    &mut self.terminal,
                    &mut self.crossterm_events,
                    self.viewport_height,
                )?;
            }
            (KeyCode::BackTab, _) => {
                approval::cycle_mode(&self.shared_mode);
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

    // ── Enter key (idle) ───────────────────────────────────────

    async fn handle_idle_enter(&mut self) -> anyhow::Result<bool> {
        // Wizard input mode: submit value
        if matches!(self.prompt_mode, PromptMode::WizardInput { .. }) {
            self.handle_wizard_submit().await;
            return Ok(true);
        }

        // Chat submit
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
            emit_above(
                &mut self.terminal,
                Line::from(vec![
                    Span::styled(format!("{icon}> "), Style::default().fg(Color::Cyan)),
                    Span::raw(text.clone()),
                ]),
            );
            self.pending_command = Some(text);
        }
        Ok(true)
    }

    // ── Menu navigation ───────────────────────────────────────

    /// Handle a key when a menu/dropdown is active.
    ///
    /// Returns `Some(true)` if consumed (continue loop), `Some(false)` if
    /// menu was dismissed, or `None` to fall through to normal handling.
    async fn handle_menu_key(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> Option<bool> {
        let is_up = key.code == KeyCode::Up
            || (key.code == KeyCode::Char('k')
                && key.modifiers.contains(KeyModifiers::CONTROL));
        let is_down = key.code == KeyCode::Down
            || key.code == KeyCode::Tab
            || (key.code == KeyCode::Char('j')
                && key.modifiers.contains(KeyModifiers::CONTROL));

        if is_up {
            self.menu_navigate(-1);
            return Some(true);
        }
        if is_down {
            self.menu_navigate(1);
            return Some(true);
        }

        match key.code {
            KeyCode::Enter => {
                self.handle_menu_select().await;
                return Some(true);
            }
            KeyCode::Esc => {
                self.menu = MenuContent::None;
                if matches!(self.prompt_mode, PromptMode::WizardInput { .. }) {
                    self.prompt_mode = PromptMode::Chat;
                    self.provider_wizard = None;
                    self.textarea.select_all();
                    self.textarea.cut();
                }
                return Some(true);
            }
            _ => {}
        }
        None
    }

    /// Navigate the active menu up (dir = -1) or down (dir = 1).
    fn menu_navigate(&mut self, dir: i8) {
        macro_rules! nav {
            ($dd:expr) => {
                if dir < 0 { $dd.up() } else { $dd.down() }
            };
        }
        match &mut self.menu {
            MenuContent::Slash(dd) => nav!(dd),
            MenuContent::Model(dd) => nav!(dd),
            MenuContent::Provider(dd) => nav!(dd),
            MenuContent::Session(dd) => nav!(dd),
            MenuContent::File { dropdown: dd, .. } => nav!(dd),
            MenuContent::Approval { .. }
            | MenuContent::LoopCap
            | MenuContent::WizardTrail(_)
            | MenuContent::None => {}
        }
    }

    /// Handle Enter on the currently active menu/dropdown.
    async fn handle_menu_select(&mut self) {
        match &self.menu {
            MenuContent::Slash(dd) => {
                if let Some(item) = dd.selected_item() {
                    let cmd = item.command.to_string();
                    self.textarea.select_all();
                    self.textarea.cut();
                    self.textarea.insert_str(&cmd);
                }
            }
            MenuContent::Model(dd) => {
                if let Some(item) = dd.selected_item() {
                    let model_id = item.id.clone();
                    self.config.model = model_id.clone();
                    self.config.model_settings.model = model_id.clone();
                    self.config.recalculate_model_derived();
                    {
                        let prov = self.provider.read().await;
                        self.config
                            .query_and_apply_capabilities(prov.as_ref())
                            .await;
                    }
                    crate::tui_wizards::save_provider(&self.config);
                    emit_above(
                        &mut self.terminal,
                        Line::styled(
                            format!("  \u{2714} Model set to: {model_id}"),
                            Style::default().fg(Color::Green),
                        ),
                    );
                    self.renderer.model = model_id;
                }
            }
            MenuContent::Provider(dd) => {
                if let Some(item) = dd.selected_item() {
                    let key = item.key;
                    let ptype =
                        koda_core::config::ProviderType::from_url_or_name("", Some(key));
                    let base_url = ptype.default_base_url().to_string();
                    let provider_name = item.name.to_string();

                    if ptype.requires_api_key() {
                        let env_name = ptype.env_key_name().to_string();
                        let has_key = koda_core::runtime_env::is_set(&env_name);
                        let label = if has_key {
                            format!("API key for {} (Enter to keep current)", ptype)
                        } else {
                            format!("API key for {}", ptype)
                        };
                        self.menu = MenuContent::WizardTrail(vec![(
                            "Provider".into(),
                            provider_name,
                        )]);
                        self.prompt_mode = PromptMode::WizardInput {
                            label,
                            masked: true,
                        };
                        self.provider_wizard = Some(ProviderWizard::NeedApiKey {
                            provider_type: ptype,
                            base_url,
                            env_name,
                        });
                        self.textarea.select_all();
                        self.textarea.cut();
                    } else {
                        self.menu = MenuContent::WizardTrail(vec![(
                            "Provider".into(),
                            provider_name,
                        )]);
                        self.prompt_mode = PromptMode::WizardInput {
                            label: format!("{} URL", ptype),
                            masked: false,
                        };
                        self.provider_wizard = Some(ProviderWizard::NeedUrl {
                            provider_type: ptype,
                        });
                        self.textarea.select_all();
                        self.textarea.cut();
                        self.textarea.insert_str(&base_url);
                    }
                }
                // Provider dropdown transitions to wizard, don't clear menu
                return;
            }
            MenuContent::Session(dd) => {
                if let Some(item) = dd.selected_item() {
                    if item.is_current {
                        emit_above(
                            &mut self.terminal,
                            Line::styled(
                                "  Already in this session.",
                                Style::default().fg(Color::DarkGray),
                            ),
                        );
                    } else {
                        let target_id = item.id.clone();
                        let short = item.short_id.clone();
                        self.session.id = target_id;
                        emit_above(
                            &mut self.terminal,
                            Line::from(vec![
                                Span::styled(
                                    "  \u{2714} ",
                                    Style::default().fg(Color::Green),
                                ),
                                Span::raw("Resumed session "),
                                Span::styled(short, Style::default().fg(Color::Cyan)),
                            ]),
                        );
                    }
                }
            }
            MenuContent::File { dropdown, prefix } => {
                if let Some(item) = dropdown.selected_item() {
                    let replacement = format!("{}@{}", prefix, item.path);
                    self.textarea.select_all();
                    self.textarea.cut();
                    self.textarea.insert_str(&replacement);
                }
            }
            MenuContent::Approval { .. }
            | MenuContent::LoopCap
            | MenuContent::WizardTrail(_)
            | MenuContent::None => {}
        }
        self.menu = MenuContent::None;
    }

    // ── Wizard submit ──────────────────────────────────────────

    async fn handle_wizard_submit(&mut self) {
        let value = self.textarea.lines().join("");
        self.textarea.select_all();
        self.textarea.cut();

        if let Some(wizard) = self.provider_wizard.take() {
            match wizard {
                ProviderWizard::NeedApiKey {
                    provider_type,
                    base_url,
                    env_name,
                } => {
                    if value.is_empty() && !koda_core::runtime_env::is_set(&env_name) {
                        emit_above(
                            &mut self.terminal,
                            Line::styled(
                                "  \u{2716} No API key provided.",
                                Style::default().fg(Color::Red),
                            ),
                        );
                        self.prompt_mode = PromptMode::Chat;
                        self.menu = MenuContent::None;
                        return;
                    }
                    if !value.is_empty() {
                        koda_core::runtime_env::set(&env_name, &value);
                        if let Ok(mut store) = koda_core::keystore::KeyStore::load() {
                            store.set(&env_name, &value);
                            let _ = store.save();
                        }
                        let masked = koda_core::keystore::mask_key(&value);
                        emit_above(
                            &mut self.terminal,
                            Line::styled(
                                format!("  \u{2714} {env_name} set to {masked}"),
                                Style::default().fg(Color::Green),
                            ),
                        );
                    }
                    self.apply_provider(provider_type, base_url).await;
                }
                ProviderWizard::NeedUrl { provider_type } => {
                    let url = if value.is_empty() {
                        provider_type.default_base_url().to_string()
                    } else {
                        value
                    };
                    self.apply_provider(provider_type, url).await;
                }
            }
        }
        self.prompt_mode = PromptMode::Chat;
        self.menu = MenuContent::None;
    }

    /// Apply provider config, create the provider, and sync model info.
    async fn apply_provider(
        &mut self,
        provider_type: koda_core::config::ProviderType,
        base_url: String,
    ) {
        self.config.provider_type = provider_type.clone();
        self.config.base_url = base_url.clone();
        self.config.model = provider_type.default_model().to_string();
        self.config.model_settings.model = self.config.model.clone();
        self.config.recalculate_model_derived();
        *self.provider.write().await = koda_core::providers::create_provider(&self.config);
        crate::tui_wizards::save_provider(&self.config);

        let prov = self.provider.read().await;
        if let Ok(models) = prov.list_models().await {
            if let Some(first) = models.first() {
                self.config.model = first.id.clone();
                self.config.model_settings.model = self.config.model.clone();
                self.config.recalculate_model_derived();
            }
            self.config
                .query_and_apply_capabilities(prov.as_ref())
                .await;
            self.completer
                .set_model_names(models.iter().map(|m| m.id.clone()).collect());
        }
        self.renderer.model = self.config.model.clone();
        emit_above(
            &mut self.terminal,
            Line::styled(
                format!(
                    "  \u{2714} Provider: {} ({})",
                    self.config.provider_type, self.config.model
                ),
                Style::default().fg(Color::Green),
            ),
        );
    }

    // ── History navigation ──────────────────────────────────────

    fn history_up(&mut self) {
        if !self.history.is_empty() {
            let idx = match self.history_idx {
                None => self.history.len() - 1,
                Some(i) => i.saturating_sub(1),
            };
            self.history_idx = Some(idx);
            self.textarea.select_all();
            self.textarea.cut();
            self.textarea.insert_str(&self.history[idx]);
        }
    }

    fn history_down(&mut self) {
        if let Some(idx) = self.history_idx {
            if idx + 1 < self.history.len() {
                self.history_idx = Some(idx + 1);
                self.textarea.select_all();
                self.textarea.cut();
                self.textarea.insert_str(&self.history[idx + 1]);
            } else {
                self.history_idx = None;
                self.textarea.select_all();
                self.textarea.cut();
            }
        }
    }

    // ── Reactive menu updates ───────────────────────────────────

    /// Update menu state reactively after the user types a character.
    fn update_reactive_menu(&mut self) {
        let after_input = self.textarea.lines().join("\n");
        let trimmed = after_input.trim_end();

        if trimmed.starts_with('/') && !trimmed.contains(' ') {
            if let Some(dd) = crate::widgets::slash_menu::from_input(
                crate::completer::SLASH_COMMANDS,
                trimmed,
            ) {
                self.menu = MenuContent::Slash(dd);
            } else if matches!(self.menu, MenuContent::Slash(_)) {
                self.menu = MenuContent::None;
            }
        } else if let Some(at_pos) = crate::completer::find_last_at_token(trimmed) {
            let partial = &trimmed[at_pos + 1..];
            let prefix = &trimmed[..at_pos];
            let matches =
                crate::completer::list_path_matches_public(&self.project_root, partial);
            if !matches.is_empty() {
                let items: Vec<crate::widgets::file_menu::FileItem> = matches
                    .iter()
                    .map(|p| crate::widgets::file_menu::FileItem {
                        path: p.clone(),
                        is_dir: p.ends_with('/'),
                    })
                    .collect();
                let dd = crate::widgets::dropdown::DropdownState::new(
                    items,
                    "\u{1f4c2} Files",
                );
                self.menu = MenuContent::File {
                    dropdown: dd,
                    prefix: prefix.to_string(),
                };
            } else if matches!(self.menu, MenuContent::File { .. }) {
                self.menu = MenuContent::None;
            }
        } else if matches!(
            self.menu,
            MenuContent::Slash(_) | MenuContent::File { .. }
        ) {
            self.menu = MenuContent::None;
        }
    }
}
