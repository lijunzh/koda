//! REPL command handlers — /compact, /mcp, /provider, /trust.
//!
//! Extracted from app.rs to keep each file under 600 lines.

use crate::select_menu::SelectOption;
use koda_core::approval::ApprovalMode;
use koda_core::config::{KodaConfig, ProviderType};
use koda_core::db::Database;
use koda_core::providers::LlmProvider;

use std::sync::Arc;
use tokio::sync::RwLock;

/// Number of recent messages to preserve during compaction.
const COMPACT_PRESERVE_COUNT: usize = 4;

// ── Raw stdin input ──────────────────────────────────────────

/// Read a line from stdin without rustyline (no completions needed).
/// Used for API key and URL prompts during provider setup.
fn read_line_raw(prompt: &str) -> anyhow::Result<String> {
    use std::io::Write;
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
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
pub(crate) async fn handle_compact(
    db: &Database,
    session_id: &str,
    config: &KodaConfig,
    provider: &Arc<RwLock<Box<dyn LlmProvider>>>,
    silent: bool,
) {
    use koda_core::providers::ChatMessage;

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
    let compact_message = format!("[Compacted conversation summary]\n\n{summary}");

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
pub(crate) async fn handle_mcp_command(
    args: &str,
    mcp_registry: &Arc<tokio::sync::RwLock<koda_core::mcp::McpRegistry>>,
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
                println!(
                    "  \x1b[90mAdd servers via .mcp.json or /mcp add <name> <command> [args...]\x1b[0m"
                );
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
                println!(
                    "  \x1b[90mExample: /mcp add filesystem npx -y @modelcontextprotocol/server-filesystem /tmp\x1b[0m"
                );
                return;
            }
            let name = add_parts[0].to_string();
            let cmd_parts: Vec<&str> = add_parts[1].split_whitespace().collect();
            let command = cmd_parts[0].to_string();
            let cmd_args: Vec<String> = cmd_parts[1..].iter().map(|s| s.to_string()).collect();

            let config = koda_core::mcp::config::McpServerConfig {
                command: command.clone(),
                args: cmd_args,
                env: std::collections::HashMap::new(),
                timeout: None,
            };

            // Save to .mcp.json
            if let Err(e) =
                koda_core::mcp::config::save_server_to_project(project_root, &name, &config)
            {
                println!("  \x1b[31mFailed to save config: {e}\x1b[0m");
                return;
            }

            // Connect
            println!("  \x1b[36m\u{1f50c} Connecting to '{name}'...\x1b[0m");
            let mut registry = mcp_registry.write().await;
            match registry.add_server(name.clone(), config).await {
                Ok(()) => {
                    let tool_count = registry
                        .server_info()
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
                let _ = koda_core::mcp::config::remove_server_from_project(project_root, name);
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

pub(crate) async fn handle_setup_provider(
    config: &mut KodaConfig,
    provider: &Arc<RwLock<Box<dyn LlmProvider>>>,
    ptype: ProviderType,
    base_url: String,
) {
    let env_name = ptype.env_key_name();
    let key_missing = ptype.requires_api_key() && !koda_core::runtime_env::is_set(env_name);
    let is_same_provider = ptype == config.provider_type;

    config.provider_type = ptype.clone();
    config.base_url = base_url;
    config.model = ptype.default_model().to_string();
    config.model_settings.model = config.model.clone();

    if key_missing || (is_same_provider && ptype.requires_api_key()) {
        let prompt_msg = if is_same_provider {
            format!(
                "  Update {} API key (enter to keep current): ",
                config.provider_type
            )
        } else {
            println!("  \x1b[33m{}\x1b[0m is not set.", env_name);
            format!("  Paste your {} API key: ", config.provider_type)
        };
        match read_line_raw(&prompt_msg) {
            Ok(key) => {
                if key.is_empty() {
                    if !is_same_provider {
                        println!("  \x1b[31mNo key provided, provider not changed.\x1b[0m");
                        return;
                    }
                } else {
                    koda_core::runtime_env::set(env_name, &key);
                    let masked = koda_core::keystore::mask_key(&key);
                    println!(
                        "  \x1b[32m\u{2713}\x1b[0m {} set to \x1b[90m{masked}\x1b[0m",
                        env_name
                    );
                    if let Ok(mut store) = koda_core::keystore::KeyStore::load() {
                        store.set(env_name, &key);
                        if let Err(e) = store.save() {
                            println!("  \x1b[33m\u{26a0} Could not persist key: {e}\x1b[0m");
                        } else if let Ok(path) = koda_core::keystore::KeyStore::keys_path() {
                            println!(
                                "  \x1b[32m\u{2713}\x1b[0m Saved to \x1b[90m{}\x1b[0m",
                                path.display()
                            );
                        }
                    }
                }
            }
            Err(_) => {
                println!("  \x1b[31mProvider switch cancelled.\x1b[0m");
                return;
            }
        }
    } else if !ptype.requires_api_key() {
        let default_url = ptype.default_base_url();
        let prompt_msg = format!("  Enter {} URL (enter for {}): ", ptype, default_url);
        match read_line_raw(&prompt_msg) {
            Ok(url) => {
                if !url.is_empty() {
                    config.base_url = url;
                } else {
                    config.base_url = default_url.to_string();
                }
                println!(
                    "  \x1b[32m\u{2713}\x1b[0m URL set to \x1b[36m{}\x1b[0m",
                    config.base_url
                );
            }
            Err(_) => {
                println!("  \x1b[31mProvider switch cancelled.\x1b[0m");
                return;
            }
        }
    }

    *provider.write().await = create_provider(config);
    println!(
        "  \x1b[32m\u{2713}\x1b[0m Provider: \x1b[36m{}\x1b[0m",
        config.provider_type
    );

    // Persist for next startup
    let mut s = koda_core::approval::Settings::load();
    let _ = s.save_last_provider(
        &config.provider_type.to_string(),
        &config.base_url,
        &config.model,
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

#[allow(dead_code)] // Used by onboarding (pre-raw-mode)
pub(crate) async fn handle_pick_provider(
    config: &mut KodaConfig,
    provider: &Arc<RwLock<Box<dyn LlmProvider>>>,
) {
    let providers = crate::repl::PROVIDERS;
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

    let selection =
        match crate::select_menu::select("\u{1f43b} Select a provider", &options, current_idx) {
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

    handle_setup_provider(config, provider, ptype, base_url).await;
}

// ── Trust mode picker ───────────────────────────────────────

/// Interactive trust mode picker (arrow-key menu).
#[allow(dead_code)] // Used by onboarding (pre-raw-mode)
pub(crate) fn pick_trust_mode(current: ApprovalMode) -> Option<ApprovalMode> {
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
    match crate::select_menu::select("\u{1f43b} Trust level", &options, initial) {
        Ok(Some(idx)) => Some(modes[idx]),
        _ => None,
    }
}

// ── Provider factory ───────────────────────────────────────

/// Create an LLM provider from the config.
pub(crate) fn create_provider(config: &KodaConfig) -> Box<dyn LlmProvider> {
    koda_core::providers::create_provider(config)
}
