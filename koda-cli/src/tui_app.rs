//! TUI main event loop.
//!
//! The event loop that ties everything together: dispatches keyboard
//! events, manages inference turns, handles slash commands, and
//! coordinates the persistent inline viewport.
//!
//! Supporting modules:
//! - `tui_types` — enums, type aliases, constants
//! - `tui_viewport` — viewport drawing and terminal lifecycle
//! - `tui_history` — command history persistence
//! - `tui_commands` — slash command dispatch
//! - `tui_render` — streaming inference output rendering
//! - `tui_output` — low-level terminal output helpers
//!
//! See #209 for the refactoring plan.

use crate::input;
use crate::sink::UiEvent;
use crate::tui_commands::{self, SlashAction};
use crate::tui_history;
use crate::tui_output;
use crate::tui_render::TuiRenderer;
use crate::tui_types::{MIN_VIEWPORT_HEIGHT, MenuContent, PromptMode, ProviderWizard, TuiState};
use crate::tui_viewport::{
    draw_viewport, emit_above, init_terminal, maybe_resize_viewport, reinit_viewport_in_place,
    restore_terminal,
};

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyModifiers};
use futures_util::StreamExt;
use koda_core::agent::KodaAgent;
use koda_core::approval::{self, ApprovalMode};
use koda_core::config::KodaConfig;
use koda_core::db::{Database, Role};
use koda_core::engine::{ApprovalDecision, EngineCommand, EngineEvent};
use koda_core::providers::LlmProvider;
use koda_core::session::KodaSession;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::sync::mpsc;
use tui_textarea::TextArea;

// ── Main event loop ──────────────────────────────────────────

