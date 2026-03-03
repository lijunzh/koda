//! The main application entry points: interactive REPL and headless mode.
//!
//! Handles user input, command dispatch, and delegates to the inference engine.

use crate::approval::{self, ApprovalMode, Settings};
use crate::config::{KodaConfig, ProviderType};
use crate::db::{Database, Role};
use crate::inference;
use crate::input::{self, KodaHelper};
use crate::memory;
use crate::providers::LlmProvider;

/// Number of recent messages to preserve during compaction.
/// Keeps the user's last question + the assistant's last answer intact.
const COMPACT_PRESERVE_COUNT: usize = 4;
use crate::providers::anthropic::AnthropicProvider;
use crate::providers::gemini::GeminiProvider;
use crate::providers::openai_compat::OpenAiCompatProvider;
use crate::repl::{self, ReplAction};
use crate::tools::ToolRegistry;
use crate::tui::{self, SelectOption};

use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Run the main interactive event loop.
pub async fn run(
    project_root: PathBuf,
    mut config: KodaConfig,
    db: Database,
    session_id: String,
    version_check: tokio::task::JoinHandle<Option<String>>,
) -> Result<()> {
    let provider: Arc<RwLock<Box<dyn LlmProvider>>> =
        Arc::new(RwLock::new(create_provider(&config)));

    // Auto-detect the serving model for LM Studio
    if config.provider_type == ProviderType::LMStudio {
        let prov = provider.read().await;
        match prov.list_models().await {
            Ok(models) if !models.is_empty() => {
                config.model = models[0].id.clone();
                config.model_settings.model = config.model.clone();
                tracing::info!("Auto-detected LM Studio model: {}", config.model);
            }
            Ok(_) => {
                config.model = "(no model loaded)".to_string();
                config.model_settings.model = config.model.clone();
                eprintln!("  \x1b[33m\u{26a0} No model loaded in LM Studio.\x1b[0m");
                eprintln!(
                    "    Load a model in LM Studio, then use \x1b[36m/model\x1b[0m to select it."
                );
            }
            Err(e) => {
                config.model = "(connection failed)".to_string();
                config.model_settings.model = config.model.clone();
                eprintln!(
                    "  \x1b[31m\u{2717} Could not connect to LM Studio at {}\x1b[0m",
                    config.base_url
                );
                eprintln!("    {e}");
                eprintln!(
                    "    Make sure LM Studio is running, or switch provider with \x1b[36m/provider\x1b[0m"
                );
            }
        }
    }

    let recent = db.recent_user_messages(3).await.unwrap_or_default();
    repl::print_banner(&config, &session_id, &recent);

    // Show update hint if version check completed
    if let Ok(Some(latest)) = version_check.await {
        crate::version::print_update_hint(&latest);
    }

    // Initialize MCP servers from .mcp.json configs
    let mcp_registry = Arc::new(tokio::sync::RwLock::new(crate::mcp::McpRegistry::new()));
    {
        let mut mcp = mcp_registry.write().await;
        mcp.start_from_config(&project_root).await;
    }

    let tools = ToolRegistry::new(project_root.clone())
        .with_mcp_registry(mcp_registry.clone());
    let tool_defs = tools.get_definitions(&config.allowed_tools);

    let semantic_memory = memory::load(&project_root)?;
    let system_prompt =
        inference::build_system_prompt(&config.system_prompt, &semantic_memory, &config.agents_dir);

    // REPL with smart completions
    // Initialize approval mode and user settings
    let shared_mode = approval::new_shared_mode(ApprovalMode::Normal);
    let mut settings = Settings::load();

    let mut helper = KodaHelper::new(project_root.clone(), shared_mode.clone());
    {
        let prov = provider.read().await;
        if let Ok(models) = prov.list_models().await {
            helper.model_names = models.iter().map(|m| m.id.clone()).collect();
        }
    }

    let mut rl = rustyline::Editor::with_config(
        rustyline::Config::builder()
            .completion_type(rustyline::CompletionType::List)
            .build(),
    )?;
    rl.set_helper(Some(helper));

    // Esc clears the current line (no-op when already empty).
    // Ctrl-C clears the line when non-empty, or exits when the buffer is empty.
    rl.bind_sequence(
        rustyline::KeyEvent(rustyline::KeyCode::Esc, rustyline::Modifiers::NONE),
        rustyline::EventHandler::Conditional(Box::new(input::EscClearHandler)),
    );
    rl.bind_sequence(
        rustyline::KeyEvent::ctrl('c'),
        rustyline::EventHandler::Conditional(Box::new(input::CtrlCClearHandler)),
    );

    // Shift+Tab cycles approval mode: Plan → Normal → Yolo
    // Note: rustyline normalizes Shift+Tab to BackTab with NONE modifiers
    rl.bind_sequence(
        rustyline::KeyEvent(rustyline::KeyCode::BackTab, rustyline::Modifiers::NONE),
        rustyline::EventHandler::Conditional(Box::new(input::ShiftTabModeHandler::new(
            shared_mode.clone(),
        ))),
    );

    let history_path = history_file_path();
    if history_path.exists() {
        let _ = rl.load_history(&history_path);
    }

    let mut pending_command: Option<String> = None;
    let mut silent_compact_deferred = false;

    loop {
        let input = if let Some(cmd) = pending_command.take() {
            cmd
        } else {
            let prompt = repl::format_prompt(&config.model, approval::read_mode(&shared_mode));
            match rl.readline(&prompt) {
                Ok(line) => line,
                Err(
                    rustyline::error::ReadlineError::Interrupted
                    | rustyline::error::ReadlineError::Eof,
                ) => break,
                Err(e) => return Err(e.into()),
            }
        };

        let input = input.trim().to_string();
        if input.is_empty() {
            continue;
        }

        let _ = rl.add_history_entry(&input);

        // Handle REPL commands
        if input.starts_with('/') {
            match repl::handle_command(&input, &config, &provider).await {
                ReplAction::Quit => {
                    println!("\x1b[36m\u{1f43b} Goodbye!\x1b[0m");
                    break;
                }
                ReplAction::SwitchModel(model) => {
                    config.model = model.clone();
                    config.model_settings.model = model.clone();
                    println!("  \x1b[32m\u{2713}\x1b[0m Model set to: \x1b[36m{model}\x1b[0m");
                    continue;
                }
                ReplAction::PickModel => {
                    let prov = provider.read().await;
                    match prov.list_models().await {
                        Ok(models) if models.is_empty() => {
                            println!(
                                "  \x1b[33mNo models available from {}\x1b[0m",
                                prov.provider_name()
                            );
                        }
                        Ok(models) => {
                            drop(prov);
                            if let Some(h) = rl.helper_mut() {
                                h.model_names = models.iter().map(|m| m.id.clone()).collect();
                            }
                            let current_idx = models
                                .iter()
                                .position(|m| m.id == config.model)
                                .unwrap_or(0);
                            let options: Vec<SelectOption> = models
                                .iter()
                                .map(|m| {
                                    let desc = if m.id == config.model {
                                        "\u{25c0} current".to_string()
                                    } else {
                                        String::new()
                                    };
                                    SelectOption::new(&m.id, desc)
                                })
                                .collect();
                            match tui::select("\u{1f43b} Select a model", &options, current_idx) {
                                Ok(Some(idx)) => {
                                    config.model = models[idx].id.clone();
                                    config.model_settings.model = config.model.clone();
                                    println!(
                                        "  \x1b[32m\u{2713}\x1b[0m Model set to: \x1b[36m{}\x1b[0m",
                                        config.model
                                    );
                                }
                                Ok(None) => println!("  \x1b[90mCancelled.\x1b[0m"),
                                Err(e) => println!("  \x1b[31mTUI error: {e}\x1b[0m"),
                            }
                        }
                        Err(e) => println!("  \x1b[31mFailed to list models: {e}\x1b[0m"),
                    }
                    continue;
                }
                ReplAction::SetupProvider(ptype, base_url) => {
                    handle_setup_provider(&mut config, &provider, &mut rl, ptype, base_url).await;
                    continue;
                }
                ReplAction::PickProvider => {
                    handle_pick_provider(&mut config, &provider, &mut rl).await;
                    continue;
                }

                ReplAction::ShowHelp => {
                    let commands = [
                        ("/agent", "List available sub-agents"),
                        ("/compact", "Summarize conversation to reclaim context"),
                        ("/cost", "Show token usage for this session"),
                        ("/diff", "Show git diff / review / commit message"),
                        ("/mcp", "MCP servers: status / add / remove / restart"),
                        ("/memory", "View/save project & global memory"),
                        ("/model", "Pick a model interactively"),
                        ("/provider", "Switch LLM provider"),
                        ("/sessions", "List/resume/delete sessions"),
                        ("/trust", "Set approval mode (always / auto / never)"),
                        ("/exit", "Quit the session"),
                    ];
                    let options: Vec<SelectOption> = commands
                        .iter()
                        .map(|(cmd, desc)| SelectOption::new(*cmd, *desc))
                        .collect();
                    if let Ok(Some(idx)) = tui::select("\u{1f43b} Commands", &options, 0) {
                        let (cmd, _) = commands[idx];
                        pending_command = Some(cmd.to_string());
                    }
                    println!();
                    println!(
                        "  \x1b[90mTips: @file to attach context \u{00b7} Ctrl+C to clear input \u{00b7} Ctrl+D to exit\x1b[0m"
                    );
                    continue;
                }
                ReplAction::ShowCost => {
                    match db.session_token_usage(&session_id).await {
                        Ok(u) => {
                            let total = u.prompt_tokens
                                + u.completion_tokens
                                + u.cache_read_tokens
                                + u.cache_creation_tokens;
                            println!();
                            println!("  \x1b[1m\u{1f43b} Session Cost\x1b[0m");
                            println!();
                            println!("  Prompt tokens:     \x1b[36m{:>8}\x1b[0m", u.prompt_tokens);
                            println!(
                                "  Completion tokens: \x1b[36m{:>8}\x1b[0m",
                                u.completion_tokens
                            );
                            if u.cache_read_tokens > 0 {
                                println!(
                                    "  Cache read tokens: \x1b[32m{:>8}\x1b[0m",
                                    u.cache_read_tokens
                                );
                            }
                            if u.cache_creation_tokens > 0 {
                                println!(
                                    "  Cache write tokens:\x1b[33m{:>8}\x1b[0m",
                                    u.cache_creation_tokens
                                );
                            }
                            if u.thinking_tokens > 0 {
                                println!(
                                    "  Thinking tokens:   \x1b[35m{:>8}\x1b[0m",
                                    u.thinking_tokens
                                );
                            }
                            println!("  Total tokens:      \x1b[1m{total:>8}\x1b[0m");
                            println!("  API calls:         \x1b[90m{:>8}\x1b[0m", u.api_calls);
                            println!();
                            println!("  \x1b[90mModel: {}\x1b[0m", config.model);
                            println!("  \x1b[90mProvider: {}\x1b[0m", config.provider_type);
                        }
                        Err(e) => println!("  \x1b[31mError: {e}\x1b[0m"),
                    }
                    continue;
                }
                ReplAction::ListSessions => {
                    match db.list_sessions(10, &project_root).await {
                        Ok(sessions) => {
                            println!();
                            println!("  \x1b[1m\u{1f43b} Recent Sessions\x1b[0m");
                            println!();
                            if sessions.is_empty() {
                                println!("  \x1b[90mNo sessions found.\x1b[0m");
                            } else {
                                for s in &sessions {
                                    let marker = if s.id == session_id {
                                        " \x1b[32m← current\x1b[0m"
                                    } else {
                                        ""
                                    };
                                    println!(
                                        "  \x1b[36m{}\x1b[0m  \x1b[90m{}  {}  {} msgs  {}k tokens\x1b[0m{}",
                                        &s.id[..8],
                                        s.created_at,
                                        s.agent_name,
                                        s.message_count,
                                        s.total_tokens / 1000,
                                        marker,
                                    );
                                }
                            }
                            println!();
                            println!("  \x1b[90mResume: koda --session <id>\x1b[0m");
                            println!("  \x1b[90mDelete: /sessions delete <id>\x1b[0m");
                        }
                        Err(e) => println!("  \x1b[31mError: {e}\x1b[0m"),
                    }
                    continue;
                }
                ReplAction::DeleteSession(ref id) => {
                    if id == &session_id {
                        println!("  \x1b[31mCannot delete the current session.\x1b[0m");
                    } else {
                        // Match by prefix
                        match db.list_sessions(100, &project_root).await {
                            Ok(sessions) => {
                                let matches: Vec<_> =
                                    sessions.iter().filter(|s| s.id.starts_with(id)).collect();
                                match matches.len() {
                                    0 => println!(
                                        "  \x1b[31mNo session found matching '{id}'.\x1b[0m"
                                    ),
                                    1 => {
                                        let full_id = &matches[0].id;
                                        match db.delete_session(full_id).await {
                                            Ok(true) => println!(
                                                "  \x1b[32m\u{2713}\x1b[0m Deleted session {}",
                                                &full_id[..8]
                                            ),
                                            Ok(false) => {
                                                println!("  \x1b[31mSession not found.\x1b[0m")
                                            }
                                            Err(e) => println!("  \x1b[31mError: {e}\x1b[0m"),
                                        }
                                    }
                                    n => println!(
                                        "  \x1b[31mAmbiguous: '{id}' matches {n} sessions. Be more specific.\x1b[0m"
                                    ),
                                }
                            }
                            Err(e) => println!("  \x1b[31mError: {e}\x1b[0m"),
                        }
                    }
                    continue;
                }
                ReplAction::InjectPrompt(prompt) => {
                    // Treat injected prompt as user input (used by /diff review, /diff commit)
                    pending_command = Some(prompt);
                    continue;
                }
                ReplAction::Compact => {
                    handle_compact(&db, &session_id, &config, &provider, false).await;
                    continue;
                }
                ReplAction::McpCommand(ref args) => {
                    handle_mcp_command(args, &mcp_registry, &project_root).await;
                    continue;
                }
                ReplAction::SetTrust(mode_name) => {
                    let new_mode = if let Some(ref name) = mode_name {
                        // Explicit: /trust yolo
                        ApprovalMode::from_str(name)
                    } else {
                        // Interactive picker
                        pick_trust_mode(approval::read_mode(&shared_mode))
                    };
                    if let Some(m) = new_mode {
                        approval::set_mode(&shared_mode, m);
                        println!(
                            "  \x1b[32m\u{2713}\x1b[0m Trust: \x1b[1m{}\x1b[0m \u{2014} {}",
                            m.label(),
                            m.description()
                        );
                    } else if let Some(ref name) = mode_name {
                        println!(
                            "  \x1b[31m\u{2717}\x1b[0m Unknown trust level '{}'. Use: plan, normal, yolo",
                            name
                        );
                    }
                    continue;
                }
                ReplAction::Handled => continue,
                ReplAction::NotACommand => {}
            }
        }

        // Process @file references
        let processed = input::process_input(&input, &project_root);
        // Show attached images
        if !processed.images.is_empty() {
            for (i, _img) in processed.images.iter().enumerate() {
                println!("  \x1b[35m\u{1f5bc} Image {}\x1b[0m", i + 1);
            }
        }

        let user_message =
            if let Some(context) = input::format_context_files(&processed.context_files) {
                if !processed.context_files.is_empty() {
                    for f in &processed.context_files {
                        println!("  \x1b[36m\u{1f4ce} {}\x1b[0m", f.path);
                    }
                }
                format!("{}\n\n{context}", processed.prompt)
            } else {
                processed.prompt.clone()
            };

        db.insert_message(
            &session_id,
            &Role::User,
            Some(&user_message),
            None,
            None,
            None,
        )
        .await?;

        // Pass images to inference (they're not stored in DB, only used in-flight)
        let pending_images = if processed.images.is_empty() {
            None
        } else {
            Some(processed.images)
        };

        // Run the inference loop
        let prov = provider.read().await;
        let current_mode = approval::read_mode(&shared_mode);
        inference::inference_loop(
            &project_root,
            &config,
            &db,
            &session_id,
            &system_prompt,
            prov.as_ref(),
            &tools,
            &tool_defs,
            pending_images,
            current_mode,
            &mut settings,
        )
        .await?;

        // Auto-compact when context window gets crowded
        if config.auto_compact_threshold > 0 {
            let ctx_pct = crate::context::percentage();
            if ctx_pct >= config.auto_compact_threshold {
                // Fix 5: Defer compaction if tool calls are still pending
                let pending = db.has_pending_tool_calls(&session_id).await.unwrap_or(false);
                if pending {
                    if !silent_compact_deferred {
                        println!();
                        println!("  \x1b[33m\u{1f43b} Context at {ctx_pct}% — deferring compact (tool calls pending)\x1b[0m");
                        silent_compact_deferred = true;
                    }
                } else {
                    silent_compact_deferred = false;
                    println!();
                    println!("  \x1b[36m\u{1f43b} Context at {ctx_pct}% — auto-compacting...\x1b[0m");
                    handle_compact(&db, &session_id, &config, &provider, true).await;
                }
            }
        }
    }

    if let Some(parent) = history_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = rl.save_history(&history_path);

    // Shut down MCP servers
    {
        let mut mcp = mcp_registry.write().await;
        mcp.shutdown();
    }

    Ok(())
}

