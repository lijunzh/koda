//! Native TUI wizard handlers for /provider, /compact, /mcp, /trust.
//!
//! Extracted from tui_commands.rs to keep files under 600 lines.
//! All output rendered through tui_output::emit_line().

use crate::select_menu::{self, SelectOption};
use crate::tui_output;

use koda_core::approval::ApprovalMode;
use koda_core::config::KodaConfig;
use koda_core::providers::LlmProvider;
use koda_core::session::KodaSession;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use std::sync::Arc;
use tokio::sync::RwLock;

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

const DIM: Style = Style::new().fg(Color::DarkGray);
const ERR: Style = Style::new().fg(Color::Red);
const OK: Style = Style::new().fg(Color::Green);
const CYAN: Style = Style::new().fg(Color::Cyan);
const WARN: Style = Style::new().fg(Color::Yellow);
const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);

fn ok_msg(terminal: &mut Term, msg: String) {
    tui_output::emit_line(
        terminal,
        Line::from(vec![Span::styled("  \u{2713} ", OK), Span::raw(msg)]),
    );
}
fn err_msg(terminal: &mut Term, msg: String) {
    tui_output::emit_line(
        terminal,
        Line::from(vec![
            Span::styled("  \u{2717} ", ERR),
            Span::styled(msg, ERR),
        ]),
    );
}
fn dim_msg(terminal: &mut Term, msg: String) {
    tui_output::emit_line(terminal, Line::styled(format!("  {msg}"), DIM));
}
fn warn_msg(terminal: &mut Term, msg: String) {
    tui_output::emit_line(
        terminal,
        Line::from(vec![
            Span::styled("  \u{26a0} ", WARN),
            Span::styled(msg, WARN),
        ]),
    );
}

// ── Provider (native TUI) ───────────────────────────────────

pub(crate) async fn handle_pick_provider(
    terminal: &mut Term,
    config: &mut KodaConfig,
    provider: &Arc<RwLock<Box<dyn LlmProvider>>>,
) {
    let providers = crate::repl::PROVIDERS;
    let current_idx = providers
        .iter()
        .position(|(key, _, _)| {
            koda_core::config::ProviderType::from_url_or_name("", Some(key)) == config.provider_type
        })
        .unwrap_or(0);
    let options: Vec<SelectOption> = providers
        .iter()
        .map(|(_, name, url)| SelectOption::new(*name, *url))
        .collect();

    let idx = match select_menu::select_inline(
        terminal,
        "\u{1f43b} Select a provider",
        &options,
        current_idx,
    ) {
        Ok(Some(idx)) => idx,
        Ok(None) => {
            dim_msg(terminal, "Cancelled.".into());
            return;
        }
        Err(e) => {
            err_msg(terminal, format!("TUI error: {e}"));
            return;
        }
    };

    let (key, _, _) = providers[idx];
    let ptype = koda_core::config::ProviderType::from_url_or_name("", Some(key));
    let base_url = ptype.default_base_url().to_string();
    handle_setup_provider(terminal, config, provider, ptype, base_url).await;
}

