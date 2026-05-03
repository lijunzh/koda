//! Menu navigation, dropdown pickers, and wizard state machines.
//!
//! All methods are `impl TuiContext` extensions for menu/wizard interaction.

use super::*;

/// Available providers for the interactive picker.
///
/// Tuple: `(internal_key, display_name)`. Descriptions like "Local, no API key"
/// are derived from `ProviderType::requires_api_key()` at render time.
pub(crate) const PROVIDERS: &[(&str, &str)] = &[
    ("lmstudio", "LM Studio"),
    ("ollama", "Ollama"),
    ("openai", "OpenAI"),
    ("anthropic", "Anthropic"),
    ("deepseek", "DeepSeek"),
    ("gemini", "Google Gemini"),
    ("groq", "Groq"),
    ("grok", "Grok (xAI)"),
    ("mistral", "Mistral"),
    ("minimax", "MiniMax"),
    ("openrouter", "OpenRouter"),
    ("together", "Together"),
    ("fireworks", "Fireworks"),
    ("vllm", "vLLM"),
];

// ── History search helpers ────────────────────────────────────────────────────

/// Return up to 6 history entries (newest first) containing `query`
/// (case-insensitive substring match). Empty query → no results.
fn history_search_matches(history: &[String], query: &str) -> Vec<String> {
    if query.is_empty() {
        return Vec::new();
    }
    let q = query.to_lowercase();
    history
        .iter()
        .rev()
        .filter(|s| s.to_lowercase().contains(&q))
        .take(6)
        .cloned()
        .collect()
}

impl TuiContext {
    // ── Dropdown openers ─────────────────────────────────────────────

    /// Enter Ctrl+R reverse history search mode.
    pub(crate) fn open_history_search(&mut self) {
        self.menu = MenuContent::HistorySearch {
            query: String::new(),
            matches: Vec::new(),
            selected: 0,
        };
        // Clear the input so the best match can be previewed.
        self.textarea.set_text_clearing_elements("");
    }

    /// Update the textarea to preview the currently selected match.
    fn sync_search_textarea(&mut self) {
        let (text, cursor) = match &self.menu {
            MenuContent::HistorySearch {
                matches, selected, ..
            } => (matches.get(*selected).cloned(), *selected),
            _ => return,
        };
        let _ = cursor; // suppresses unused warning if selected unused
        self.textarea.set_text_clearing_elements("");
        if let Some(t) = text {
            self.textarea.insert_str(&t);
        }
    }

    pub(crate) async fn open_model_picker(&mut self) {
        // Try to auto-detect local model for the "local" alias
        let local_model = self.detect_local_model().await;
        let items =
            crate::widgets::model_menu::build_items(&self.config.model, local_model.as_deref());
        let mut dd =
            crate::widgets::dropdown::DropdownState::new(items, "\u{1f43b} Select a model");
        if let Some(idx) = dd.filtered.iter().position(|m| m.is_current) {
            dd.selected = idx;
            let max_vis = crate::widgets::dropdown::MAX_VISIBLE;
            if idx >= max_vis {
                dd.scroll_offset = idx + 1 - max_vis;
            }
        }
        self.menu = MenuContent::Model(dd);
    }

    /// Try to detect a locally running LMStudio model.
    async fn detect_local_model(&self) -> Option<String> {
        let ptype = koda_core::config::ProviderType::LMStudio;
        let url = koda_core::runtime_env::get("KODA_LOCAL_URL")
            .unwrap_or_else(|| ptype.default_base_url().to_string());
        let mut temp_config = self.config.clone();
        temp_config.provider_type = ptype;
        temp_config.base_url = url;
        let temp_provider = koda_core::providers::create_provider(&temp_config);
        let models = temp_provider.list_models().await.ok()?;
        models.first().map(|m| m.id.clone())
    }

    pub(crate) fn open_provider_picker(&mut self) {
        let providers = PROVIDERS;
        let items: Vec<crate::widgets::provider_menu::ProviderItem> = providers
            .iter()
            .map(|(key, name)| {
                let ptype = koda_core::config::ProviderType::from_url_or_name("", Some(key));
                crate::widgets::provider_menu::ProviderItem {
                    key,
                    name,
                    local: !ptype.requires_api_key(),
                    key_set: false,
                }
            })
            .collect();
        let dd = crate::widgets::dropdown::DropdownState::new(items, "\u{1f43b} Select a provider");
        self.menu = MenuContent::Provider(dd);
    }

