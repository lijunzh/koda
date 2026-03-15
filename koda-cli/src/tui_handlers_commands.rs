//! Command dispatch — slash commands, dropdown openers, and inference start.
//!
//! Extracted from `TuiContext::run_event_loop()` (Step 3a, #447).
//! Called when `tui_state == Idle` and there is a pending/queued input.

use crate::input;
use crate::tui_commands::{self, SlashAction};
use crate::tui_context::TuiContext;
use crate::tui_output;
use crate::tui_types::{MIN_VIEWPORT_HEIGHT, MenuContent, PromptMode, ProviderWizard};
use crate::tui_viewport::{emit_above, init_terminal};

use crossterm::event::EventStream;
use koda_core::approval::{self, ApprovalMode};
use koda_core::db::Role;
use koda_core::persistence::Persistence;
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

/// Outcome of dispatching a single command.
pub(crate) enum CommandOutcome {
    /// No input was available to dispatch.
    NoInput,
    /// Command was fully handled (slash command, dropdown opened, etc.).
    Handled,
    /// Non-slash input — ready to start an inference turn.
    StartInference {
        pending_images: Option<Vec<koda_core::providers::ImageData>>,
    },
    /// /quit was invoked.
    Quit,
}

impl TuiContext {
    /// Dequeue the next pending or queued input string, if any.
    ///
    /// Returns `None` when there is nothing to dispatch.
    pub(crate) fn dequeue_input(&mut self) -> Option<String> {
        if let Some(cmd) = self.pending_command.take() {
            return Some(cmd);
        }
        if let Some(queued) = self.input_queue.pop_front() {
            let mode = approval::read_mode(&self.shared_mode);
            let icon = match mode {
                ApprovalMode::Confirm => "🔒",
                ApprovalMode::Auto => "⚡",
            };
            emit_above(
                &mut self.terminal,
                Line::from(vec![
                    Span::styled(format!("{icon}> "), Style::default().fg(Color::Cyan)),
                    Span::raw(queued.clone()),
                ]),
            );
            return Some(queued);
        }
        None
    }

    /// Dispatch a raw input string.
    ///
    /// Handles slash commands, dropdown openers (`/model`, `/provider`,
    /// `/sessions`), and prepares non-slash input for inference.
    pub(crate) async fn dispatch_command(&mut self, raw: &str) -> CommandOutcome {
        let input = raw.trim().to_string();
        if input.is_empty() {
            return CommandOutcome::NoInput;
        }

        if input.starts_with('/') {
            return self.dispatch_slash(&input).await;
        }

        // Non-slash: prepare for inference
        self.prepare_inference_start(&input).await
    }

    // ── Slash command routing ────────────────────────────────────

    async fn dispatch_slash(&mut self, input: &str) -> CommandOutcome {
        // /model (no args) — open model picker dropdown
        if input.trim() == "/model" {
            self.open_model_picker().await;
            return CommandOutcome::Handled;
        }

        // /provider (no args) — open provider picker dropdown
        if input.trim() == "/provider" {
            self.open_provider_picker();
            return CommandOutcome::Handled;
        }

        // /provider <name> — skip dropdown, start wizard
        if input.trim().starts_with("/provider ") {
            let name = input.trim().strip_prefix("/provider ").unwrap().trim();
            self.start_provider_wizard(name);
            return CommandOutcome::Handled;
        }

        // /sessions — open session picker dropdown
        if input.trim() == "/sessions" {
            self.open_session_picker().await;
            return CommandOutcome::Handled;
        }

        // General slash commands (delegated to tui_commands)
        let action = tui_commands::handle_slash_command(
            &mut self.terminal,
            input,
            &mut self.config,
            &self.provider,
            &mut self.session,
            &self.shared_mode,
            &mut self.renderer,
            &self.project_root,
            &self.agent,
            &mut self.pending_command,
        )
        .await;

        match action {
            SlashAction::Continue => {
                self.reinit_after_slash().await;
                CommandOutcome::Handled
            }
            SlashAction::Quit => {
                tui_output::emit_line(
                    &mut self.terminal,
                    Line::styled(
                        "\u{1f43b} Goodbye!",
                        Style::default().fg(Color::Cyan),
                    ),
                );
                CommandOutcome::Quit
            }
        }
    }

    // ── Dropdown openers ─────────────────────────────────────────