pub(crate) async fn handle_setup_provider(
    terminal: &mut Term,
    config: &mut KodaConfig,
    provider: &Arc<RwLock<Box<dyn LlmProvider>>>,
    ptype: koda_core::config::ProviderType,
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
        let prompt = if is_same_provider {
            format!(
                "Update {} API key (enter to keep current): ",
                config.provider_type
            )
        } else {
            warn_msg(terminal, format!("{env_name} is not set."));
            format!("Paste your {} API key: ", config.provider_type)
        };

        let key = crate::widgets::text_input::read_line(terminal, &prompt, true);
        if key.is_empty() {
            if !is_same_provider {
                err_msg(terminal, "No key provided, provider not changed.".into());
                return;
            }
        } else {
            koda_core::runtime_env::set(env_name, &key);
            let masked = koda_core::keystore::mask_key(&key);
            ok_msg(terminal, format!("{env_name} set to {masked}"));
            if let Ok(mut store) = koda_core::keystore::KeyStore::load() {
                store.set(env_name, &key);
                if let Err(e) = store.save() {
                    warn_msg(terminal, format!("Could not persist key: {e}"));
                } else if let Ok(path) = koda_core::keystore::KeyStore::keys_path() {
                    ok_msg(terminal, format!("Saved to {}", path.display()));
                }
            }
        }
    } else if !ptype.requires_api_key() {
        let default_url = ptype.default_base_url();
        let prompt = format!("Enter {} URL (enter for {}): ", ptype, default_url);
        let url = crate::widgets::text_input::read_line(terminal, &prompt, false);
        if !url.is_empty() {
            config.base_url = url;
        } else {
            config.base_url = default_url.to_string();
        }
        ok_msg(terminal, format!("URL set to {}", config.base_url));
    }

    *provider.write().await = crate::commands::create_provider(config);
    ok_msg(terminal, format!("Provider: {}", config.provider_type));
    save_provider(config);

    // Verify connection
    let prov = provider.read().await;
    match prov.list_models().await {
        Ok(models) => {
            if let Some(first) = models.first() {
                config.model = first.id.clone();
                config.model_settings.model = config.model.clone();
            }
            ok_msg(terminal, "Connection verified! Available models:".into());
            for m in &models {
                let marker = if m.id == config.model {
                    " \u{25c0} selected"
                } else {
                    ""
                };
                tui_output::emit_line(
                    terminal,
                    Line::from(vec![
                        Span::raw(format!("      {}", m.id)),
                        Span::styled(marker, OK),
                    ]),
                );
            }
        }
        Err(e) => {
            warn_msg(terminal, format!("Could not verify connection: {e}"));
            dim_msg(
                terminal,
                format!("Model set to: {} (unverified)", config.model),
            );
        }
    }
}

// ── Compact (native TUI) ────────────────────────────────────

const COMPACT_PRESERVE_COUNT: usize = 4;

pub(crate) async fn handle_compact(
    terminal: &mut Term,
    session: &KodaSession,
    config: &KodaConfig,
    provider: &Arc<RwLock<Box<dyn LlmProvider>>>,
) {
    use koda_core::providers::ChatMessage;

    if let Ok(true) = session.db.has_pending_tool_calls(&session.id).await {
        warn_msg(
            terminal,
            "Tool calls are still pending — deferring compact.".into(),
        );
        return;
    }

    let history = match session
        .db
        .load_context(&session.id, config.max_context_tokens)
        .await
    {
        Ok(msgs) => msgs,
        Err(e) => {
            err_msg(terminal, format!("Error loading conversation: {e}"));
            return;
        }
    };

    if history.len() < 4 {
        dim_msg(
            terminal,
            format!(
                "Conversation is too short to compact ({} messages).",
                history.len()
            ),
        );
        return;
    }

    tui_output::emit_line(
        terminal,
        Line::styled(
            format!(
                "  \u{1f43b} Compacting {} messages (preserving last {})...",
                history.len(),
                COMPACT_PRESERVE_COUNT
            ),
            CYAN,
        ),
    );

    // Build conversation text for summarization
    let mut conversation_text = String::new();
    for msg in &history {
        let role = msg.role.as_str();
        if let Some(ref content) = msg.content {
            let truncated: String = content.chars().take(2000).collect();
            conversation_text.push_str(&format!("[{role}]: {truncated}\n\n"));
        }
        if let Some(ref tool_calls) = msg.tool_calls {
            let truncated: String = tool_calls.chars().take(500).collect();
            conversation_text.push_str(&format!("[{role} tool_calls]: {truncated}\n\n"));
        }
    }
    if conversation_text.len() > 20_000 {
        let mut end = 20_000;
        while end > 0 && !conversation_text.is_char_boundary(end) {
            end -= 1;
        }
        conversation_text.truncate(end);
        conversation_text.push_str("\n\n[...truncated for summarization...]");
    }

    let summary_prompt = format!(
        "Summarize the conversation below. This summary will replace the older messages \
         so an AI assistant can continue the session seamlessly.\n\
         \n\
         Preserve ALL of the following:\n\
         1. **User Intent** — Every goal, request, and requirement.\n\
         2. **Key Decisions** — Decisions made and their rationale.\n\
         3. **Files & Code** — Every file created, modified, or deleted.\n\
         4. **Errors & Fixes** — Bugs encountered and how they were resolved.\n\
         5. **Current State** — What is working, what has been tested.\n\
         6. **Pending Tasks** — Anything unfinished or deferred.\n\
         7. **Next Step** — Only if clearly stated or implied.\n\
         \n\
         Use concise bullet points. Do not add new ideas.\n\
         \n\
         ---\n\n{conversation_text}"
    );

    let messages = vec![ChatMessage::text("user", &summary_prompt)];
    let prov = provider.read().await;
    let response = match prov.chat(&messages, &[], &config.model_settings).await {
        Ok(r) => r,
        Err(e) => {
            err_msg(terminal, format!("Failed to generate summary: {e}"));
            return;
        }
    };

    let summary = match response.content {
        Some(text) if !text.trim().is_empty() => text,
        _ => {
            err_msg(
                terminal,
                "LLM returned an empty summary. Aborting compact.".into(),
            );
            return;
        }
    };

    let compact_message = format!("[Compacted conversation summary]\n\n{summary}");

    match session
        .db
        .compact_session(&session.id, &compact_message, COMPACT_PRESERVE_COUNT)
        .await
    {
        Ok(deleted) => {
            let summary_tokens = summary.len() / 4;
            ok_msg(
                terminal,
                format!("Compacted {deleted} messages → ~{summary_tokens} tokens"),
            );
            dim_msg(
                terminal,
                "Conversation context has been summarized. Continue as normal!".into(),
            );
        }
        Err(e) => err_msg(terminal, format!("Failed to compact session: {e}")),
    }
}