    pub(crate) async fn open_key_picker(&mut self) {
        // Only show cloud providers (ones that need API keys)
        let providers = PROVIDERS;
        let items: Vec<crate::widgets::provider_menu::ProviderItem> = providers
            .iter()
            .filter(|&&(name, _)| {
                let ptype = koda_core::config::ProviderType::from_url_or_name("", Some(name));
                ptype.requires_api_key()
            })
            .map(|&(key, name)| {
                let ptype = koda_core::config::ProviderType::from_url_or_name("", Some(key));
                let has_key = koda_core::runtime_env::is_set(ptype.env_key_name());
                crate::widgets::provider_menu::ProviderItem {
                    key,
                    name,
                    local: false,
                    key_set: has_key,
                }
            })
            .collect();
        let dd = crate::widgets::dropdown::DropdownState::new(items, "\u{1f511} Set API key for");
        self.menu = MenuContent::Key(dd);
    }

    pub(crate) fn start_provider_wizard(&mut self, name: &str) {
        let ptype = koda_core::config::ProviderType::from_url_or_name("", Some(name));
        let base_url = ptype.default_base_url().to_string();
        let provider_name = ptype.to_string();

        if ptype.requires_api_key() {
            let env_name = ptype.env_key_name().to_string();
            let has_key = koda_core::runtime_env::is_set(&env_name);
            let label = if has_key {
                format!("API key for {} (Enter to keep current)", ptype)
            } else {
                format!("API key for {}", ptype)
            };
            self.menu = MenuContent::WizardTrail(vec![("Provider".into(), provider_name)]);
            // PR 6 of #1178: API key entry is the one masked wizard
            // path. The textarea renders `*` chars instead of the
            // actual keystrokes so over-the-shoulder onlookers and
            // screen recordings don't capture the secret.
            self.prompt_mode = PromptMode::WizardInput { label, mask: true };
            self.provider_wizard = Some(ProviderWizard::ApiKey {
                provider_type: ptype,
                base_url,
                env_name,
            });
            self.textarea.set_text_clearing_elements("");
        } else {
            self.menu = MenuContent::WizardTrail(vec![("Provider".into(), provider_name)]);
            self.prompt_mode = PromptMode::WizardInput {
                label: format!("{} URL", ptype),
                mask: false,
            };
            self.provider_wizard = Some(ProviderWizard::Url {
                provider_type: ptype,
            });
            self.textarea.set_text_clearing_elements("");
            self.textarea.insert_str(&base_url);
        }
    }

    /// List all models from a specific provider (power-user path).
    async fn open_provider_model_list(&mut self, ptype: koda_core::config::ProviderType) {
        let base_url = ptype.default_base_url().to_string();

        // Temporarily create a provider to list its models
        let temp_config = {
            let mut c = self.config.clone();
            c.provider_type = ptype;
            c.base_url = base_url;
            c
        };
        let temp_provider = koda_core::providers::create_provider(&temp_config);

        match temp_provider.list_models().await {
            Ok(models) if !models.is_empty() => {
                let items: Vec<crate::widgets::model_menu::ModelItem> = models
                    .iter()
                    .map(|m| crate::widgets::model_menu::ModelItem {
                        label: m.id.clone(),
                        model_id: m.id.clone(),
                        provider: ptype.to_string(),
                        is_current: m.id == self.config.model,
                        is_local: false,
                    })
                    .collect();
                let title = format!("\u{1f43b} {} models", ptype);
                let mut dd = crate::widgets::dropdown::DropdownState::new(items, &title);
                if let Some(idx) = dd.filtered.iter().position(|m| m.is_current) {
                    dd.selected = idx;
                    let max_vis = crate::widgets::dropdown::MAX_VISIBLE;
                    if idx >= max_vis {
                        dd.scroll_offset = idx + 1 - max_vis;
                    }
                }
                self.menu = MenuContent::ProviderModels(dd, ptype);
            }
            Ok(_) => {
                self.scroll_buffer.push(Line::styled(
                    format!("  \u{26a0} No models available from {ptype}"),
                    Style::default().fg(Color::Yellow),
                ));
            }
            Err(e) => {
                self.scroll_buffer.push(Line::styled(
                    format!("  \u{2717} Failed to list models from {ptype}: {e}"),
                    Style::default().fg(Color::Red),
                ));
            }
        }
    }

