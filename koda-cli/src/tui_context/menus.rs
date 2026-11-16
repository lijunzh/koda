//! Menu navigation, dropdown pickers, and wizard state machines.
//!
//! All methods are `impl TuiContext` extensions for menu/wizard interaction.

use super::settings::ModelScope;
use super::*;
use koda_client::protocol::model::ModelProviderSummary;

/// Where `/feedback` sends a report about a problem, as Codex's TUI links
/// its CLI bug template.
const FEEDBACK_ISSUE_URL: &str =
    "https://github.com/lijunzh/koda/issues/new?template=bug_report.md";

/// The provider picker's items, from the host's providers: with
/// `keys_only`, the ones that take a key, each saying whether it has one
/// (`/key`).
pub(crate) fn provider_items(
    providers: &crate::providers::Providers,
    keys_only: bool,
) -> Vec<crate::widgets::provider_menu::ProviderItem> {
    providers
        .all()
        .iter()
        .filter(|p| !keys_only || p.requires_api_key)
        .map(|p| crate::widgets::provider_menu::ProviderItem {
            key: p.id.clone(),
            name: p.name.clone(),
            local: !p.requires_api_key,
            key_set: keys_only && p.has_api_key,
        })
        .collect()
}
/// Whether a `/provider` argument is an endpoint URL rather than a provider
/// name. A URL sets `base_url` and the provider type is re-detected from it.
pub(crate) fn is_provider_url(arg: &str) -> bool {
    let arg = arg.trim();
    arg.starts_with("http://") || arg.starts_with("https://")
}

// ── History search helpers ────────────────────────────────────────────────────