// ── Headless mode ──────────────────────────────────────────────

/// Run a single prompt and exit. Returns process exit code (0 = success).
pub async fn run_headless(
    project_root: PathBuf,
    config: KodaConfig,
    db: Database,
    session_id: String,
    prompt: String,
    output_format: &str,
) -> Result<i32> {
    let provider: Arc<RwLock<Box<dyn LlmProvider>>> =
        Arc::new(RwLock::new(create_provider(&config)));

    let tools = crate::tools::ToolRegistry::new(project_root.clone());
    let tool_defs = tools.get_definitions(&config.allowed_tools);

    let semantic_memory = memory::load(&project_root)?;
    let system_prompt =
        inference::build_system_prompt(&config.system_prompt, &semantic_memory, &config.agents_dir);

    // Process @file references and images (same as interactive mode)
    let processed = input::process_input(&prompt, &project_root);
    let user_message = if let Some(context) = input::format_context_files(&processed.context_files)
    {
        format!("{}\n\n{context}", processed.prompt)
    } else {
        processed.prompt.clone()
    };

    let pending_images = if processed.images.is_empty() {
        None
    } else {
        Some(processed.images)
    };

    db.insert_message(
        &session_id,
        &Role::User,
        Some(&user_message),
        None,
        None,
        None,
    )
    .await?;

    // Headless mode: use Yolo (no confirmation prompts)
    let mut settings = Settings::load();

    // Run inference (tools work, streaming prints to stdout)
    let prov = provider.read().await;
    let result = inference::inference_loop(
        &project_root,
        &config,
        &db,
        &session_id,
        &system_prompt,
        prov.as_ref(),
        &tools,
        &tool_defs,
        pending_images,
        ApprovalMode::Yolo,
        &mut settings,
    )
    .await;

    // For JSON output, wrap the last assistant response
    if output_format == "json" {
        let last_response = db
            .last_assistant_message(&session_id)
            .await
            .unwrap_or_default();
        let json = serde_json::json!({
            "success": result.is_ok(),
            "response": last_response,
            "session_id": session_id,
            "model": config.model,
        });
        println!("{}", serde_json::to_string_pretty(&json)?);
    }

    match result {
        Ok(()) => Ok(0),
        Err(e) => {
            eprintln!("Error: {e}");
            Ok(1)
        }
    }
}