/// Run the main interactive event loop with persistent TUI.
pub async fn run(
    project_root: PathBuf,
    mut config: KodaConfig,
    db: Database,
    session_id: String,
    version_check: tokio::task::JoinHandle<Option<String>>,
    first_run: bool,
) -> Result<()> {
    // ── Setup (same as before) ───────────────────────────────

    let settings = koda_core::approval::Settings::load();
    if let Some(ref last) = settings.last_provider {
        let ptype =
            koda_core::config::ProviderType::from_url_or_name("", Some(&last.provider_type));
        config.provider_type = ptype;
        config.base_url = last.base_url.clone();
        config.model = last.model.clone();
        config.model_settings.model = last.model.clone();
        // Recalculate context window and tier for the restored model
        config.recalculate_model_derived();
    }

    let provider: Arc<RwLock<Box<dyn LlmProvider>>> =
        Arc::new(RwLock::new(koda_core::providers::create_provider(&config)));

    if config.model == "auto-detect" {
        let prov = provider.read().await;
        match prov.list_models().await {
            Ok(models) if !models.is_empty() => {
                config.model = models[0].id.clone();
                config.model_settings.model = config.model.clone();
                config.recalculate_model_derived();
                tracing::info!("Auto-detected model: {}", config.model);
            }
            Ok(_) => {
                config.model = "(no model loaded)".to_string();
                config.model_settings.model = config.model.clone();
            }
            Err(e) => {
                config.model = "(connection failed)".to_string();
                config.model_settings.model = config.model.clone();
                tracing::warn!("Auto-detect failed: {e}");
            }
        }
    }

    // Query actual model capabilities from the provider API.
    // This overrides the hardcoded context window with the real value.
    if config.model != "(no model loaded)" && config.model != "(connection failed)" {
        let prov = provider.read().await;
        config.query_and_apply_capabilities(prov.as_ref()).await;
    }

    // Print startup UI BEFORE entering raw mode
    let recent = db.recent_user_messages(3).await.unwrap_or_default();
    crate::startup::print_banner(&config, &recent);
    crate::startup::print_model_warning(&config);

    if let Ok(Some(latest)) = version_check.await
        && let Some((current, latest)) = koda_core::version::update_available(&latest)
    {
        crate::startup::print_update_notice(current, &latest);
    }

    let agent = Arc::new(KodaAgent::new(&config, project_root.clone()).await?);
    crate::startup::print_mcp_status(&agent.mcp_statuses);

    let mut session = KodaSession::new(
        session_id.clone(),
        agent.clone(),
        db,
        &config,
        ApprovalMode::Auto,
    );

    let shared_mode = approval::new_shared_mode(ApprovalMode::Auto);

    // ── Initialize persistent terminal ───────────────────────

    let mut viewport_height = MIN_VIEWPORT_HEIGHT;
    let mut terminal = init_terminal(viewport_height)?;

    let mut textarea = TextArea::default();
    textarea.set_cursor_line_style(Style::default());
    textarea.set_cursor_style(
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::REVERSED),
    );
    textarea.set_placeholder_text("Type a message...");
    textarea.set_placeholder_style(Style::default().fg(Color::DarkGray));

    // ── Channels ─────────────────────────────────────────────

    let (ui_tx, mut ui_rx) = mpsc::channel::<UiEvent>(256);
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<EngineCommand>(32);

    // ── State ────────────────────────────────────────────────

    let mut renderer = TuiRenderer::new();
    renderer.model = config.model.clone();
    let mut tui_state = TuiState::Idle;
    let mut input_queue: VecDeque<String> = VecDeque::new();
    let mut pending_command: Option<String> = None;
    let mut silent_compact_deferred = false;
    let mut should_quit = false;
    let mut menu = MenuContent::None;
    let mut prompt_mode = PromptMode::Chat;
    let mut provider_wizard: Option<ProviderWizard> = None;
    let mut pending_approval_id: Option<String> = None;

    // First-run onboarding: auto-open provider dropdown
    if first_run {
        emit_above(
            &mut terminal,
            Line::styled(
                "  \u{1f43b} Welcome to Koda! Let's pick your LLM provider.",
                Style::default().fg(Color::Cyan),
            ),
        );
        let providers = crate::repl::PROVIDERS;
        let items: Vec<crate::widgets::provider_menu::ProviderItem> = providers
            .iter()
            .map(
                |(key, name, desc)| crate::widgets::provider_menu::ProviderItem {
                    key,
                    name,
                    description: desc,
                    is_current: false,
                },
            )
            .collect();
        menu = MenuContent::Provider(crate::widgets::dropdown::DropdownState::new(
            items,
            "\u{1f43b} Choose your LLM provider",
        ));
    }
    let mut inference_start: Option<std::time::Instant> = None;
    let mut history: Vec<String> = tui_history::load_history();
    let mut history_idx: Option<usize> = None; // None = not browsing history
    let mut completer = crate::completer::InputCompleter::new(project_root.clone());

    // Cache model names for /model Tab completion
    {
        let prov = provider.read().await;
        if let Ok(models) = prov.list_models().await {
            completer.set_model_names(models.iter().map(|m| m.id.clone()).collect());
        }
    }

    // Crossterm event stream for async key capture
    let mut crossterm_events = EventStream::new();

    // ── Initial viewport draw ────────────────────────────────

    let mode = approval::read_mode(&shared_mode);
    let ctx = koda_core::context::percentage() as u32;
    maybe_resize_viewport(&mut terminal, &textarea, &mut viewport_height)?;
    terminal.draw(|f| {
        draw_viewport(
            f,
            &textarea,
            &config.model,
            config.model_tier.label(),
            mode,
            ctx,
            tui_state,
            &prompt_mode,
            input_queue.len(),
            inference_start.map(|s| s.elapsed().as_secs()).unwrap_or(0),
            renderer.last_turn_stats.as_ref(),
            &menu,
        );
    })?;

    // ── Main event loop ──────────────────────────────────────

    loop {
        if should_quit {
            break;
        }

        // Check if we have a queued or pending command to process
        if tui_state == TuiState::Idle {
            let input = if let Some(cmd) = pending_command.take() {
                Some(cmd)
            } else if let Some(queued) = input_queue.pop_front() {
                // Echo queued input above viewport
                let mode = approval::read_mode(&shared_mode);
                let icon = match mode {
                    ApprovalMode::Safe => "🔍",
                    ApprovalMode::Strict => "🔒",
                    ApprovalMode::Auto => "⚡",
                };
                emit_above(
                    &mut terminal,
                    Line::from(vec![
                        Span::styled(format!("{icon}> "), Style::default().fg(Color::Cyan)),
                        Span::raw(queued.clone()),
                    ]),
                );
                Some(queued)
            } else {
                None
            };

            if let Some(input) = input {
                let input = input.trim().to_string();
                if !input.is_empty() {
                    // Try slash commands first
                    if input.starts_with('/') {
                        // Intercept /model (no args) — open inline dropdown.
                        if input.trim() == "/model" {
                            let prov = provider.read().await;
                            match prov.list_models().await {
                                Ok(models) if !models.is_empty() => {
                                    let items: Vec<crate::widgets::model_menu::ModelItem> = models
                                        .iter()
                                        .map(|m| crate::widgets::model_menu::ModelItem {
                                            id: m.id.clone(),
                                            is_current: m.id == config.model,
                                        })
                                        .collect();
                                    let mut dd = crate::widgets::dropdown::DropdownState::new(
                                        items,
                                        "\u{1f43b} Select a model",
                                    );
                                    // Pre-select current model
                                    if let Some(idx) = dd.filtered.iter().position(|m| m.is_current)
                                    {
                                        dd.selected = idx;
                                        // Adjust scroll so current model is visible
                                        let max_vis = crate::widgets::dropdown::MAX_VISIBLE;
                                        if idx >= max_vis {
                                            dd.scroll_offset = idx + 1 - max_vis;
                                        }
                                    }
                                    menu = MenuContent::Model(dd);
                                }
                                Ok(_) => {
                                    emit_above(
                                        &mut terminal,
                                        Line::styled(
                                            "  \u{26a0} No models available",
                                            Style::default().fg(Color::Yellow),
                                        ),
                                    );
                                }
                                Err(e) => {
                                    emit_above(
                                        &mut terminal,
                                        Line::styled(
                                            format!("  \u{2717} Failed to list models: {e}"),
                                            Style::default().fg(Color::Red),
                                        ),
                                    );
                                }
                            }
                            continue;
                        }

                        // Intercept /provider (no args) — open inline dropdown
                        if input.trim() == "/provider" {
                            let providers = crate::repl::PROVIDERS;
                            let items: Vec<crate::widgets::provider_menu::ProviderItem> = providers
                                .iter()
                                .map(|(key, name, desc)| {
                                    crate::widgets::provider_menu::ProviderItem {
                                        key,
                                        name,
                                        description: desc,
                                        is_current:
                                            koda_core::config::ProviderType::from_url_or_name(
                                                "",
                                                Some(key),
                                            ) == config.provider_type,
                                    }
                                })
                                .collect();
                            let mut dd = crate::widgets::dropdown::DropdownState::new(
                                items,
                                "\u{1f43b} Select a provider",
                            );
                            // Pre-select current provider
                            if let Some(idx) = dd.filtered.iter().position(|p| p.is_current) {
                                dd.selected = idx;
                                let max_vis = crate::widgets::dropdown::MAX_VISIBLE;
                                if idx >= max_vis {
                                    dd.scroll_offset = idx + 1 - max_vis;
                                }
                            }
                            menu = MenuContent::Provider(dd);
                            continue;
                        }

                        // Intercept /provider <name> — skip dropdown, start wizard at API key step
                        if input.trim().starts_with("/provider ") {
                            let name = input.trim().strip_prefix("/provider ").unwrap().trim();
                            let ptype =
                                koda_core::config::ProviderType::from_url_or_name("", Some(name));
                            let base_url = ptype.default_base_url().to_string();
                            let provider_name = ptype.to_string();

                            if ptype.requires_api_key() {
                                let env_name = ptype.env_key_name().to_string();
                                // Check if key already exists in keystore
                                if koda_core::runtime_env::is_set(&env_name) {
                                    // Key exists — just switch provider, no wizard
                                    config.provider_type = ptype.clone();
                                    config.base_url = base_url;
                                    config.model = ptype.default_model().to_string();
                                    config.model_settings.model = config.model.clone();
                                    config.recalculate_model_derived();
                                    *provider.write().await =
                                        koda_core::providers::create_provider(&config);
                                    crate::tui_wizards::save_provider(&config);
                                    let prov = provider.read().await;
                                    if let Ok(models) = prov.list_models().await {
                                        if let Some(first) = models.first() {
                                            config.model = first.id.clone();
                                            config.model_settings.model = config.model.clone();
                                            config.recalculate_model_derived();
                                        }
                                        config.query_and_apply_capabilities(prov.as_ref()).await;
                                        completer.set_model_names(
                                            models.iter().map(|m| m.id.clone()).collect(),
                                        );
                                    }
                                    renderer.model = config.model.clone();
                                    emit_above(
                                        &mut terminal,
                                        Line::styled(
                                            format!(
                                                "  \u{2714} Provider: {} ({})",
                                                config.provider_type, config.model
                                            ),
                                            Style::default().fg(Color::Green),
                                        ),
                                    );
                                } else {
                                    // Need API key — start wizard at step 2
                                    menu = MenuContent::WizardTrail(vec![(
                                        "Provider".into(),
                                        provider_name,
                                    )]);
                                    prompt_mode = PromptMode::WizardInput {
                                        label: format!("API key for {}", ptype),
                                        masked: true,
                                    };
                                    provider_wizard = Some(ProviderWizard::NeedApiKey {
                                        provider_type: ptype,
                                        base_url,
                                        env_name,
                                    });
                                    textarea.select_all();
                                    textarea.cut();
                                }
                            } else {
                                // Local provider — start wizard at URL step
                                menu = MenuContent::WizardTrail(vec![(
                                    "Provider".into(),
                                    provider_name,
                                )]);
                                prompt_mode = PromptMode::WizardInput {
                                    label: format!("{} URL", ptype),
                                    masked: false,
                                };
                                provider_wizard = Some(ProviderWizard::NeedUrl {
                                    provider_type: ptype,
                                });
                                textarea.select_all();
                                textarea.cut();
                                textarea.insert_str(&base_url);
                            }
                            continue;
                        }

                        // Intercept /sessions (no args) — open inline dropdown
                        if input.trim() == "/sessions" {
                            match session.db.list_sessions(10, &project_root).await {
                                Ok(sessions) if !sessions.is_empty() => {
                                    let items: Vec<crate::widgets::session_menu::SessionItem> =
                                        sessions
                                            .iter()
                                            .map(|s| crate::widgets::session_menu::SessionItem {
                                                id: s.id.clone(),
                                                short_id: s.id[..8.min(s.id.len())].to_string(),
                                                created_at: s.created_at.clone(),
                                                message_count: s.message_count,
                                                total_tokens: s.total_tokens,
                                                is_current: s.id == session.id,
                                            })
                                            .collect();
                                    let mut dd = crate::widgets::dropdown::DropdownState::new(
                                        items,
                                        "\u{1f43b} Sessions",
                                    );
                                    // Pre-select current session
                                    if let Some(idx) = dd.filtered.iter().position(|s| s.is_current)
                                    {
                                        dd.selected = idx;
                                        let max_vis = crate::widgets::dropdown::MAX_VISIBLE;
                                        if idx >= max_vis {
                                            dd.scroll_offset = idx + 1 - max_vis;
                                        }
                                    }
                                    menu = MenuContent::Session(dd);
                                }
                                Ok(_) => {
                                    emit_above(
                                        &mut terminal,
                                        Line::styled(
                                            "  No other sessions found.",
                                            Style::default().fg(Color::DarkGray),
                                        ),
                                    );
                                }
                                Err(e) => {
                                    emit_above(
                                        &mut terminal,
                                        Line::styled(
                                            format!("  \u{2717} Error: {e}"),
                                            Style::default().fg(Color::Red),
                                        ),
                                    );
                                }
                            }
                            continue;
                        }

                        let action = tui_commands::handle_slash_command(
                            &mut terminal,
                            &input,
                            &mut config,
                            &provider,
                            &mut session,
                            &shared_mode,
                            &mut renderer,
                            &project_root,
                            &agent,
                            &mut pending_command,
                        )
                        .await;

                        match action {
                            SlashAction::Continue => {
                                // Re-init terminal to resync viewport with cursor
                                // position after crossterm direct writes.
                                viewport_height = MIN_VIEWPORT_HEIGHT;
                                // Drop the old EventStream BEFORE init_terminal.
                                // EventStream spawns a background wake thread that
                                // reads from stdin; if it's still active it can
                                // consume the DSR response that Viewport::Inline's
                                // cursor-position query needs, causing a timeout.
                                crossterm_events = EventStream::new();
                                terminal = init_terminal(viewport_height)?;
                                // Refresh model name cache (provider may have changed)
                                let prov = provider.read().await;
                                if let Ok(models) = prov.list_models().await {
                                    completer.set_model_names(
                                        models.iter().map(|m| m.id.clone()).collect(),
                                    );
                                }
                                // Sync model name for cost estimation
                                renderer.model = config.model.clone();
                                // Force immediate redraw so the prompt is visible
                                // after slash command output (don't wait for next event).
                                let mode = approval::read_mode(&shared_mode);
                                let ctx = koda_core::context::percentage() as u32;
                                terminal.draw(|f| {
                                    draw_viewport(
                                        f,
                                        &textarea,
                                        &config.model,
                                        config.model_tier.label(),
                                        mode,
                                        ctx,
                                        tui_state,
                                        &prompt_mode,
                                        input_queue.len(),
                                        0,
                                        renderer.last_turn_stats.as_ref(),
                                        &menu,
                                    );
                                })?;
                            }
                            SlashAction::Quit => {
                                tui_output::emit_line(
                                    &mut terminal,
                                    Line::styled(
                                        "\u{1f43b} Goodbye!",
                                        Style::default().fg(Color::Cyan),
                                    ),
                                );
                                should_quit = true;
                                continue;
                            }
                        }
                    } else {
                        // ── Start inference turn inline ──────────
                        let user_input = input.clone();
                        let processed = input::process_input(&user_input, &project_root);
                        if !processed.images.is_empty() {
                            for (i, _img) in processed.images.iter().enumerate() {
                                emit_above(
                                    &mut terminal,
                                    Line::from(vec![
                                        ratatui::text::Span::raw("  "),
                                        ratatui::text::Span::styled(
                                            format!("\u{1f5bc} Image {}", i + 1),
                                            Style::default().fg(Color::Magenta),
                                        ),
                                    ]),
                                );
                            }
                        }

                        let user_message = if let Some(context) =
                            input::format_context_files(&processed.context_files)
                        {
                            for f in &processed.context_files {
                                emit_above(
                                    &mut terminal,
                                    Line::from(vec![
                                        ratatui::text::Span::raw("  "),
                                        ratatui::text::Span::styled(
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

                        if let Err(e) = session
                            .db
                            .insert_message(
                                &session.id,
                                &Role::User,
                                Some(&user_message),
                                None,
                                None,
                                None,
                            )
                            .await
                        {
                            tracing::warn!("Failed to persist user message: {e}");
                        }

                        let pending_images = if processed.images.is_empty() {
                            None
                        } else {
                            Some(processed.images)
                        };

                        session.mode = approval::read_mode(&shared_mode);
                        session.update_provider(&config);

                        let cli_sink = crate::sink::CliSink::channel(ui_tx.clone());
                        let cancel_token = session.cancel.clone();

                        // Run the inference turn as a pinned future
                        tui_state = TuiState::Inferring;
                        inference_start = Some(std::time::Instant::now());
                        renderer.last_turn_stats = None;

                        {
                            let turn =
                                session.run_turn(&config, pending_images, &cli_sink, &mut cmd_rx);
                            tokio::pin!(turn);

                            loop {
                                // Redraw viewport inside inference loop
                                let mode = approval::read_mode(&shared_mode);
                                let ctx = koda_core::context::percentage() as u32;
                                terminal.draw(|f| {
                                    draw_viewport(
                                        f,
                                        &textarea,
                                        &config.model,
                                        config.model_tier.label(),
                                        mode,
                                        ctx,
                                        tui_state,
                                        &prompt_mode,
                                        input_queue.len(),
                                        inference_start.map(|s| s.elapsed().as_secs()).unwrap_or(0),
                                        renderer.last_turn_stats.as_ref(),
                                        &menu,
                                    );
                                })?;

                                tokio::select! {
                                    result = &mut turn => {
                                        if let Err(e) = result {
                                            emit_above(
                                                &mut terminal,
                                                Line::from(vec![
                                                    ratatui::text::Span::raw("  "),
                                                    ratatui::text::Span::styled(
                                                        format!("\u{2717} Turn failed: {e:#}"),
                                                        Style::default().fg(Color::Red),
                                                    ),
                                                ]),
                                            );
                                        }
                                        break;
                                    }
                                    Some(Ok(ev)) = crossterm_events.next() => {
                                        if let Event::Resize(_, _) = ev {
                                            // Terminal resized during inference — erase stale
                                            // viewport and reinit to prevent ghost prompt lines.
                                            reinit_viewport_in_place(&mut terminal, viewport_height, viewport_height)?;
                                        } else if let Event::Key(key) = ev {
                                            // Approval hotkeys during inference
                                            if let MenuContent::Approval { id, .. } = &menu {
                                                let approval_id = id.clone();
                                                let decision = match key.code {
                                                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                                                        Some(ApprovalDecision::Approve)
                                                    }
                                                    KeyCode::Char('n') | KeyCode::Char('N') => {
                                                        Some(ApprovalDecision::Reject)
                                                    }
                                                    KeyCode::Char('a') | KeyCode::Char('A') => {
                                                        // "Always allow" = approve + switch to Auto mode
                                                        approval::set_mode(&shared_mode, ApprovalMode::Auto);
                                                        Some(ApprovalDecision::Approve)
                                                    }
                                                    KeyCode::Char('f') | KeyCode::Char('F') => {
                                                        // Switch prompt to feedback input
                                                        prompt_mode = PromptMode::WizardInput {
                                                            label: "Feedback".into(),
                                                            masked: false,
                                                        };
                                                        menu = MenuContent::WizardTrail(vec![
                                                            ("Action".into(), "Rejected with feedback".into()),
                                                        ]);
                                                        // Store approval ID for when feedback is submitted
                                                        pending_approval_id = Some(approval_id.clone());
                                                        textarea.select_all();
                                                        textarea.cut();
                                                        None // Don't send response yet
                                                    }
                                                    KeyCode::Esc => {
                                                        Some(ApprovalDecision::Reject)
                                                    }
                                                    _ => None,
                                                };
                                                if let Some(d) = decision {
                                                    menu = MenuContent::None;
                                                    let _ = cmd_tx
                                                        .send(EngineCommand::ApprovalResponse {
                                                            id: approval_id,
                                                            decision: d,
                                                        })
                                                        .await;
                                                }
                                                continue;
                                            }

                                            // LoopCap hotkeys
                                            if matches!(menu, MenuContent::LoopCap) {
                                                let action = match key.code {
                                                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                                                        Some(koda_core::loop_guard::LoopContinuation::Continue200)
                                                    }
                                                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                                                        Some(koda_core::loop_guard::LoopContinuation::Stop)
                                                    }
                                                    _ => None,
                                                };
                                                if let Some(a) = action {
                                                    menu = MenuContent::None;
                                                    let _ = cmd_tx
                                                        .send(EngineCommand::LoopDecision { action: a })
                                                        .await;
                                                }
                                                continue;
                                            }

                                            // Feedback text input during inference
                                            if matches!(prompt_mode, PromptMode::WizardInput { .. })
                                                && pending_approval_id.is_some()
                                            {
                                                match key.code {
                                                    KeyCode::Enter => {
                                                        let feedback = textarea.lines().join("\n");
                                                        textarea.select_all();
                                                        textarea.cut();
                                                        prompt_mode = PromptMode::Chat;
                                                        menu = MenuContent::None;
                                                        if let Some(aid) = pending_approval_id.take() {
                                                            let decision = if feedback.trim().is_empty() {
                                                                ApprovalDecision::Reject
                                                            } else {
                                                                ApprovalDecision::RejectWithFeedback { feedback }
                                                            };
                                                            let _ = cmd_tx
                                                                .send(EngineCommand::ApprovalResponse {
                                                                    id: aid,
                                                                    decision,
                                                                })
                                                                .await;
                                                        }
                                                        continue;
                                                    }
                                                    KeyCode::Esc => {
                                                        textarea.select_all();
                                                        textarea.cut();
                                                        prompt_mode = PromptMode::Chat;
                                                        menu = MenuContent::None;
                                                        if let Some(aid) = pending_approval_id.take() {
                                                            let _ = cmd_tx
                                                                .send(EngineCommand::ApprovalResponse {
                                                                    id: aid,
                                                                    decision: ApprovalDecision::Reject,
                                                                })
                                                                .await;
                                                        }
                                                        continue;
                                                    }
                                                    _ => {
                                                        // Let textarea handle the key
                                                        textarea.input(Event::Key(key));
                                                        continue;
                                                    }
                                                }
                                            }

                                            match (key.code, key.modifiers) {
                                                (KeyCode::Enter, KeyModifiers::NONE) => {
                                                    let text = textarea.lines().join("\n");
                                                    if !text.trim().is_empty() {
                                                        textarea.select_all();
                                                        textarea.cut();
                                                        history.push(text.clone());
                                                        tui_history::save_history(&history);
                                                        history_idx = None;
                                                        input_queue.push_back(text);
                                                    }
                                                }
                                                (KeyCode::Esc, _) => {
                                                    cancel_token.cancel();
                                                }
                                                (KeyCode::Char('c'), m)
                                                    if m.contains(KeyModifiers::CONTROL) =>
                                                {
                                                    if crate::interrupt::handle_sigint() {
                                                        restore_terminal(&mut terminal, viewport_height);
                                                        tui_output::err_msg("Force quit.".into());
                                                        std::process::exit(130);
                                                    }
                                                    cancel_token.cancel();
                                                }
                                                (KeyCode::BackTab, _) => {
                                                    approval::cycle_mode(&shared_mode);
                                                }
                                                (KeyCode::Tab, KeyModifiers::NONE) => {
                                                    // Silent Tab completion during inference
                                                    // (no dropdown — would block the event loop)
                                                    let current = textarea.lines().join("\n");
                                                    if let Some(completed) = completer.complete(&current) {
                                                        textarea.select_all();
                                                        textarea.cut();
                                                        textarea.insert_str(&completed);
                                                    }
                                                }
                                                _ => {
                                                    completer.reset();
                                                    textarea.input(Event::Key(key));
                                                }
                                            }
                                        }
                                    }
                                    Some(ui_event) = ui_rx.recv() => {
                                        match ui_event {
                                            UiEvent::Engine(EngineEvent::ApprovalRequest {
                                                id, tool_name, detail, preview,
                                            }) => {
                                                if preview.is_some() {
                                                    renderer.preview_shown = true;
                                                }
                                                // Emit diff preview above the viewport
                                                if let Some(ref prev) = preview {
                                                    let diff_lines = crate::diff_render::render_lines(prev);
                                                    for line in &diff_lines {
                                                        emit_above(&mut terminal, line.clone());
                                                    }
                                                }
                                                // Show approval hotkey bar in menu_area
                                                menu = MenuContent::Approval {
                                                    id,
                                                    tool_name,
                                                    detail,
                                                };
                                                // Hotkey handling is in the crossterm_events
                                                // branch above — no blocking, no terminal reinit
                                            }
                                            UiEvent::Engine(EngineEvent::LoopCapReached { cap, recent_tools }) => {
                                                // Emit cap info above the viewport
                                                emit_above(&mut terminal, Line::from(vec![
                                                    Span::raw("  "),
                                                    Span::styled(
                                                        format!("\u{26a0} Hard cap reached ({cap} iterations)"),
                                                        Style::default().fg(Color::Yellow),
                                                    ),
                                                ]));
                                                for name in &recent_tools {
                                                    emit_above(&mut terminal, Line::from(vec![
                                                        Span::raw("    "),
                                                        Span::styled(format!("\u{25cf} {name}"), Style::default().fg(Color::DarkGray)),
                                                    ]));
                                                }
                                                // Show hotkey bar in menu_area
                                                menu = MenuContent::LoopCap;
                                            }
                                            UiEvent::Engine(event) => {
                                                renderer.render_to_terminal(event, &mut terminal);
                                            }
                                        }
                                    }
                                }
                            }
                        } // end of pinned turn block

                        // Turn completed — cleanup
                        tui_state = TuiState::Idle;
                        inference_start = None;
                        crate::interrupt::reset();
                        session.cancel = tokio_util::sync::CancellationToken::new();

                        // Commit undo snapshots for this turn
                        if let Ok(mut undo) = agent.tools.undo.lock() {
                            undo.commit_turn();
                        }

                        // Drain remaining UI events
                        while let Ok(UiEvent::Engine(e)) = ui_rx.try_recv() {
                            renderer.render_to_terminal(e, &mut terminal);
                        }

                        // Auto-compact
                        if config.auto_compact_threshold > 0 {
                            let ctx_pct = koda_core::context::percentage();
                            if ctx_pct >= config.auto_compact_threshold {
                                let pending = session
                                    .db
                                    .has_pending_tool_calls(&session.id)
                                    .await
                                    .unwrap_or(false);
                                if pending {
                                    if !silent_compact_deferred {
                                        emit_above(
                                            &mut terminal,
                                            Line::from(vec![
                                                ratatui::text::Span::raw("  "),
                                                ratatui::text::Span::styled(
                                                    format!(
                                                        "\u{1f43b} Context at {ctx_pct}% \u{2014} deferring compact (tool calls pending)"
                                                    ),
                                                    Style::default().fg(Color::Yellow),
                                                ),
                                            ]),
                                        );
                                        silent_compact_deferred = true;
                                    }
                                } else {
                                    silent_compact_deferred = false;
                                    emit_above(
                                        &mut terminal,
                                        Line::from(vec![
                                            ratatui::text::Span::raw("  "),
                                            ratatui::text::Span::styled(
                                                format!(
                                                    "\u{1f43b} Context at {ctx_pct}% \u{2014} auto-compacting..."
                                                ),
                                                Style::default().fg(Color::Cyan),
                                            ),
                                        ]),
                                    );
                                    match koda_core::compact::compact_session(
                                        &session.db,
                                        &session.id,
                                        config.max_context_tokens,
                                        &config.model_settings,
                                        &provider,
                                    )
                                    .await
                                    {
                                        Ok(Ok(result)) => {
                                            emit_above(
                                                &mut terminal,
                                                Line::styled(
                                                    format!(
                                                        "  \u{2713} Compacted {} messages \u{2192} ~{} tokens",
                                                        result.deleted, result.summary_tokens
                                                    ),
                                                    Style::default().fg(Color::Green),
                                                ),
                                            );
                                        }
                                        Ok(Err(_skip)) => {} // silently skip
                                        Err(e) => {
                                            emit_above(
                                                &mut terminal,
                                                Line::styled(
                                                    format!(
                                                        "  \u{2717} Auto-compact failed: {e:#}"
                                                    ),
                                                    Style::default().fg(Color::Red),
                                                ),
                                            );
                                        }
                                    }
                                }
                            }
                        }

                        // Loop back to drain queue before blocking on keyboard
                        continue;
                    }
                }
            }
        }

        // Redraw viewport (resize if textarea grew/shrank)
        let mode = approval::read_mode(&shared_mode);
        let ctx = koda_core::context::percentage() as u32;
        maybe_resize_viewport(&mut terminal, &textarea, &mut viewport_height)?;
        terminal.draw(|f| {
            draw_viewport(
                f,
                &textarea,
                &config.model,
                config.model_tier.label(),
                mode,
                ctx,
                tui_state,
                &prompt_mode,
                input_queue.len(),
                inference_start.map(|s| s.elapsed().as_secs()).unwrap_or(0),
                renderer.last_turn_stats.as_ref(),
                &menu,
            );
        })?;

        // ── Idle: wait for keyboard input ────────────────────

        tokio::select! {
            Some(Ok(ev)) = crossterm_events.next() => {
                if let Event::Resize(_, _) = ev {
                    // Terminal resized while idle — erase stale viewport and reinit.
                    reinit_viewport_in_place(&mut terminal, viewport_height, viewport_height)?;
                } else if let Event::Key(key) = ev {
                    // ── Slash menu key interception ───────────
                    // When a menu is active, intercept navigation
                    // and selection keys before normal handling.
                    if !menu.is_none() {
                        let is_up = key.code == KeyCode::Up
                            || (key.code == KeyCode::Char('k')
                                && key.modifiers.contains(KeyModifiers::CONTROL));
                        let is_down = key.code == KeyCode::Down
                            || key.code == KeyCode::Tab
                            || (key.code == KeyCode::Char('j')
                                && key.modifiers.contains(KeyModifiers::CONTROL));

                        if is_up {
                            match &mut menu {
                                MenuContent::Slash(dd) => dd.up(),
                                MenuContent::Model(dd) => dd.up(),
                                MenuContent::Provider(dd) => dd.up(),
                                MenuContent::Session(dd) => dd.up(),
                                MenuContent::File { dropdown: dd, .. } => dd.up(),
                                MenuContent::Approval { .. } | MenuContent::LoopCap | MenuContent::WizardTrail(_) | MenuContent::None => {}
                            }
                            continue;
                        } else if is_down {
                            match &mut menu {
                                MenuContent::Slash(dd) => dd.down(),
                                MenuContent::Model(dd) => dd.down(),
                                MenuContent::Provider(dd) => dd.down(),
                                MenuContent::Session(dd) => dd.down(),
                                MenuContent::File { dropdown: dd, .. } => dd.down(),
                                MenuContent::Approval { .. } | MenuContent::LoopCap | MenuContent::WizardTrail(_) | MenuContent::None => {}
                            }
                            continue;
                        }

                        match key.code {
                            KeyCode::Enter => {
                                match &menu {
                                    MenuContent::Slash(dd) => {
                                        if let Some(item) = dd.selected_item() {
                                            let cmd = item.command.to_string();
                                            textarea.select_all();
                                            textarea.cut();
                                            textarea.insert_str(&cmd);
                                        }
                                    }
                                    MenuContent::Model(dd) => {
                                        if let Some(item) = dd.selected_item() {
                                            let model_id = item.id.clone();
                                            config.model = model_id.clone();
                                            config.model_settings.model = model_id.clone();
                                            config.recalculate_model_derived();
                                            {
                                                let prov = provider.read().await;
                                                config.query_and_apply_capabilities(prov.as_ref()).await;
                                            }
                                            crate::tui_wizards::save_provider(&config);
                                            emit_above(
                                                &mut terminal,
                                                Line::styled(
                                                    format!("  \u{2714} Model set to: {model_id}"),
                                                    Style::default().fg(Color::Green),
                                                ),
                                            );
                                            renderer.model = model_id;
                                        }
                                    }
                                    MenuContent::Provider(dd) => {
                                        if let Some(item) = dd.selected_item() {
                                            let key = item.key;
                                            let ptype = koda_core::config::ProviderType::from_url_or_name("", Some(key));
                                            let base_url = ptype.default_base_url().to_string();
                                            let provider_name = item.name.to_string();

                                            if ptype.requires_api_key() {
                                                // Start wizard: need API key
                                                let env_name = ptype.env_key_name().to_string();
                                                menu = MenuContent::WizardTrail(vec![
                                                    ("Provider".into(), provider_name),
                                                ]);
                                                prompt_mode = PromptMode::WizardInput {
                                                    label: format!("API key for {}", ptype),
                                                    masked: true,
                                                };
                                                provider_wizard = Some(ProviderWizard::NeedApiKey {
                                                    provider_type: ptype,
                                                    base_url,
                                                    env_name,
                                                });
                                                textarea.select_all();
                                                textarea.cut();
                                            } else {
                                                // Local provider: need URL
                                                menu = MenuContent::WizardTrail(vec![
                                                    ("Provider".into(), provider_name),
                                                ]);
                                                prompt_mode = PromptMode::WizardInput {
                                                    label: format!("{} URL", ptype),
                                                    masked: false,
                                                };
                                                provider_wizard = Some(ProviderWizard::NeedUrl {
                                                    provider_type: ptype,
                                                });
                                                textarea.select_all();
                                                textarea.cut();
                                                // Pre-fill with default URL
                                                textarea.insert_str(&base_url);
                                            }
                                        }
                                        continue;
                                    }
                                    MenuContent::Session(dd) => {
                                        if let Some(item) = dd.selected_item() {
                                            if item.is_current {
                                                emit_above(
                                                    &mut terminal,
                                                    Line::styled(
                                                        "  Already in this session.",
                                                        Style::default().fg(Color::DarkGray),
                                                    ),
                                                );
                                            } else {
                                                let target_id = item.id.clone();
                                                let short = &item.short_id;
                                                session.id = target_id;
                                                emit_above(
                                                    &mut terminal,
                                                    Line::from(vec![
                                                        Span::styled(
                                                            "  \u{2714} ",
                                                            Style::default().fg(Color::Green),
                                                        ),
                                                        Span::raw("Resumed session "),
                                                        Span::styled(
                                                            short.to_string(),
                                                            Style::default().fg(Color::Cyan),
                                                        ),
                                                    ]),
                                                );
                                            }
                                        }
                                    }
                                    MenuContent::File {
                                        dropdown,
                                        prefix,
                                    } => {
                                        if let Some(item) = dropdown.selected_item() {
                                            let replacement =
                                                format!("{prefix}@{}", item.path);
                                            textarea.select_all();
                                            textarea.cut();
                                            textarea.insert_str(&replacement);
                                        }
                                    }
                                    MenuContent::Approval { .. }
                                    | MenuContent::LoopCap
                                    | MenuContent::WizardTrail(_)
                                    | MenuContent::None => {}
                                }
                                menu = MenuContent::None;
                                continue;
                            }
                            KeyCode::Esc => {
                                menu = MenuContent::None;
                                // Cancel wizard if active
                                if matches!(prompt_mode, PromptMode::WizardInput { .. }) {
                                    prompt_mode = PromptMode::Chat;
                                    provider_wizard = None;
                                    textarea.select_all();
                                    textarea.cut();
                                }
                                continue;
                            }
                            _ => {
                                // Fall through — let normal handlers process
                                // (typing filters the slash menu via the _ arm)
                            }
                        }
                    }
                    match (key.code, key.modifiers) {
                        // Shift+Enter or Alt+Enter → insert newline
                        // Note: Shift+Enter only works on terminals with kitty
                        // keyboard protocol. Alt+Enter works everywhere.
                        (KeyCode::Enter, m)
                            if m.contains(KeyModifiers::SHIFT)
                                || m.contains(KeyModifiers::ALT) =>
                        {
                            textarea.insert_newline();
                        }
                        (KeyCode::Enter, KeyModifiers::NONE) => {
                            // Wizard input mode: submit value to wizard
                            if matches!(prompt_mode, PromptMode::WizardInput { .. }) {
                                let value = textarea.lines().join("");
                                textarea.select_all();
                                textarea.cut();

                                if let Some(wizard) = provider_wizard.take() {
                                    match wizard {
                                        ProviderWizard::NeedApiKey {
                                            provider_type,
                                            base_url,
                                            env_name,
                                        } => {
                                            if !value.is_empty() {
                                                koda_core::runtime_env::set(&env_name, &value);
                                                // Persist to keystore
                                                if let Ok(mut store) =
                                                    koda_core::keystore::KeyStore::load()
                                                {
                                                    store.set(&env_name, &value);
                                                    let _ = store.save();
                                                }
                                                let masked =
                                                    koda_core::keystore::mask_key(&value);
                                                emit_above(
                                                    &mut terminal,
                                                    Line::styled(
                                                        format!(
                                                            "  \u{2714} {env_name} set to {masked}"
                                                        ),
                                                        Style::default().fg(Color::Green),
                                                    ),
                                                );
                                            }
                                            // Apply provider config
                                            config.provider_type = provider_type.clone();
                                            config.base_url = base_url;
                                            config.model =
                                                provider_type.default_model().to_string();
                                            config.model_settings.model = config.model.clone();
                                            config.recalculate_model_derived();
                                            *provider.write().await =
                                                koda_core::providers::create_provider(&config);
                                            crate::tui_wizards::save_provider(&config);
                                            // Verify connection + auto-select model
                                            let prov = provider.read().await;
                                            if let Ok(models) = prov.list_models().await {
                                                if let Some(first) = models.first() {
                                                    config.model = first.id.clone();
                                                    config.model_settings.model =
                                                        config.model.clone();
                                                    config.recalculate_model_derived();
                                                }
                                                config
                                                    .query_and_apply_capabilities(prov.as_ref())
                                                    .await;
                                                completer.set_model_names(
                                                    models
                                                        .iter()
                                                        .map(|m| m.id.clone())
                                                        .collect(),
                                                );
                                            }
                                            renderer.model = config.model.clone();
                                            emit_above(
                                                &mut terminal,
                                                Line::styled(
                                                    format!(
                                                        "  \u{2714} Provider: {} ({})",
                                                        config.provider_type, config.model
                                                    ),
                                                    Style::default().fg(Color::Green),
                                                ),
                                            );
                                        }
                                        ProviderWizard::NeedUrl { provider_type } => {
                                            let url = if value.is_empty() {
                                                provider_type
                                                    .default_base_url()
                                                    .to_string()
                                            } else {
                                                value
                                            };
                                            config.provider_type = provider_type;
                                            config.base_url = url.clone();
                                            config.model = config
                                                .provider_type
                                                .default_model()
                                                .to_string();
                                            config.model_settings.model = config.model.clone();
                                            config.recalculate_model_derived();
                                            *provider.write().await =
                                                koda_core::providers::create_provider(&config);
                                            crate::tui_wizards::save_provider(&config);
                                            let prov = provider.read().await;
                                            if let Ok(models) = prov.list_models().await {
                                                if let Some(first) = models.first() {
                                                    config.model = first.id.clone();
                                                    config.model_settings.model =
                                                        config.model.clone();
                                                    config.recalculate_model_derived();
                                                }
                                                config
                                                    .query_and_apply_capabilities(prov.as_ref())
                                                    .await;
                                                completer.set_model_names(
                                                    models
                                                        .iter()
                                                        .map(|m| m.id.clone())
                                                        .collect(),
                                                );
                                            }
                                            renderer.model = config.model.clone();
                                            emit_above(
                                                &mut terminal,
                                                Line::styled(
                                                    format!(
                                                        "  \u{2714} Provider: {} at {}",
                                                        config.provider_type, url
                                                    ),
                                                    Style::default().fg(Color::Green),
                                                ),
                                            );
                                        }
                                    }
                                }
                                // Reset wizard state
                                prompt_mode = PromptMode::Chat;
                                menu = MenuContent::None;
                                continue;
                            }

                            // Paste detection: peek ahead for more input.
                            // If characters arrive within 30ms, it's a paste —
                            // insert newline instead of submitting.
                            let is_paste = tokio::time::timeout(
                                std::time::Duration::from_millis(30),
                                crossterm_events.next(),
                            )
                            .await;

                            match is_paste {
                                Ok(Some(Ok(Event::Key(next_key)))) => {
                                    // More input arrived quickly — it's a paste
                                    textarea.insert_newline();
                                    textarea.input(Event::Key(next_key));
                                }
                                _ => {
                                    // Timeout or no event — real Enter, submit
                                    let text = textarea.lines().join("\n");
                                    if !text.trim().is_empty() {
                                        textarea.select_all();
                                        textarea.cut();
                                        history.push(text.clone());
                                        tui_history::save_history(&history);
                                        history_idx = None;
                                        let mode = approval::read_mode(&shared_mode);
                                        let icon = match mode {
                                            ApprovalMode::Safe => "🔍",
                                            ApprovalMode::Strict => "🔒",
                                            ApprovalMode::Auto => "⚡",
                                        };
                                        emit_above(&mut terminal, Line::from(vec![
                                            Span::styled(format!("{icon}> "), Style::default().fg(Color::Cyan)),
                                            Span::raw(text.clone()),
                                        ]));
                                        pending_command = Some(text);
                                    }
                                }
                            }
                        }
                        (KeyCode::Up, KeyModifiers::NONE)
                        | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                            if !history.is_empty() {
                                let idx = match history_idx {
                                    None => history.len() - 1,
                                    Some(i) => i.saturating_sub(1),
                                };
                                history_idx = Some(idx);
                                textarea.select_all();
                                textarea.cut();
                                textarea.insert_str(&history[idx]);
                            }
                        }
                        (KeyCode::Down, KeyModifiers::NONE)
                        | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                            if let Some(idx) = history_idx {
                                if idx + 1 < history.len() {
                                    history_idx = Some(idx + 1);
                                    textarea.select_all();
                                    textarea.cut();
                                    textarea.insert_str(&history[idx + 1]);
                                } else {
                                    history_idx = None;
                                    textarea.select_all();
                                    textarea.cut();
                                }
                            }
                        }
                        (KeyCode::Esc, _) => {
                            textarea.select_all();
                            textarea.cut();
                            history_idx = None;
                        }
                        (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                            textarea.select_all();
                            textarea.cut();
                            history_idx = None;
                        }
                        (KeyCode::Char('d'), m) if m.contains(KeyModifiers::CONTROL) => {
                            if textarea.lines().join("").trim().is_empty() {
                                should_quit = true;
                            }
                        }
                        (KeyCode::BackTab, _) => {
                            approval::cycle_mode(&shared_mode);
                            // Status bar updates on next draw — no scrollback noise
                        }
                        (KeyCode::Tab, KeyModifiers::NONE) => {
                            // Tab cycles through completions (single insertion).
                            // Multi-match dropdowns are now handled by the
                            // auto-dropdowns on / and @ in the _ handler.
                            let current = textarea.lines().join("\n");
                            if let Some(completed) = completer.complete(&current) {
                                textarea.select_all();
                                textarea.cut();
                                textarea.insert_str(&completed);
                                completer.reset();
                            }
                        }
                        _ => {
                            history_idx = None;
                            completer.reset();
                            textarea.input(Event::Key(key));

                            // Update menu state reactively based on input
                            let after_input = textarea.lines().join("\n");
                            let trimmed_after = after_input.trim_end();

                            if trimmed_after.starts_with('/') && !trimmed_after.contains(' ') {
                                // Slash command dropdown
                                if let Some(dd) = crate::widgets::slash_menu::from_input(
                                    crate::completer::SLASH_COMMANDS,
                                    trimmed_after,
                                ) {
                                    menu = MenuContent::Slash(dd);
                                } else if matches!(menu, MenuContent::Slash(_)) {
                                    menu = MenuContent::None;
                                }
                            } else if let Some(at_pos) =
                                crate::completer::find_last_at_token(trimmed_after)
                            {
                                // @file dropdown
                                let partial = &trimmed_after[at_pos + 1..];
                                let prefix = &trimmed_after[..at_pos];
                                let matches =
                                    crate::completer::list_path_matches_public(
                                        &project_root,
                                        partial,
                                    );
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
                                    menu = MenuContent::File {
                                        dropdown: dd,
                                        prefix: prefix.to_string(),
                                    };
                                } else if matches!(menu, MenuContent::File { .. }) {
                                    menu = MenuContent::None;
                                }
                            } else {
                                // Clear menu if it was a slash or file menu
                                if matches!(menu, MenuContent::Slash(_) | MenuContent::File { .. }) {
                                    menu = MenuContent::None;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // ── Cleanup ───────────────────────────────────────────────

    restore_terminal(&mut terminal, viewport_height);
    {
        let mut mcp = agent.mcp_registry.write().await;
        mcp.shutdown();
    }

    crate::startup::print_resume_hint(&session.id);

    Ok(())
}