// ── MCP (native TUI) ───────────────────────────────────────

pub(crate) async fn handle_mcp(
    terminal: &mut Term,
    args: &str,
    mcp_registry: &Arc<RwLock<koda_core::mcp::McpRegistry>>,
    project_root: &std::path::Path,
) {
    let parts: Vec<&str> = args.splitn(3, ' ').collect();
    let sub = parts.first().map(|s| s.trim()).unwrap_or("");

    match sub {
        "" | "status" => {
            let registry = mcp_registry.read().await;
            let servers = registry.server_info();
            tui_output::emit_blank(terminal);
            if servers.is_empty() {
                dim_msg(terminal, "No MCP servers connected.".into());
                dim_msg(
                    terminal,
                    "Add servers via .mcp.json or /mcp add <name> <command> [args...]".into(),
                );
            } else {
                tui_output::emit_line(terminal, Line::styled("  \u{1f50c} MCP Servers", BOLD));
                tui_output::emit_blank(terminal);
                for server in &servers {
                    let cmd = if server.args.is_empty() {
                        server.command.clone()
                    } else {
                        format!("{} {}", server.command, server.args.join(" "))
                    };
                    tui_output::emit_line(
                        terminal,
                        Line::from(vec![
                            Span::styled("  \u{25cf} ", OK),
                            Span::styled(&server.name, BOLD),
                            Span::raw(format!(" \u{2014} {} tool(s)", server.tool_count)),
                        ]),
                    );
                    dim_msg(terminal, format!("    {cmd}"));
                    for tool_name in &server.tool_names {
                        tui_output::emit_line(
                            terminal,
                            Line::from(vec![
                                Span::styled("    \u{2022} ", CYAN),
                                Span::raw(tool_name.clone()),
                            ]),
                        );
                    }
                }
            }
            tui_output::emit_blank(terminal);
        }

        "add" => {
            let rest = args.strip_prefix("add").unwrap_or("").trim();
            let add_parts: Vec<&str> = rest.splitn(2, ' ').collect();
            if add_parts.len() < 2 {
                warn_msg(
                    terminal,
                    "Usage: /mcp add <name> <command> [args...]".into(),
                );
                dim_msg(
                    terminal,
                    "Example: /mcp add filesystem npx -y @modelcontextprotocol/server-filesystem /tmp".into(),
                );
                return;
            }
            let name = add_parts[0].to_string();
            let cmd_parts: Vec<&str> = add_parts[1].split_whitespace().collect();
            let command = cmd_parts[0].to_string();
            let cmd_args: Vec<String> = cmd_parts[1..].iter().map(|s| s.to_string()).collect();

            let config = koda_core::mcp::config::McpServerConfig {
                command,
                args: cmd_args,
                env: std::collections::HashMap::new(),
                timeout: None,
            };

            if let Err(e) =
                koda_core::mcp::config::save_server_to_project(project_root, &name, &config)
            {
                err_msg(terminal, format!("Failed to save config: {e}"));
                return;
            }

            tui_output::emit_line(
                terminal,
                Line::styled(format!("  \u{1f50c} Connecting to '{name}'..."), CYAN),
            );
            let mut registry = mcp_registry.write().await;
            match registry.add_server(name.clone(), config).await {
                Ok(()) => {
                    let tool_count = registry
                        .server_info()
                        .iter()
                        .find(|s| s.name == name)
                        .map(|s| s.tool_count)
                        .unwrap_or(0);
                    ok_msg(
                        terminal,
                        format!("Added '{name}' ({tool_count} tools). Saved to .mcp.json"),
                    );
                }
                Err(e) => err_msg(terminal, format!("Failed to connect: {e}")),
            }
        }

        "remove" => {
            let name = args.strip_prefix("remove").unwrap_or("").trim();
            if name.is_empty() {
                warn_msg(terminal, "Usage: /mcp remove <name>".into());
                return;
            }
            let mut registry = mcp_registry.write().await;
            if registry.remove_server(name) {
                let _ = koda_core::mcp::config::remove_server_from_project(project_root, name);
                ok_msg(terminal, format!("Removed MCP server '{name}'"));
            } else {
                err_msg(terminal, format!("MCP server '{name}' not found"));
            }
        }

        "restart" => {
            let name = args.strip_prefix("restart").unwrap_or("").trim();
            let mut registry = mcp_registry.write().await;
            if name.is_empty() {
                tui_output::emit_line(
                    terminal,
                    Line::styled("  \u{1f50c} Restarting all MCP servers...", CYAN),
                );
                registry.restart_all(project_root).await;
                ok_msg(terminal, "Done".into());
            } else {
                tui_output::emit_line(
                    terminal,
                    Line::styled(format!("  \u{1f50c} Restarting '{name}'..."), CYAN),
                );
                match registry.restart_server(name, project_root).await {
                    Ok(()) => ok_msg(terminal, format!("Restarted '{name}'")),
                    Err(e) => err_msg(terminal, format!("Failed: {e}")),
                }
            }
        }

        other => {
            warn_msg(terminal, format!("Unknown MCP command: {other}"));
            dim_msg(terminal, "Usage: /mcp [status|add|remove|restart]".into());
        }
    }
}