// ── Compact handler ───────────────────────────────────────────

/// Compact the conversation by summarizing history via the LLM.
/// When `silent` is true (auto-compact), suppresses the "too short" message.
///
/// Improvements over v1:
/// - Preserves the most recent messages (Fix 1)
/// - Inserts summary as `system` role, not `user` (Fix 3)
/// - Adds a continuation instruction for the LLM (Fix 2)
/// - Checks for pending tool calls before proceeding (Fix 5)
/// - Uses a structured summarization prompt (Fix 6)
async fn handle_compact(
    db: &Database,
    session_id: &str,
    config: &KodaConfig,
    provider: &Arc<RwLock<Box<dyn LlmProvider>>>,
    silent: bool,
) {
    use crate::providers::ChatMessage;

    // Fix 5: Defer compaction if tool calls are pending
    if let Ok(true) = db.has_pending_tool_calls(session_id).await {
        if !silent {
            println!("  \x1b[33mTool calls are still pending — deferring compact.\x1b[0m");
        }
        return;
    }

    // Load current conversation
    let history = match db.load_context(session_id, config.max_context_tokens).await {
        Ok(msgs) => msgs,
        Err(e) => {
            if !silent {
                println!("  \x1b[31mError loading conversation: {e}\x1b[0m");
            }
            return;
        }
    };

    if history.len() < 4 {
        if !silent {
            println!(
                "  \x1b[90mConversation is too short to compact ({} messages).\x1b[0m",
                history.len()
            );
        }
        return;
    }

    // Format the conversation for summarization
    let mut conversation_text = String::new();
    for msg in &history {
        let role = msg.role.as_str();
        if let Some(ref content) = msg.content {
            // Cap individual messages to avoid blowing the summary prompt
            let truncated: String = content.chars().take(2000).collect();
            conversation_text.push_str(&format!("[{role}]: {truncated}\n\n"));
        }
        if let Some(ref tool_calls) = msg.tool_calls {
            let truncated: String = tool_calls.chars().take(500).collect();
            conversation_text.push_str(&format!("[{role} tool_calls]: {truncated}\n\n"));
        }
    }

    // Cap total conversation text
    if conversation_text.len() > 20_000 {
        let mut end = 20_000;
        while end > 0 && !conversation_text.is_char_boundary(end) {
            end -= 1;
        }
        conversation_text.truncate(end);
        conversation_text.push_str("\n\n[...truncated for summarization...]");
    }

    println!();
    println!(
        "  \x1b[36m\u{1f43b} Compacting {} messages (preserving last {})...\x1b[0m",
        history.len(),
        COMPACT_PRESERVE_COUNT,
    );

    // Fix 6: Structured summarization prompt preserving code-relevant context
    let summary_prompt = format!(
        "Summarize the conversation below. This summary will replace the older messages \
         so an AI assistant can continue the session seamlessly.\n\
         \n\
         Preserve ALL of the following — do NOT omit any section:\n\
         \n\
         1. **User Intent** — Every goal, request, and requirement the user stated.\n\
         2. **Key Decisions** — Decisions made and their rationale.\n\
         3. **Files & Code** — Every file created, modified, or deleted. Include file paths, \n\
            key function/struct names, and the purpose of each change.\n\
         4. **Errors & Fixes** — Bugs encountered, error messages, and how they were resolved.\n\
         5. **Current State** — What is working, what has been tested, what the project \n\
            looks like right now.\n\
         6. **Pending Tasks** — Anything unfinished or explicitly deferred.\n\
         7. **Next Step** — Only if one was clearly stated or implied.\n\
         \n\
         Use concise bullet points. Do not add new ideas or suggestions.\n\
         \n\
         ---\n\n{conversation_text}"
    );

    let messages = vec![ChatMessage::text("user", &summary_prompt)];

    // Call the LLM for the summary (no tools, no streaming)
    let prov = provider.read().await;
    let response = match prov.chat(&messages, &[], &config.model_settings).await {
        Ok(r) => r,
        Err(e) => {
            println!("  \x1b[31mFailed to generate summary: {e}\x1b[0m");
            return;
        }
    };

    let summary = match response.content {
        Some(text) if !text.trim().is_empty() => text,
        _ => {
            println!("  \x1b[31mLLM returned an empty summary. Aborting compact.\x1b[0m");
            return;
        }
    };

    // Fix 3: Summary is wrapped clearly for the LLM context (inserted as system role in DB)
    let compact_message =
        format!("[Compacted conversation summary]\n\n{summary}");

    // Fix 1: Preserve recent messages; Fix 2 & 3: DB inserts system + assistant continuation
    match db
        .compact_session(session_id, &compact_message, COMPACT_PRESERVE_COUNT)
        .await
    {
        Ok(deleted) => {
            let summary_tokens = summary.len() / 4;
            println!(
                "  \x1b[32m\u{2713}\x1b[0m Compacted {deleted} messages → ~{summary_tokens} tokens"
            );
            println!(
                "  \x1b[90mConversation context has been summarized. Continue as normal!\x1b[0m"
            );
        }
        Err(e) => {
            println!("  \x1b[31mFailed to compact session: {e}\x1b[0m");
        }
    }
    println!();
}