    pub(crate) async fn open_session_picker(&mut self) {
        match self.session.db.list_sessions(10, &self.project_root).await {
            Ok(sessions) if !sessions.is_empty() => {
                let items: Vec<crate::widgets::session_menu::SessionItem> = sessions
                    .iter()
                    .map(|s| crate::widgets::session_menu::SessionItem {
                        id: s.id.clone(),
                        short_id: s.id[..8.min(s.id.len())].to_string(),
                        created_at: s.created_at.clone(),
                        message_count: s.message_count,
                        total_tokens: s.total_tokens,
                        is_current: s.id == self.session.id,
                        title: s.title.clone(),
                    })
                    .collect();
                let mut dd =
                    crate::widgets::dropdown::DropdownState::new(items, "\u{1f43b} Sessions");
                if let Some(idx) = dd.filtered.iter().position(|s| s.is_current) {
                    dd.selected = idx;
                    let max_vis = crate::widgets::dropdown::MAX_VISIBLE;
                    if idx >= max_vis {
                        dd.scroll_offset = idx + 1 - max_vis;
                    }
                }
                self.menu = MenuContent::Session(dd);
            }
            Ok(_) => {
                self.scroll_buffer.push(Line::styled(
                    "  No other sessions found.",
                    Style::default().fg(Color::DarkGray),
                ));
            }
            Err(e) => {
                self.scroll_buffer.push(Line::styled(
                    format!("  \u{2717} Error: {e}"),
                    Style::default().fg(Color::Red),
                ));
            }
        }
    }

    // ── Menu navigation ───────────────────────────────────────