/// Return up to 6 history entries (newest first) containing `query`
/// (case-insensitive substring match). Empty query → no results.
fn history_search_matches(
    history: &[String],
    pastes: &[Vec<(String, String)>],
    query: &str,
) -> Vec<String> {
    if query.is_empty() {
        return Vec::new();
    }
    let q = query.to_lowercase();
    // A word from a paste finds its prompt, as in Claude Code.
    let pasted = |i: usize| {
        pastes.get(i).is_some_and(|list| {
            list.iter()
                .any(|(_, text)| text.to_lowercase().contains(&q))
        })
    };
    history
        .iter()
        .enumerate()
        .rev()
        .filter(|(i, s)| s.to_lowercase().contains(&q) || pasted(*i))
        .take(6)
        .map(|(_, s)| s.clone())
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
        // The match with the pastes behind its markers.
        match text.and_then(|t| self.history.iter().rposition(|h| *h == t)) {
            Some(idx) => self.recall(idx),
            None => self.pastes.restore(&mut self.textarea, "", Vec::new()),
        }
    }

    /// `/model`: the session's provider's models, from `model/list`, as
    /// Codex's `/model` lists its thread's provider's. Another provider's
    /// model is a new session (`/provider`). Enter makes the pick the
    /// default too; `s` changes only this session, as in Codex.
    pub(crate) async fn open_model_picker(&mut self) {
        let provider = self.settings.model_provider.clone();
        let models = self
            .list_models(&provider, &self.settings.base_url.clone())
            .await;
        let name = self.providers.name(&provider).to_string();
        if models.is_empty() {
            crate::tui_output::warn_msg(
                &mut self.scroll_buffer,
                format!("{name} lists no models. Is it running?"),
            );
            return;
        }
        let items = model_items(&models, &name, &self.settings.model);
        let mut dd = crate::widgets::dropdown::DropdownState::new(
            items,
            "\u{1f43b} Select a model (/provider for another provider)",
        )
        .with_select_hint("enter default \u{00b7} s session");
        if let Some(idx) = dd.filtered.iter().position(|m| m.is_current) {
            dd.selected = idx;
            let max_vis = crate::widgets::dropdown::MAX_VISIBLE;
            if idx >= max_vis {
                dd.scroll_offset = idx + 1 - max_vis;
            }
        }
        self.menu = MenuContent::Model(dd);
    }

    pub(crate) fn open_provider_picker(&mut self) {
        let items = provider_items(&self.providers, false);
        let dd = crate::widgets::dropdown::DropdownState::new(items, "\u{1f43b} Select a provider");
        self.menu = MenuContent::Provider(dd);
    }

    /// `/key`: the providers that take a key, whether the host has one for
    /// each, and a picker to set one.
    pub(crate) async fn open_key_picker(&mut self) {
        // Asked again: a key may have been set elsewhere meanwhile.
        self.providers = crate::providers::Providers::fetch(&self.client).await;
        crate::tui_wizards::handle_keys(&mut self.scroll_buffer, &self.providers);
        let items = provider_items(&self.providers, true);
        let dd = crate::widgets::dropdown::DropdownState::new(items, "\u{1f511} Set API key for");
        self.menu = MenuContent::Key(dd);
    }

    /// `/feedback`, step 1: what kind of report, in Codex's words.
    pub(crate) fn open_feedback_picker(&mut self) {
        let dd = crate::widgets::dropdown::DropdownState::new(
            crate::widgets::feedback_menu::categories(),
            "How was this?",
        );
        self.menu = MenuContent::Feedback {
            dropdown: dd,
            classification: None,
        };
    }

    /// `/feedback`, step 2: Codex's question about logs and history.
    fn open_feedback_consent(&mut self, classification: &'static str) {
        let dd = crate::widgets::dropdown::DropdownState::new(
            crate::widgets::feedback_menu::consent(),
            "Include logs and session history? They may hold prompts, commands and tool output",
        );
        self.menu = MenuContent::Feedback {
            dropdown: dd,
            classification: Some(classification),
        };
    }

    /// `/feedback`, step 3: an optional note, then the report is saved.
    fn start_feedback_note(&mut self, classification: &'static str, include_logs: bool) {
        self.menu = MenuContent::WizardTrail(vec![
            ("Feedback".into(), classification.replace('_', " ")),
            (
                "Logs and history".into(),
                if include_logs { "yes" } else { "no" }.into(),
            ),
        ]);
        self.prompt_mode = PromptMode::WizardInput {
            label: "Note (optional)".into(),
            mask: false,
        };
        self.provider_wizard = Some(ProviderWizard::FeedbackNote {
            classification,
            include_logs,
        });
        self.textarea.set_text_clearing_elements("");
    }

    /// Save the report through `feedback/save` to
    /// `<config>/feedback/koda-feedback-<time>-<id>.zip`, as Codex's TUI saves
    /// to `<codex home>/feedback/`, and say where. A report about a problem
    /// also gets the link to file it.
    async fn save_feedback(
        &mut self,
        classification: &'static str,
        include_logs: bool,
        note: String,
    ) {
        use koda_client::protocol::{feedback::FeedbackSaveParams, methods};
        let dir = match koda_config::config_dir() {
            Ok(dir) => dir.join("feedback"),
            Err(e) => {
                crate::tui_output::err_msg(&mut self.scroll_buffer, format!("{e:#}"));
                return;
            }
        };
        if let Err(e) = std::fs::create_dir_all(&dir) {
            crate::tui_output::err_msg(
                &mut self.scroll_buffer,
                format!("Couldn't create {}: {e}", dir.display()),
            );
            return;
        }
        let now = crate::util::utc_now();
        // Codex adds part of a fresh UUID; the clock's nanoseconds and the
        // pid tell two reports in the same second apart as well.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or_default();
        let suffix = format!("{:08x}", nanos ^ std::process::id());
        let path = dir.join(format!(
            "koda-feedback-{:04}-{:02}-{:02}T{:02}-{:02}-{:02}Z-{}.zip",
            now.year(),
            now.month() as u8,
            now.day(),
            now.hour(),
            now.minute(),
            now.second(),
            &suffix[..8]
        ));
        let note = note.trim();
        let saved = self
            .client
            .request::<methods::FeedbackSave>(FeedbackSaveParams {
                classification: classification.to_string(),
                reason: (!note.is_empty()).then(|| note.to_string()),
                session_id: Some(self.session.id.clone()),
                include_logs,
                save_path: path.display().to_string(),
            })
            .await;
        match saved {
            Ok(saved) => {
                crate::tui_output::ok_msg(
                    &mut self.scroll_buffer,
                    format!("Feedback saved \u{2192} {}", saved.saved_path),
                );
                if include_logs {
                    crate::tui_output::dim_msg(
                        &mut self.scroll_buffer,
                        "It holds your prompts, commands and tool output: check it before you share it."
                            .into(),
                    );
                }
                if classification != "good_result" {
                    crate::tui_output::dim_msg(
                        &mut self.scroll_buffer,
                        format!(
                            "To report it, open an issue and attach the ZIP: {FEEDBACK_ISSUE_URL}"
                        ),
                    );
                }
            }
            Err(e) => crate::tui_output::err_msg(
                &mut self.scroll_buffer,
                format!("Couldn't save the feedback: {e}"),
            ),
        }
    }

    /// `/provider <name>`: a provider that takes a key asks for it (Enter
    /// keeps one already set); a local one asks for its address.
    pub(crate) fn start_provider_wizard(&mut self, name: &str) {
        let Some(provider) = self.providers.find(name).cloned() else {
            let known: Vec<&str> = self.providers.all().iter().map(|p| p.id.as_str()).collect();
            crate::tui_output::err_msg(
                &mut self.scroll_buffer,
                format!("No provider '{name}'. Koda offers: {}.", known.join(", ")),
            );
            return;
        };
        self.menu = MenuContent::WizardTrail(vec![("Provider".into(), provider.id.clone())]);
        self.textarea.set_text_clearing_elements("");
        if provider.requires_api_key {
            let label = if provider.has_api_key {
                format!("API key for {} (Enter to keep current)", provider.id)
            } else {
                format!("API key for {}", provider.id)
            };
            // PR 6 of #1178: API key entry is the one masked wizard
            // path. The textarea renders `*` chars instead of the
            // actual keystrokes so over-the-shoulder onlookers and
            // screen recordings don't capture the secret.
            self.prompt_mode = PromptMode::WizardInput { label, mask: true };
            self.provider_wizard = Some(ProviderWizard::ApiKey { provider });
        } else {
            self.prompt_mode = PromptMode::WizardInput {
                label: format!("{} URL", provider.id),
                mask: false,
            };
            self.textarea.insert_str(&provider.base_url);
            self.provider_wizard = Some(ProviderWizard::Url { provider });
        }
    }

    /// Where to reach `provider`: the address the session runs with when
    /// it's the session's provider (`--base-url`, or one saved earlier),
    /// else the provider's default. Always taking the default listed and
    /// switched to the default port's server, not the one the user pointed
    /// Koda at.
    fn base_url_for(&self, provider: &ModelProviderSummary) -> String {
        if provider.id == self.settings.model_provider {
            self.settings.base_url.clone()
        } else {
            provider.base_url.clone()
        }
    }

    /// List a provider's models (`model/list`) to pick one from.
    async fn open_provider_model_list(&mut self, provider: ModelProviderSummary) {
        let base_url = self.base_url_for(&provider);
        let models = self.list_models(&provider.id, &base_url).await;
        if models.is_empty() {
            self.scroll_buffer.push(Line::styled(
                format!("  \u{26a0} No models available from {}", provider.id),
                Style::default().fg(Color::Yellow),
            ));
            return;
        }
        let items = model_items(&models, &provider.name, &self.settings.model);
        let title = format!("\u{1f43b} {} models", provider.name);
        let mut dd = crate::widgets::dropdown::DropdownState::new(items, &title);
        if let Some(idx) = dd.filtered.iter().position(|m| m.is_current) {
            dd.selected = idx;
            let max_vis = crate::widgets::dropdown::MAX_VISIBLE;
            if idx >= max_vis {
                dd.scroll_offset = idx + 1 - max_vis;
            }
        }
        self.menu = MenuContent::ProviderModels(dd, provider);
    }

    /// The `/resume` picker.
    pub(crate) async fn open_session_picker(&mut self) {
        match crate::tui_commands::list_sessions(&self.client, 10).await {
            Ok(sessions) if !sessions.is_empty() => {
                let items: Vec<crate::widgets::session_menu::SessionItem> = sessions
                    .iter()
                    .map(|s| crate::widgets::session_menu::SessionItem {
                        id: s.id.clone(),
                        short_id: s.id[..8.min(s.id.len())].to_string(),
                        updated: crate::util::unix_minutes(s.updated_at),
                        is_current: s.id == self.session.id,
                        title: s.name.clone(),
                        preview: s.preview.clone(),
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
                    let new_matches =
                        history_search_matches(&self.history, &self.history_pastes, query);
                    *selected = 0;
                    *matches = new_matches;
                }
                // Backspace → shrink query
                (KeyCode::Backspace, _) => {
                    query.pop();
                    let new_matches =
                        history_search_matches(&self.history, &self.history_pastes, query);
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
        // overlay above the status bar; Esc and Ctrl+C cancel all of it.)

        // Typing while "Implement this plan?" is up is a reply to the
        // model: the menu goes and the key reaches the input box.
        if matches!(self.menu, MenuContent::PlanImplement(_))
            && matches!(key.code, KeyCode::Char(_))
            && !key.modifiers.contains(KeyModifiers::CONTROL)
        {
            self.menu = MenuContent::None;
            return None;
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

        // `s` in `/model`: this session only, as Codex's `s`. The picker
        // doesn't filter, so a typed `s` has nothing else to do there.
        if matches!(self.menu, MenuContent::Model(_))
            && key.code == KeyCode::Char('s')
            && key.modifiers.difference(KeyModifiers::SHIFT).is_empty()
        {
            if let MenuContent::Model(dd) = &self.menu
                && let Some(item) = dd.selected_item()
            {
                let choice = item.label.clone();
                self.menu = MenuContent::None;
                self.change_model(&choice, ModelScope::Session).await;
            }
            return Some(true);
        }

        match key.code {
            // The wizard trail only shows which wizard is asking: Enter
            // answers the input box under it. Taking Enter here closed the
            // trail and dropped the answer, so a pasted API key needed a
            // second Enter.
            KeyCode::Enter if matches!(self.menu, MenuContent::WizardTrail(_)) => {
                return None;
            }
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
            MenuContent::Feedback { dropdown, .. } => nav!(dropdown),
            MenuContent::PlanImplement(dd) => nav!(dd),
            MenuContent::Session(dd) => nav!(dd),
            MenuContent::File { dropdown: dd, .. } => nav!(dd),
            MenuContent::Approval { .. }
            | MenuContent::AskUser { .. }
            | MenuContent::LoopCap { .. }
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
                        // Self-contained / picker-opener: the event loop runs
                        // it next, as if typed, and acts on its outcome.
                        self.pending_command = Some(cmd);
                    }
                }
                return; // skip the trailing self.menu = None
            }
            MenuContent::Model(dd) => {
                if let Some(item) = dd.selected_item() {
                    // An alias is picked by name, so it follows its line.
                    let choice = item.label.clone();
                    self.change_model(&choice, ModelScope::Default).await;
                }
            }
            MenuContent::Provider(dd) => {
                let picked = dd
                    .selected_item()
                    .and_then(|item| self.providers.find(&item.key))
                    .cloned();
                if let Some(provider) = picked {
                    if provider.requires_api_key && !provider.has_api_key {
                        // Need API key first — enter wizard
                        self.start_provider_wizard(&provider.id);
                    } else {
                        // Key is set (or not needed) — list models from this provider
                        self.open_provider_model_list(provider).await;
                    }
                }
                return;
            }
            MenuContent::Key(dd) => {
                let picked = dd
                    .selected_item()
                    .and_then(|item| self.providers.find(&item.key))
                    .cloned();
                if let Some(provider) = picked {
                    let label = if provider.has_api_key {
                        format!("API key for {} (Enter to keep current)", provider.id)
                    } else {
                        format!("API key for {}", provider.id)
                    };
                    self.menu = MenuContent::WizardTrail(vec![("Key".into(), provider.id.clone())]);
                    // PR 6 of #1178: second API-key entry path (re-enter
                    // an existing key from the /key picker). Also masked.
                    self.prompt_mode = PromptMode::WizardInput { label, mask: true };
                    self.provider_wizard = Some(ProviderWizard::ApiKeyOnly { provider });
                    self.textarea.set_text_clearing_elements("");
                }
                return;
            }
            MenuContent::Feedback {
                dropdown,
                classification,
            } => {
                let chosen = dropdown.selected_item().map(|item| item.value);
                let classification = *classification;
                if let Some(value) = chosen {
                    match classification {
                        None => self.open_feedback_consent(value),
                        Some(classification) => {
                            self.start_feedback_note(classification, value == "yes")
                        }
                    }
                }
                return;
            }
            MenuContent::PlanImplement(dd) => {
                let mode = dd.selected_item().and_then(|choice| choice.mode);
                self.menu = MenuContent::None;
                if let Some(mode) = mode {
                    self.implement_plan(mode).await;
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
                        self.session.id = target_id.clone();
                        self.scroll_buffer.push(Line::from(vec![
                            Span::styled("  \u{2714} ", Style::default().fg(Color::Green)),
                            Span::raw("Resumed session "),
                            Span::styled(short, Style::default().fg(Color::Cyan)),
                        ]));
                        super::show_earlier_turns(
                            &self.client,
                            &target_id,
                            &mut self.renderer,
                            &mut self.scroll_buffer,
                        )
                        .await;
                    }
                }
            }
            MenuContent::ProviderModels(dd, provider) => {
                if let Some(item) = dd.selected_item() {
                    let choice = item.label.clone();
                    let provider = provider.clone();
                    let base_url = self.base_url_for(&provider);
                    self.switch_provider(Some(&provider), Some(base_url), Some(choice))
                        .await;
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
            | MenuContent::LoopCap { .. }
            | MenuContent::HistorySearch { .. }
            | MenuContent::WizardTrail(_)
            | MenuContent::ShortcutsOverlay
            | MenuContent::None => {}
        }
        self.menu = MenuContent::None;
    }

    // ── Wizard submit ──────────────────────────────────────────

    /// Store `provider`'s key through the host, which also makes it this
    /// run's, and say so with the key masked. Returns whether it was stored.
    async fn store_key(&mut self, provider: &ModelProviderSummary, value: &str) -> bool {
        match super::settings::set_api_key(&self.client, &provider.id, value).await {
            Ok(()) => {
                self.scroll_buffer.push(Line::styled(
                    format!("  \u{2714} {} set to {}", provider.env_key, mask_key(value)),
                    Style::default().fg(Color::Green),
                ));
                // Whether a provider has a key is the host's to say.
                self.providers = crate::providers::Providers::fetch(&self.client).await;
                true
            }
            Err(e) => {
                crate::tui_output::err_msg(&mut self.scroll_buffer, format!("{e:#}"));
                false
            }
        }
    }

    pub(crate) async fn handle_wizard_submit(&mut self) {
        let value = self.textarea.text().to_string();
        self.textarea.set_text_clearing_elements("");

        if let Some(wizard) = self.provider_wizard.take() {
            match wizard {
                ProviderWizard::FeedbackNote {
                    classification,
                    include_logs,
                } => {
                    self.prompt_mode = PromptMode::Chat;
                    self.menu = MenuContent::None;
                    self.save_feedback(classification, include_logs, value)
                        .await;
                    return;
                }
                ProviderWizard::ApiKey { provider } => {
                    if value.is_empty() && !provider.has_api_key {
                        self.scroll_buffer.push(Line::styled(
                            "  \u{2716} No API key provided.",
                            Style::default().fg(Color::Red),
                        ));
                        self.prompt_mode = PromptMode::Chat;
                        self.menu = MenuContent::None;
                        return;
                    }
                    if !value.is_empty() && !self.store_key(&provider, &value).await {
                        self.prompt_mode = PromptMode::Chat;
                        self.menu = MenuContent::None;
                        return;
                    }
                    let base_url = provider.base_url.clone();
                    self.switch_provider(Some(&provider), Some(base_url), None)
                        .await;
                }
                ProviderWizard::ApiKeyOnly { provider } => {
                    if value.is_empty() && !provider.has_api_key {
                        self.scroll_buffer.push(Line::styled(
                            "  \u{2716} No API key provided.",
                            Style::default().fg(Color::Red),
                        ));
                    } else if !value.is_empty() {
                        self.store_key(&provider, &value).await;
                    }
                    // Key-only mode: just return to chat, no provider switch
                }
                ProviderWizard::Url { provider } => {
                    let url = if value.is_empty() {
                        provider.base_url.clone()
                    } else {
                        value
                    };
                    self.switch_provider(Some(&provider), Some(url), None).await;
                }
            }
        }
        self.prompt_mode = PromptMode::Chat;
        self.menu = MenuContent::None;
    }
}

/// Picker items for `models`, of the provider named `provider`: an alias
/// by its name, a model by its id.
fn model_items(
    models: &[koda_client::protocol::model::Model],
    provider: &str,
    current: &str,
) -> Vec<crate::widgets::model_menu::ModelItem> {
    models
        .iter()
        .map(|m| crate::widgets::model_menu::ModelItem {
            label: m.alias.clone().unwrap_or_else(|| m.id.clone()),
            model_id: m.id.clone(),
            provider: provider.to_string(),
            is_current: m.id == current,
        })
        .collect()
}

/// A key shown as it was stored: its first and last four characters, or
/// `****` for a short one. Only what was typed is shown, so the TUI does
/// this itself.
fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() > 8 {
        let head: String = chars[..4].iter().collect();
        let tail: String = chars[chars.len() - 4..].iter().collect();
        format!("{head}...{tail}")
    } else {
        "****".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{history_search_matches, is_provider_url, mask_key};

    #[test]
    fn a_stored_key_is_shown_masked() {
        assert_eq!(mask_key("sk-ant-api03-longkey1234"), "sk-a...1234");
        assert_eq!(mask_key("short"), "****");
    }

    fn hist(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn provider_url_is_told_from_a_name() {
        assert!(is_provider_url("http://127.0.0.1:8080/v1"));
        assert!(is_provider_url("  https://api.example.com/v1 "));
        for name in ["llama-cpp", "ollama", "localhost:8080", ""] {
            assert!(!is_provider_url(name), "{name:?}");
        }
    }

    #[test]
    fn empty_query_returns_nothing() {
        let h = hist(&["foo", "bar"]);
        assert!(history_search_matches(&h, &[], "").is_empty());
    }

    #[test]
    fn newest_first_order() {
        let h = hist(&["first cmd", "second cmd", "third cmd"]);
        let m = history_search_matches(&h, &[], "cmd");
        assert_eq!(m[0], "third cmd");
        assert_eq!(m[1], "second cmd");
        assert_eq!(m[2], "first cmd");
    }

    #[test]
    fn case_insensitive_match() {
        let h = hist(&["cargo Build", "cargo test"]);
        assert_eq!(history_search_matches(&h, &[], "BUILD").len(), 1);
    }

    #[test]
    fn caps_at_six_results() {
        let h: Vec<String> = (0..10).map(|i| format!("match {i}")).collect();
        assert_eq!(history_search_matches(&h, &[], "match").len(), 6);
    }

    #[test]
    fn no_match_returns_empty() {
        let h = hist(&["foo", "bar"]);
        assert!(history_search_matches(&h, &[], "zzz").is_empty());
    }

    /// A word from a paste finds the prompt it was pasted into.
    #[test]
    fn history_search_looks_inside_pastes() {
        let h = vec![
            "fix [Pasted text #1 +4 lines]".to_string(),
            "other".to_string(),
        ];
        let pastes = vec![
            vec![(
                "[Pasted text #1 +4 lines]".to_string(),
                "thread 'main' panicked\nat src/lib.rs\n\n".to_string(),
            )],
            vec![],
        ];
        assert_eq!(
            history_search_matches(&h, &pastes, "panicked"),
            [h[0].clone()]
        );
    }
}