// ── MCP command handler ──────────────────────────────────────

/// Handle `/mcp` subcommands: status, add, remove, restart.
async fn handle_mcp_command(
    args: &str,
    mcp_registry: &Arc<tokio::sync::RwLock<crate::mcp::McpRegistry>>,
    project_root: &std::path::Path,
) {
    let parts: Vec<&str> = args.splitn(3, ' ').collect();
    let subcommand = parts.first().map(|s| s.trim()).unwrap_or("");

    match subcommand {
        "" | "status" => {
            // Show MCP server status
            let registry = mcp_registry.read().await;
            let servers = registry.server_info();
            println!();
            if servers.is_empty() {
                println!("  \x1b[90mNo MCP servers connected.\x1b[0m");
                println!("  \x1b[90mAdd servers via .mcp.json or /mcp add <name> <command> [args...]\x1b[0m");
            } else {
                println!("  \x1b[1m\u{1f50c} MCP Servers\x1b[0m");
                println!();
                for server in &servers {
                    let cmd = if server.args.is_empty() {
                        server.command.clone()
                    } else {
                        format!("{} {}", server.command, server.args.join(" "))
                    };
                    println!(
                        "  \x1b[32m\u{25cf}\x1b[0m \x1b[1m{}\x1b[0m \u{2014} {} tool(s)",
                        server.name, server.tool_count
                    );
                    println!("    \x1b[90m{cmd}\x1b[0m");
                    for tool_name in &server.tool_names {
                        println!("    \x1b[36m\u{2022}\x1b[0m {tool_name}");
                    }
                }
            }
            println!();
        }

        "add" => {
            // /mcp add <name> <command> [args...]
            let rest = args.strip_prefix("add").unwrap_or("").trim();
            let add_parts: Vec<&str> = rest.splitn(2, ' ').collect();
            if add_parts.len() < 2 {
                println!("  \x1b[33mUsage: /mcp add <name> <command> [args...]\x1b[0m");
                println!("  \x1b[90mExample: /mcp add filesystem npx -y @modelcontextprotocol/server-filesystem /tmp\x1b[0m");
                return;
            }
            let name = add_parts[0].to_string();
            let cmd_parts: Vec<&str> = add_parts[1].split_whitespace().collect();
            let command = cmd_parts[0].to_string();
            let cmd_args: Vec<String> = cmd_parts[1..].iter().map(|s| s.to_string()).collect();

            let config = crate::mcp::config::McpServerConfig {
                command: command.clone(),
                args: cmd_args,
                env: std::collections::HashMap::new(),
                timeout: None,
            };

            // Save to .mcp.json
            if let Err(e) = crate::mcp::config::save_server_to_project(project_root, &name, &config) {
                println!("  \x1b[31mFailed to save config: {e}\x1b[0m");
                return;
            }

            // Connect
            println!("  \x1b[36m\u{1f50c} Connecting to '{name}'...\x1b[0m");
            let mut registry = mcp_registry.write().await;
            match registry.add_server(name.clone(), config).await {
                Ok(()) => {
                    let tool_count = registry.server_info()
                        .iter()
                        .find(|s| s.name == name)
                        .map(|s| s.tool_count)
                        .unwrap_or(0);
                    println!(
                        "  \x1b[32m\u{2713}\x1b[0m Added '{}' ({} tools). Saved to .mcp.json",
                        name, tool_count
                    );
                }
                Err(e) => {
                    println!("  \x1b[31m\u{2717}\x1b[0m Failed to connect: {e}");
                }
            }
        }

        "remove" => {
            let name = args.strip_prefix("remove").unwrap_or("").trim();
            if name.is_empty() {
                println!("  \x1b[33mUsage: /mcp remove <name>\x1b[0m");
                return;
            }
            let mut registry = mcp_registry.write().await;
            if registry.remove_server(name) {
                // Also remove from .mcp.json
                let _ = crate::mcp::config::remove_server_from_project(project_root, name);
                println!("  \x1b[32m\u{2713}\x1b[0m Removed MCP server '{name}'");
            } else {
                println!("  \x1b[31mMCP server '{name}' not found\x1b[0m");
            }
        }

        "restart" => {
            let name = args.strip_prefix("restart").unwrap_or("").trim();
            let mut registry = mcp_registry.write().await;
            if name.is_empty() {
                println!("  \x1b[36m\u{1f50c} Restarting all MCP servers...\x1b[0m");
                registry.restart_all(project_root).await;
                println!("  \x1b[32m\u{2713}\x1b[0m Done");
            } else {
                println!("  \x1b[36m\u{1f50c} Restarting '{name}'...\x1b[0m");
                match registry.restart_server(name, project_root).await {
                    Ok(()) => println!("  \x1b[32m\u{2713}\x1b[0m Restarted '{name}'"),
                    Err(e) => println!("  \x1b[31m\u{2717}\x1b[0m Failed: {e}"),
                }
            }
        }

        other => {
            println!("  \x1b[33mUnknown MCP command: {other}\x1b[0m");
            println!("  \x1b[90mUsage: /mcp [status|add|remove|restart]\x1b[0m");
        }
    }
}