    pub(crate) async fn handle_menu_key(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> Option<bool> {
        // History search: intercept ALL keys while search overlay is open.
        if let MenuContent::HistorySearch {
            query,
            matches,
            selected,
        } = &mut self.menu
        {
            match (key.code, key.modifiers) {
                // Ctrl+R → older match (down the list)
                (KeyCode::Char('r'), m) if m.contains(KeyModifiers::CONTROL) => {
                    if *selected + 1 < matches.len() {
                        *selected += 1;
                    }
                }
                // Ctrl+S → newer match (up the list)
                (KeyCode::Char('s'), m) if m.contains(KeyModifiers::CONTROL) => {
                    if *selected > 0 {
                        *selected -= 1;
                    }
                }
                // Arrow keys to navigate
                (KeyCode::Up, _) => {
                    if *selected + 1 < matches.len() {
                        *selected += 1;
                    }
                }
                (KeyCode::Down, _) => {
                    if *selected > 0 {
                        *selected -= 1;
                    }
                }
                // Printable char → extend query
                (KeyCode::Char(c), m)
                    if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
                {
                    query.push(c);
                    let new_matches = history_search_matches(&self.history, query);
                    *selected = 0;
                    *matches = new_matches;
                }
                // Backspace → shrink query
                (KeyCode::Backspace, _) => {
                    query.pop();
                    let new_matches = history_search_matches(&self.history, query);
                    *selected = 0;
                    *matches = new_matches;
                }
                // Enter → accept (textarea already shows the match)
                (KeyCode::Enter, _) => {
                    self.menu = MenuContent::None;
                    return Some(true);
                }
                // Esc / Ctrl+G → cancel, clear textarea
                (KeyCode::Esc, _) | (KeyCode::Char('g'), KeyModifiers::CONTROL) => {
                    self.menu = MenuContent::None;
                    self.textarea.set_text_clearing_elements("");
                    return Some(true);
                }
                _ => return Some(true), // consume everything else
            }
            self.sync_search_textarea();
            return Some(true);
        }

        // Shortcuts overlay (#1194 / #1195): the keybinding cheat-sheet
        // popped up by `?`. Any keypress dismisses it — most usefully
        // `?` again (toggle), `Esc`, or any movement / typing key. We
        // consume the keystroke unconditionally so the dismissing key
        // doesn't ALSO trigger its normal action (e.g. pressing `j` to
        // close shouldn't insert `j` into the textarea).
        if matches!(self.menu, MenuContent::ShortcutsOverlay) {
            self.menu = MenuContent::None;
            return Some(true);
        }

        // (#1191's interactive `/agents` panel was removed in #1210 —
        // live bg-task surface migrated to the always-on activity
        // overlay above the status bar; cancellation now goes through
        // `/cancel <id>` (idle) or Esc cancel-all (during inference).)

        // Purge confirmation: only y/n/Esc are meaningful.
        if let MenuContent::PurgeConfirm { min_age_days, .. } = &self.menu {
            let days = *min_age_days;
            match key.code {
                KeyCode::Char('y' | 'Y') => {
                    crate::tui_wizards::execute_purge(&mut self.scroll_buffer, &self.session, days)
                        .await;
                    self.menu = MenuContent::None;
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                    crate::tui_output::dim_msg(&mut self.scroll_buffer, "Purge cancelled.".into());
                    self.menu = MenuContent::None;
                }
                _ => {}
            }
            return Some(true);
        }

        let is_up = key.code == KeyCode::Up
            || (key.code == KeyCode::Char('k') && key.modifiers.contains(KeyModifiers::CONTROL));
        let is_down = key.code == KeyCode::Down
            || key.code == KeyCode::Tab
            || (key.code == KeyCode::Char('j') && key.modifiers.contains(KeyModifiers::CONTROL));

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
                    self.textarea.set_text_clearing_elements("");
                }
                return Some(true);
            }
            _ => {}
        }
        None
    }

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
            MenuContent::ProviderModels(dd, _) => nav!(dd),
            MenuContent::Key(dd) => nav!(dd),
            MenuContent::Session(dd) => nav!(dd),
            MenuContent::File { dropdown: dd, .. } => nav!(dd),
            MenuContent::Approval { .. }
            | MenuContent::AskUser { .. }
            | MenuContent::LoopCap
            | MenuContent::PurgeConfirm { .. }
            | MenuContent::HistorySearch { .. }
            | MenuContent::WizardTrail(_)
            | MenuContent::ShortcutsOverlay
            | MenuContent::None => {}
        }
    }

    pub(crate) async fn handle_menu_select(&mut self) {
        match &self.menu {
            MenuContent::Slash(dd) => {
                // Extract owned/static data before dropping the borrow on self.menu.
                let action = dd
                    .selected_item()
                    .map(|item| (item.command.to_string(), item.arg_hint));
                // Borrow on self.menu is released here — item.command and
                // item.arg_hint are both &'static, so they don't extend it.
                if let Some((cmd, arg_hint)) = action {
                    self.menu = MenuContent::None;
                    self.textarea.set_text_clearing_elements("");
                    if arg_hint.is_some() {
                        // Command needs an argument: put "/cmd " in the box
                        // so the user can type the argument and press Enter.
                        self.textarea.insert_str(&format!("{cmd} "));
                    } else {
                        // Self-contained / picker-opener: execute immediately.
                        self.dispatch_slash(&cmd).await;
                        self.reinit_after_slash().await;
                    }
                }
                return; // skip the trailing self.menu = None
            }
            MenuContent::Model(dd) => {
                if let Some(item) = dd.selected_item() {
                    if item.is_local && item.model_id == "(not running)" {
                        self.scroll_buffer.push(Line::styled(
                            "  \u{26a0} LM Studio is not running. Start it and try again.",
                            Style::default().fg(Color::Yellow),
                        ));
                    } else {
                        let model_id = item.model_id.clone();
                        let label = item.label.clone();

                        // Resolve alias to get provider type
                        if let Some(resolved) = koda_core::model_alias::resolve(&label) {
                            let ptype = resolved.provider;
                            // Check API key for cloud providers
                            if ptype.requires_api_key()
                                && !koda_core::runtime_env::is_set(ptype.env_key_name())
                            {
                                self.scroll_buffer.push(Line::styled(
                                    format!(
                                        "  \u{2716} {} not set. Run /key to configure.",
                                        ptype.env_key_name()
                                    ),
                                    Style::default().fg(Color::Red),
                                ));
                            } else {
                                let actual_model = if resolved.needs_auto_detect() {
                                    model_id.clone()
                                } else {
                                    resolved.model_id.to_string()
                                };
                                self.apply_provider(ptype, ptype.default_base_url().to_string())
                                    .await;
                                self.config.model = actual_model.clone();
                                self.config.model_settings.model = actual_model.clone();
                                self.config.recalculate_model_derived();
                                self.query_model_capabilities().await;
                                crate::tui_wizards::save_provider(&self.config, &self.session.db)
                                    .await;
                                self.renderer.model = actual_model.clone();
                                self.scroll_buffer.push(Line::styled(
                                    format!("  \u{2714} Model: {label} ({actual_model})"),
                                    Style::default().fg(Color::Green),
                                ));
                            }
                        } else {
                            // Not an alias — use model_id directly
                            self.config.model = model_id.clone();
                            self.config.model_settings.model = model_id.clone();
                            self.config.recalculate_model_derived();
                            self.query_model_capabilities().await;
                            crate::tui_wizards::save_provider(&self.config, &self.session.db).await;
                            self.renderer.model = model_id.clone();
                            self.scroll_buffer.push(Line::styled(
                                format!("  \u{2714} Model set to: {model_id}"),
                                Style::default().fg(Color::Green),
                            ));
                        }
                    }
                }
            }
            MenuContent::Provider(dd) => {
                if let Some(item) = dd.selected_item() {
                    let key = item.key;
                    let ptype = koda_core::config::ProviderType::from_url_or_name("", Some(key));

                    if ptype.requires_api_key()
                        && !koda_core::runtime_env::is_set(ptype.env_key_name())
                    {
                        // Need API key first — enter wizard
                        self.start_provider_wizard(key);
                    } else {
                        // Key is set (or not needed) — list models from this provider
                        self.open_provider_model_list(ptype).await;
                    }
                }
                return;
            }
            MenuContent::Key(dd) => {
                if let Some(item) = dd.selected_item() {
                    let ptype =
                        koda_core::config::ProviderType::from_url_or_name("", Some(item.key));
                    let env_name = ptype.env_key_name().to_string();
                    let provider_name = ptype.to_string();
                    let has_key = koda_core::runtime_env::is_set(&env_name);
                    let label = if has_key {
                        format!("API key for {} (Enter to keep current)", provider_name)
                    } else {
                        format!("API key for {}", provider_name)
                    };
                    self.menu = MenuContent::WizardTrail(vec![("Key".into(), provider_name)]);
                    // PR 6 of #1178: second API-key entry path (re-enter
                    // an existing key from the /key picker). Also masked.
                    self.prompt_mode = PromptMode::WizardInput { label, mask: true };
                    self.provider_wizard = Some(ProviderWizard::ApiKeyOnly { env_name });
                    self.textarea.set_text_clearing_elements("");
                }
                return;
            }
            MenuContent::Session(dd) => {
                if let Some(item) = dd.selected_item() {
                    if item.is_current {
                        self.scroll_buffer.push(Line::styled(
                            "  Already in this session.",
                            Style::default().fg(Color::DarkGray),
                        ));
                    } else {
                        let target_id = item.id.clone();
                        let short = item.short_id.clone();
                        self.session.id = target_id;
                        self.scroll_buffer.push(Line::from(vec![
                            Span::styled("  \u{2714} ", Style::default().fg(Color::Green)),
                            Span::raw("Resumed session "),
                            Span::styled(short, Style::default().fg(Color::Cyan)),
                        ]));
                    }
                }
            }
            MenuContent::ProviderModels(dd, ptype) => {
                if let Some(item) = dd.selected_item() {
                    let model_id = item.model_id.clone();
                    let ptype = *ptype;
                    self.apply_provider(ptype, ptype.default_base_url().to_string())
                        .await;
                    self.config.model = model_id.clone();
                    self.config.model_settings.model = model_id.clone();
                    self.config.recalculate_model_derived();
                    self.query_model_capabilities().await;
                    crate::tui_wizards::save_provider(&self.config, &self.session.db).await;
                    self.renderer.model = model_id.clone();
                    self.scroll_buffer.push(Line::styled(
                        format!("  \u{2714} Model set to: {model_id} ({ptype})"),
                        Style::default().fg(Color::Green),
                    ));
                }
            }
            MenuContent::File { dropdown, prefix } => {
                if let Some(item) = dropdown.selected_item() {
                    let replacement = format!("{}@{}", prefix, item.path);
                    self.textarea.set_text_clearing_elements("");
                    self.textarea.insert_str(&replacement);
                }
            }
            MenuContent::Approval { .. }
            | MenuContent::AskUser { .. }
            | MenuContent::LoopCap
            | MenuContent::PurgeConfirm { .. }
            | MenuContent::HistorySearch { .. }
            | MenuContent::WizardTrail(_)
            | MenuContent::ShortcutsOverlay
            | MenuContent::None => {}
        }
        self.menu = MenuContent::None;
    }

    // ── Wizard submit ──────────────────────────────────────────

    pub(crate) async fn handle_wizard_submit(&mut self) {
        let value = self.textarea.text().to_string();
        self.textarea.set_text_clearing_elements("");

        if let Some(wizard) = self.provider_wizard.take() {
            match wizard {
                ProviderWizard::ApiKey {
                    provider_type,
                    base_url,
                    env_name,
                } => {
                    if value.is_empty() && !koda_core::runtime_env::is_set(&env_name) {
                        self.scroll_buffer.push(Line::styled(
                            "  \u{2716} No API key provided.",
                            Style::default().fg(Color::Red),
                        ));
                        self.prompt_mode = PromptMode::Chat;
                        self.menu = MenuContent::None;
                        return;
                    }
                    if !value.is_empty() {
                        koda_core::runtime_env::set(&env_name, &value);
                        let _ =
                            koda_core::keystore::set_key(&self.session.db, &env_name, &value).await;
                        let masked = koda_core::keystore::mask_key(&value);
                        self.scroll_buffer.push(Line::styled(
                            format!("  \u{2714} {env_name} set to {masked}"),
                            Style::default().fg(Color::Green),
                        ));
                    }
                    self.apply_provider(provider_type, base_url).await;
                }
                ProviderWizard::ApiKeyOnly { env_name } => {
                    if value.is_empty() && !koda_core::runtime_env::is_set(&env_name) {
                        self.scroll_buffer.push(Line::styled(
                            "  \u{2716} No API key provided.",
                            Style::default().fg(Color::Red),
                        ));
                    } else if !value.is_empty() {
                        koda_core::runtime_env::set(&env_name, &value);
                        let _ =
                            koda_core::keystore::set_key(&self.session.db, &env_name, &value).await;
                        let masked = koda_core::keystore::mask_key(&value);
                        self.scroll_buffer.push(Line::styled(
                            format!("  \u{2714} {env_name} set to {masked}"),
                            Style::default().fg(Color::Green),
                        ));
                    }
                    // Key-only mode: just return to chat, no provider switch
                }
                ProviderWizard::Url { provider_type } => {
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

    async fn apply_provider(
        &mut self,
        provider_type: koda_core::config::ProviderType,
        base_url: String,
    ) {
        self.config.provider_type = provider_type;
        self.config.base_url = base_url;
        self.config.model = provider_type.default_model().to_string();
        self.config.model_settings.model = self.config.model.clone();
        self.config.recalculate_model_derived();
        *self.provider.write().await = koda_core::providers::create_provider(&self.config);
        crate::tui_wizards::save_provider(&self.config, &self.session.db).await;

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
        self.scroll_buffer.push(Line::styled(
            format!(
                "  \u{2714} Provider: {} ({})",
                self.config.provider_type, self.config.model
            ),
            Style::default().fg(Color::Green),
        ));
    }

    /// Query the provider API for context window and apply it.
    ///
    /// Called after `recalculate_model_derived()` on model switch so local
    /// providers (LM Studio, Ollama) report their actual configured context
    /// window instead of using the lookup table.
    async fn query_model_capabilities(&mut self) {
        let prov = self.provider.read().await;
        self.config
            .query_and_apply_capabilities(prov.as_ref())
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::history_search_matches;

    fn hist(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn empty_query_returns_nothing() {
        let h = hist(&["foo", "bar"]);
        assert!(history_search_matches(&h, "").is_empty());
    }

    #[test]
    fn newest_first_order() {
        let h = hist(&["first cmd", "second cmd", "third cmd"]);
        let m = history_search_matches(&h, "cmd");
        assert_eq!(m[0], "third cmd");
        assert_eq!(m[1], "second cmd");
        assert_eq!(m[2], "first cmd");
    }

    #[test]
    fn case_insensitive_match() {
        let h = hist(&["cargo Build", "cargo test"]);
        assert_eq!(history_search_matches(&h, "BUILD").len(), 1);
    }

    #[test]
    fn caps_at_six_results() {
        let h: Vec<String> = (0..10).map(|i| format!("match {i}")).collect();
        assert_eq!(history_search_matches(&h, "match").len(), 6);
    }

    #[test]
    fn no_match_returns_empty() {
        let h = hist(&["foo", "bar"]);
        assert!(history_search_matches(&h, "zzz").is_empty());
    }
}