    async fn open_model_picker(&mut self) {
        let prov = self.provider.read().await;
        match prov.list_models().await {
            Ok(models) if !models.is_empty() => {
                let items: Vec<crate::widgets::model_menu::ModelItem> = models
                    .iter()
                    .map(|m| crate::widgets::model_menu::ModelItem {
                        id: m.id.clone(),
                        is_current: m.id == self.config.model,
                    })
                    .collect();
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
            Ok(_) => {
                emit_above(
                    &mut self.terminal,
                    Line::styled(
                        "  \u{26a0} No models available",
                        Style::default().fg(Color::Yellow),
                    ),
                );
            }
            Err(e) => {
                emit_above(
                    &mut self.terminal,
                    Line::styled(
                        format!("  \u{2717} Failed to list models: {e}"),
                        Style::default().fg(Color::Red),
                    ),
                );
            }
        }
    }

    fn open_provider_picker(&mut self) {
        let providers = crate::repl::PROVIDERS;
        let items: Vec<crate::widgets::provider_menu::ProviderItem> = providers
            .iter()
            .map(|(key, name, desc)| crate::widgets::provider_menu::ProviderItem {
                key,
                name,
                description: desc,
                is_current: koda_core::config::ProviderType::from_url_or_name("", Some(key))
                    == self.config.provider_type,
            })
            .collect();
        let mut dd =
            crate::widgets::dropdown::DropdownState::new(items, "\u{1f43b} Select a provider");
        if let Some(idx) = dd.filtered.iter().position(|p| p.is_current) {
            dd.selected = idx;
            let max_vis = crate::widgets::dropdown::MAX_VISIBLE;
            if idx >= max_vis {
                dd.scroll_offset = idx + 1 - max_vis;
            }
        }
        self.menu = MenuContent::Provider(dd);
    }

    fn start_provider_wizard(&mut self, name: &str) {
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
            self.menu = MenuContent::WizardTrail(vec![("Provider".into(), provider_name)]);
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

    async fn open_session_picker(&mut self) {
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
                emit_above(
                    &mut self.terminal,
                    Line::styled(
                        "  No other sessions found.",
                        Style::default().fg(Color::DarkGray),
                    ),
                );
            }
            Err(e) => {
                emit_above(
                    &mut self.terminal,
                    Line::styled(
                        format!("  \u{2717} Error: {e}"),
                        Style::default().fg(Color::Red),
                    ),
                );
            }
        }
    }

    // ── Post-slash reinit ────────────────────────────────────────

    async fn reinit_after_slash(&mut self) {
        self.viewport_height = MIN_VIEWPORT_HEIGHT;
        self.crossterm_events = EventStream::new();
        if let Ok(term) = init_terminal(self.viewport_height) {
            self.terminal = term;
        }
        // Refresh model name cache (provider may have changed)
        {
            let prov = self.provider.read().await;
            if let Ok(models) = prov.list_models().await {
                self.completer
                    .set_model_names(models.iter().map(|m| m.id.clone()).collect());
            }
        }
        self.renderer.model = self.config.model.clone();
        // Force immediate redraw
        let _ = self.draw();
    }

    // ── Inference start preparation ─────────────────────────────

    async fn prepare_inference_start(&mut self, input: &str) -> CommandOutcome {
        let mut processed = input::process_input(input, &self.project_root);
        processed.paste_blocks = std::mem::take(&mut self.paste_blocks);

        // Emit image indicators
        for (i, _img) in processed.images.iter().enumerate() {
            emit_above(
                &mut self.terminal,
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        format!("\u{1f5bc} Image {}", i + 1),
                        Style::default().fg(Color::Magenta),
                    ),
                ]),
            );
        }

        // Build user message with context files
        let mut user_message =
            if let Some(context) = input::format_context_files(&processed.context_files) {
                for f in &processed.context_files {
                    emit_above(
                        &mut self.terminal,
                        Line::from(vec![
                            Span::raw("  "),
                            Span::styled(
                                format!("\u{1f4ce} {}", f.path),
                                Style::default().fg(Color::Cyan),
                            ),
                        ]),
                    );
                }
                format!("{}\n\n{context}", processed.prompt)
            } else {
                processed.prompt.clone()
            };

        // Append paste blocks
        if let Some(pasted) = input::format_paste_blocks(&processed.paste_blocks) {
            user_message = format!("{user_message}\n\n{pasted}");
        }

        // Persist user message
        if let Err(e) = self
            .session
            .db
            .insert_message(&self.session.id, &Role::User, Some(&user_message), None, None, None)
            .await
        {
            tracing::warn!("Failed to persist user message: {e}");
        }

        let pending_images = if processed.images.is_empty() {
            None
        } else {
            Some(processed.images)
        };

        self.session.mode = approval::read_mode(&self.shared_mode);
        self.session.update_provider(&self.config);

        CommandOutcome::StartInference { pending_images }
    }
}