// ── Provider setup handlers ───────────────────────────────────

async fn handle_setup_provider(
    config: &mut KodaConfig,
    provider: &Arc<RwLock<Box<dyn LlmProvider>>>,
    rl: &mut rustyline::Editor<KodaHelper, rustyline::history::DefaultHistory>,
    ptype: ProviderType,
    base_url: String,
) {
    let env_name = ptype.env_key_name();
    let key_missing = ptype != ProviderType::LMStudio && !crate::runtime_env::is_set(env_name);
    let is_same_provider = ptype == config.provider_type;

    config.provider_type = ptype.clone();
    config.base_url = base_url;
    config.model = ptype.default_model().to_string();
    config.model_settings.model = config.model.clone();

    if key_missing || (is_same_provider && ptype != ProviderType::LMStudio) {
        let prompt_msg = if is_same_provider {
            format!(
                "  Update {} API key (enter to keep current): ",
                config.provider_type
            )
        } else {
            println!("  \x1b[33m{}\x1b[0m is not set.", env_name);
            format!("  Paste your {} API key: ", config.provider_type)
        };
        match rl.readline(&prompt_msg) {
            Ok(key) => {
                let key = key.trim().to_string();
                if key.is_empty() {
                    if !is_same_provider {
                        println!("  \x1b[31mNo key provided, provider not changed.\x1b[0m");
                        return;
                    }
                } else {
                    crate::runtime_env::set(env_name, &key);
                    let masked = crate::keystore::mask_key(&key);
                    println!(
                        "  \x1b[32m\u{2713}\x1b[0m {} set to \x1b[90m{masked}\x1b[0m",
                        env_name
                    );
                    if let Ok(mut store) = crate::keystore::KeyStore::load() {
                        store.set(env_name, &key);
                        if let Err(e) = store.save() {
                            println!("  \x1b[33m\u{26a0} Could not persist key: {e}\x1b[0m");
                        } else if let Ok(path) = crate::keystore::KeyStore::keys_path() {
                            println!(
                                "  \x1b[32m\u{2713}\x1b[0m Saved to \x1b[90m{}\x1b[0m",
                                path.display()
                            );
                        }
                    }
                }
            }
            Err(_) => {
                println!("  \x1b[31mCancelled, provider not changed.\x1b[0m");
                return;
            }
        }
    }

    *provider.write().await = create_provider(config);
    println!(
        "  \x1b[32m\u{2713}\x1b[0m Provider: \x1b[36m{}\x1b[0m",
        config.provider_type
    );

    let prov = provider.read().await;
    match prov.list_models().await {
        Ok(models) => {
            // Auto-select first model from API instead of using hardcoded default
            if let Some(first) = models.first() {
                config.model = first.id.clone();
                config.model_settings.model = config.model.clone();
            }
            println!("  \x1b[32m\u{2713}\x1b[0m Connection verified! Available models:");
            for m in &models {
                let current = if m.id == config.model {
                    " \x1b[32m\u{25c0} selected\x1b[0m"
                } else {
                    ""
                };
                println!("      {}{current}", m.id);
            }
        }
        Err(e) => {
            println!("  \x1b[33m\u{26a0} Could not verify connection: {e}\x1b[0m");
            println!(
                "    Model set to: \x1b[36m{}\x1b[0m (unverified)",
                config.model
            );
        }
    }
    println!();
}