// ── Agents (native TUI) ──────────────────────────────────

pub(crate) fn handle_list_agents(terminal: &mut Term, project_root: &std::path::Path) {
    let agents = koda_core::tools::agent::list_agents(project_root);
    tui_output::emit_blank(terminal);
    tui_output::emit_line(terminal, Line::styled("  \u{1f43b} Sub-Agents", BOLD));
    tui_output::emit_blank(terminal);

    if agents.is_empty() {
        dim_msg(terminal, "No sub-agents configured.".into());
    } else {
        for (name, desc, source) in &agents {
            let tag = match source.as_str() {
                "user" => " [user]",
                "project" => " [project]",
                _ => "",
            };
            tui_output::emit_line(
                terminal,
                Line::from(vec![
                    Span::styled(format!("  {name}"), CYAN),
                    Span::raw(format!(" \u{2014} {desc}")),
                    Span::styled(tag, DIM),
                ]),
            );
        }
    }

    tui_output::emit_blank(terminal);
    dim_msg(
        terminal,
        "Ask Koda to invoke them, or use koda --agent <name>".into(),
    );
    dim_msg(
        terminal,
        "Need a specialist? Ask Koda to create one for recurring tasks".into(),
    );
}

// ── Diff (native TUI) ────────────────────────────────────