async fn handle_pick_provider(
    config: &mut KodaConfig,
    provider: &Arc<RwLock<Box<dyn LlmProvider>>>,
    rl: &mut rustyline::Editor<KodaHelper, rustyline::history::DefaultHistory>,
) {
    let providers = repl::PROVIDERS;
    let current_idx = providers
        .iter()
        .position(|(key, _, _)| {
            ProviderType::from_url_or_name("", Some(key)) == config.provider_type
        })
        .unwrap_or(0);
    let options: Vec<SelectOption> = providers
        .iter()
        .map(|(_, name, url)| SelectOption::new(*name, *url))
        .collect();

    let selection = match tui::select("\u{1f43b} Select a provider", &options, current_idx) {
        Ok(Some(idx)) => idx,
        Ok(None) => {
            println!("  \x1b[90mCancelled.\x1b[0m");
            return;
        }
        Err(e) => {
            println!("  \x1b[31mTUI error: {e}\x1b[0m");
            return;
        }
    };

    let (key, _, _) = providers[selection];
    let ptype = ProviderType::from_url_or_name("", Some(key));
    let base_url = ptype.default_base_url().to_string();

    handle_setup_provider(config, provider, rl, ptype, base_url).await;
}

// ── Utilities ─────────────────────────────────────────────────

/// Interactive trust mode picker (arrow-key menu).
fn pick_trust_mode(current: ApprovalMode) -> Option<ApprovalMode> {
    use ApprovalMode::*;
    let modes = [Plan, Normal, Yolo];
    let options: Vec<SelectOption> = modes
        .iter()
        .map(|m| {
            let label = match m {
                Plan => "\u{1f4cb} plan",
                Normal => "\u{1f43b} normal",
                Yolo => "\u{26a1} yolo",
            };
            SelectOption::new(label, m.description())
        })
        .collect();

    let initial = modes.iter().position(|m| *m == current).unwrap_or(1);
    match tui::select("\u{1f43b} Trust level", &options, initial) {
        Ok(Some(idx)) => Some(modes[idx]),
        _ => None,
    }
}

fn history_file_path() -> PathBuf {
    let config_dir = std::env::var("XDG_CONFIG_HOME")
        .or_else(|_| std::env::var("HOME").map(|h| format!("{h}/.config")))
        .or_else(|_| std::env::var("USERPROFILE").map(|h| format!("{h}/.config")))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(config_dir).join("koda").join("history")
}

/// Create an LLM provider from the config.
pub fn create_provider(config: &KodaConfig) -> Box<dyn LlmProvider> {
    let api_key = crate::runtime_env::get(config.provider_type.env_key_name());
    match config.provider_type {
        ProviderType::Anthropic => {
            let key = api_key.unwrap_or_else(|| {
                tracing::warn!("No ANTHROPIC_API_KEY set");
                String::new()
            });
            Box::new(AnthropicProvider::new(key, Some(&config.base_url)))
        }
        ProviderType::Gemini => {
            let key = api_key.unwrap_or_else(|| {
                tracing::warn!("No GEMINI_API_KEY set");
                String::new()
            });
            Box::new(GeminiProvider::new(key, Some(&config.base_url)))
        }
        _ => Box::new(OpenAiCompatProvider::new(&config.base_url, api_key)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_provider_openai() {
        let config = KodaConfig::default_for_testing(ProviderType::OpenAI);
        let provider = create_provider(&config);
        assert_eq!(provider.provider_name(), "openai-compat");
    }

    #[test]
    fn test_create_provider_anthropic() {
        let config = KodaConfig::default_for_testing(ProviderType::Anthropic);
        let provider = create_provider(&config);
        assert_eq!(provider.provider_name(), "anthropic");
    }

    #[test]
    fn test_create_provider_lmstudio() {
        let config = KodaConfig::default_for_testing(ProviderType::LMStudio);
        let provider = create_provider(&config);
        assert_eq!(provider.provider_name(), "openai-compat");
    }

    #[test]
    fn test_create_provider_gemini() {
        let config = KodaConfig::default_for_testing(ProviderType::Gemini);
        let provider = create_provider(&config);
        assert_eq!(provider.provider_name(), "gemini");
    }

    #[test]
    fn test_history_file_path() {
        let path = history_file_path();
        assert!(path.to_string_lossy().contains("koda"));
        assert!(path.to_string_lossy().contains("history"));
    }
}