pub(crate) fn handle_diff(terminal: &mut Term) {
    let output = std::process::Command::new("git")
        .args(["diff", "--stat"])
        .output();

    let diff_stat = match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            err_msg(terminal, format!("Git error: {err}"));
            return;
        }
        Err(e) => {
            err_msg(terminal, format!("Failed to run git: {e}"));
            return;
        }
    };

    // Check unstaged + staged
    let has_unstaged = !diff_stat.trim().is_empty();
    let staged_stat = if !has_unstaged {
        std::process::Command::new("git")
            .args(["diff", "--cached", "--stat"])
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    let s = String::from_utf8_lossy(&o.stdout).to_string();
                    if s.trim().is_empty() { None } else { Some(s) }
                } else {
                    None
                }
            })
    } else {
        None
    };

    let stat = if has_unstaged {
        diff_stat
    } else if let Some(s) = staged_stat {
        s
    } else {
        dim_msg(terminal, "No uncommitted changes.".into());
        return;
    };

    tui_output::emit_blank(terminal);
    tui_output::emit_line(
        terminal,
        Line::styled("  \u{1f43b} Uncommitted Changes", BOLD),
    );
    tui_output::emit_blank(terminal);
    for line in stat.lines() {
        dim_msg(terminal, line.to_string());
    }
    tui_output::emit_blank(terminal);
    dim_msg(
        terminal,
        "/diff review   \u{2014} ask Koda to review the changes".into(),
    );
    dim_msg(
        terminal,
        "/diff commit   \u{2014} generate a commit message".into(),
    );
}

// ── Memory (native TUI) ───────────────────────────────────

pub(crate) fn handle_memory(
    terminal: &mut Term,
    arg: Option<&str>,
    project_root: &std::path::Path,
) {
    match arg {
        Some(text) if text.starts_with("global ") => {
            let entry = text.strip_prefix("global ").unwrap().trim();
            if entry.is_empty() {
                warn_msg(terminal, "Usage: /memory global <text>".into());
            } else {
                match koda_core::memory::append_global(entry) {
                    Ok(()) => ok_msg(terminal, "Saved to global memory".into()),
                    Err(e) => err_msg(terminal, format!("Error: {e}")),
                }
            }
        }
        Some(text) if text.starts_with("add ") => {
            let entry = text.strip_prefix("add ").unwrap().trim();
            if entry.is_empty() {
                warn_msg(terminal, "Usage: /memory add <text>".into());
            } else {
                match koda_core::memory::append(project_root, entry) {
                    Ok(()) => ok_msg(terminal, "Saved to project memory (MEMORY.md)".into()),
                    Err(e) => err_msg(terminal, format!("Error: {e}")),
                }
            }
        }
        _ => {
            let active = koda_core::memory::active_project_file(project_root);
            tui_output::emit_blank(terminal);
            tui_output::emit_line(terminal, Line::styled("  \u{1f43b} Memory", BOLD));
            tui_output::emit_blank(terminal);
            match active {
                Some(f) => tui_output::emit_line(
                    terminal,
                    Line::from(vec![Span::raw("  Project: "), Span::styled(f, CYAN)]),
                ),
                None => dim_msg(
                    terminal,
                    "Project: (none \u{2014} will create MEMORY.md on first write)".into(),
                ),
            }
            tui_output::emit_line(
                terminal,
                Line::from(vec![
                    Span::raw("  Global:  "),
                    Span::styled("~/.config/koda/memory.md", CYAN),
                ]),
            );
            tui_output::emit_blank(terminal);
            dim_msg(terminal, "Commands:".into());
            dim_msg(
                terminal,
                "  /memory add <text>      Save to project MEMORY.md".into(),
            );
            dim_msg(
                terminal,
                "  /memory global <text>   Save to global memory".into(),
            );
            tui_output::emit_blank(terminal);
            dim_msg(
                terminal,
                "Tip: the LLM can also call MemoryWrite to save insights automatically.".into(),
            );
        }
    }
}

// ── Trust picker (native TUI) ───────────────────────────────

pub(crate) fn pick_trust_inline(
    terminal: &mut Term,
    current: ApprovalMode,
) -> Option<ApprovalMode> {
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
    match select_menu::select_inline(terminal, "\u{1f43b} Trust level", &options, initial) {
        Ok(Some(idx)) => Some(modes[idx]),
        _ => None,
    }
}

pub(crate) fn save_provider(config: &KodaConfig) {
    let mut s = koda_core::approval::Settings::load();
    let _ = s.save_last_provider(
        &config.provider_type.to_string(),
        &config.base_url,
        &config.model,
    );
}
