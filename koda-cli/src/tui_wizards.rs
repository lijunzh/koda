//! Native TUI wizard handlers for /provider, /compact, /mcp, /trust.
//!
//! Extracted from tui_commands.rs to keep files under 600 lines.
//! All output rendered through tui_output::write_line(&).

use crate::select_menu::{self, SelectOption};
use crate::tui_output;

use koda_core::approval::ApprovalMode;
use koda_core::config::KodaConfig;
use koda_core::providers::LlmProvider;
use koda_core::session::KodaSession;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    text::{Line, Span},
};
use std::sync::Arc;
use tokio::sync::RwLock;

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

use tui_output::{dim_msg, err_msg, ok_msg, warn_msg};

// Re-export style constants from tui_output for inline use
use tui_output::{BOLD, CYAN, DIM, GREEN as OK};

// ── Provider (native TUI) ───────────────────────────────────

#[allow(unused_variables)]
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
            dim_msg("Cancelled.".into());
            return;
        }
        Err(e) => {
            err_msg(format!("TUI error: {e}"));
            return;
        }
    };

    let (key, _, _) = providers[idx];
    let ptype = koda_core::config::ProviderType::from_url_or_name("", Some(key));
    let base_url = ptype.default_base_url().to_string();
    handle_setup_provider(terminal, config, provider, ptype, base_url).await;
}

#[allow(unused_variables)]
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
            warn_msg(format!("{env_name} is not set."));
            format!("Paste your {} API key: ", config.provider_type)
        };

        let key = crate::widgets::text_input::read_line(terminal, &prompt, true);
        if key.is_empty() {
            if !is_same_provider {
                err_msg("No key provided, provider not changed.".into());
                return;
            }
        } else {
            koda_core::runtime_env::set(env_name, &key);
            let masked = koda_core::keystore::mask_key(&key);
            ok_msg(format!("{env_name} set to {masked}"));
            if let Ok(mut store) = koda_core::keystore::KeyStore::load() {
                store.set(env_name, &key);
                if let Err(e) = store.save() {
                    warn_msg(format!("Could not persist key: {e}"));
                } else if let Ok(path) = koda_core::keystore::KeyStore::keys_path() {
                    ok_msg(format!("Saved to {}", path.display()));
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
        ok_msg(format!("URL set to {}", config.base_url));
    }

    *provider.write().await = crate::commands::create_provider(config);
    ok_msg(format!("Provider: {}", config.provider_type));
    save_provider(config);

    // Verify connection
    let prov = provider.read().await;
    match prov.list_models().await {
        Ok(models) => {
            if let Some(first) = models.first() {
                config.model = first.id.clone();
                config.model_settings.model = config.model.clone();
            }
            ok_msg("Connection verified! Available models:".into());
            for m in &models {
                let marker = if m.id == config.model {
                    " \u{25c0} selected"
                } else {
                    ""
                };
                tui_output::write_line(&Line::from(vec![
                    Span::raw(format!("      {}", m.id)),
                    Span::styled(marker, OK),
                ]));
            }
        }
        Err(e) => {
            warn_msg(format!("Could not verify connection: {e}"));
            dim_msg(format!("Model set to: {} (unverified)", config.model));
        }
    }
}

// ── Compact (native TUI) ────────────────────────────────────

#[allow(unused_variables)]
pub(crate) async fn handle_compact(
    _terminal: &mut Term,
    session: &KodaSession,
    config: &KodaConfig,
    provider: &Arc<RwLock<Box<dyn LlmProvider>>>,
) {
    use koda_core::compact::{self, CompactSkip};

    tui_output::write_line(&Line::styled("  \u{1f43b} Compacting...", CYAN));

    match compact::compact_session(
        &session.db,
        &session.id,
        config.max_context_tokens,
        &config.model_settings,
        provider,
    )
    .await
    {
        Ok(Ok(result)) => {
            ok_msg(format!(
                "Compacted {} messages \u{2192} ~{} tokens",
                result.deleted, result.summary_tokens
            ));
            dim_msg("Conversation context has been summarized. Continue as normal!".into());
        }
        Ok(Err(CompactSkip::PendingToolCalls)) => {
            warn_msg("Tool calls are still pending \u{2014} deferring compact.".into());
        }
        Ok(Err(CompactSkip::TooShort(n))) => {
            dim_msg(format!(
                "Conversation is too short to compact ({n} messages)."
            ));
        }
        Err(e) => err_msg(format!("Compact failed: {e:#}")),
    }
}

// ── MCP (native TUI) ───────────────────────────────────────

#[allow(unused_variables)]
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
            tui_output::write_blank();
            if servers.is_empty() {
                dim_msg("No MCP servers connected.".into());
                dim_msg("Add servers via .mcp.json or /mcp add <name> <command> [args...]".into());
            } else {
                tui_output::write_line(&Line::styled("  \u{1f50c} MCP Servers", BOLD));
                tui_output::write_blank();
                for server in &servers {
                    let cmd = if server.args.is_empty() {
                        server.command.clone()
                    } else {
                        format!("{} {}", server.command, server.args.join(" "))
                    };
                    tui_output::write_line(&Line::from(vec![
                        Span::styled("  \u{25cf} ", OK),
                        Span::styled(&server.name, BOLD),
                        Span::raw(format!(" \u{2014} {} tool(s)", server.tool_count)),
                    ]));
                    dim_msg(format!("    {cmd}"));
                    for tool_name in &server.tool_names {
                        tui_output::write_line(&Line::from(vec![
                            Span::styled("    \u{2022} ", CYAN),
                            Span::raw(tool_name.clone()),
                        ]));
                    }
                }
            }
            tui_output::write_blank();
        }

        "add" => {
            let rest = args.strip_prefix("add").unwrap_or("").trim();
            let add_parts: Vec<&str> = rest.splitn(2, ' ').collect();
            if add_parts.len() < 2 {
                warn_msg("Usage: /mcp add <name> <command> [args...]".into());
                dim_msg(
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
                err_msg(format!("Failed to save config: {e}"));
                return;
            }

            tui_output::write_line(&Line::styled(
                format!("  \u{1f50c} Connecting to '{name}'..."),
                CYAN,
            ));
            let mut registry = mcp_registry.write().await;
            match registry.add_server(name.clone(), config).await {
                Ok(()) => {
                    let tool_count = registry
                        .server_info()
                        .iter()
                        .find(|s| s.name == name)
                        .map(|s| s.tool_count)
                        .unwrap_or(0);
                    ok_msg(format!(
                        "Added '{name}' ({tool_count} tools). Saved to .mcp.json"
                    ));
                }
                Err(e) => err_msg(format!("Failed to connect: {e}")),
            }
        }

        "remove" => {
            let name = args.strip_prefix("remove").unwrap_or("").trim();
            if name.is_empty() {
                warn_msg("Usage: /mcp remove <name>".into());
                return;
            }
            let mut registry = mcp_registry.write().await;
            if registry.remove_server(name) {
                let _ = koda_core::mcp::config::remove_server_from_project(project_root, name);
                ok_msg(format!("Removed MCP server '{name}'"));
            } else {
                err_msg(format!("MCP server '{name}' not found"));
            }
        }

        "restart" => {
            let name = args.strip_prefix("restart").unwrap_or("").trim();
            let mut registry = mcp_registry.write().await;
            if name.is_empty() {
                tui_output::write_line(&Line::styled(
                    "  \u{1f50c} Restarting all MCP servers...",
                    CYAN,
                ));
                registry.restart_all(project_root).await;
                ok_msg("Done".into());
            } else {
                tui_output::write_line(&Line::styled(
                    format!("  \u{1f50c} Restarting '{name}'..."),
                    CYAN,
                ));
                match registry.restart_server(name, project_root).await {
                    Ok(()) => ok_msg(format!("Restarted '{name}'")),
                    Err(e) => err_msg(format!("Failed: {e}")),
                }
            }
        }

        other => {
            warn_msg(format!("Unknown MCP command: {other}"));
            dim_msg("Usage: /mcp [status|add|remove|restart]".into());
        }
    }
}

// ── Agents (native TUI) ──────────────────────────────────

#[allow(unused_variables)]
pub(crate) fn handle_list_agents(terminal: &mut Term, project_root: &std::path::Path) {
    let agents = koda_core::tools::agent::list_agents(project_root);
    tui_output::write_blank();
    tui_output::write_line(&Line::styled("  \u{1f43b} Sub-Agents", BOLD));
    tui_output::write_blank();

    if agents.is_empty() {
        dim_msg("No sub-agents configured.".into());
    } else {
        for (name, desc, source) in &agents {
            let tag = match source.as_str() {
                "user" => " [user]",
                "project" => " [project]",
                _ => "",
            };
            tui_output::write_line(&Line::from(vec![
                Span::styled(format!("  {name}"), CYAN),
                Span::raw(format!(" \u{2014} {desc}")),
                Span::styled(tag, DIM),
            ]));
        }
    }

    tui_output::write_blank();
    dim_msg("Ask Koda to invoke them, or use koda --agent <name>".into());
    dim_msg("Need a specialist? Ask Koda to create one for recurring tasks".into());
}

// ── Diff (native TUI) ────────────────────────────────────

#[allow(unused_variables)]
pub(crate) fn handle_diff(terminal: &mut Term) {
    let output = std::process::Command::new("git")
        .args(["diff", "--stat"])
        .output();

    let diff_stat = match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            err_msg(format!("Git error: {err}"));
            return;
        }
        Err(e) => {
            err_msg(format!("Failed to run git: {e}"));
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
        dim_msg("No uncommitted changes.".into());
        return;
    };

    tui_output::write_blank();
    tui_output::write_line(&Line::styled("  \u{1f43b} Uncommitted Changes", BOLD));
    tui_output::write_blank();
    for line in stat.lines() {
        dim_msg(line.to_string());
    }
    tui_output::write_blank();
    dim_msg("/diff review   \u{2014} ask Koda to review the changes".into());
    dim_msg("/diff commit   \u{2014} generate a commit message".into());
}

// ── Memory (native TUI) ───────────────────────────────────

#[allow(unused_variables)]
pub(crate) fn handle_memory(
    terminal: &mut Term,
    arg: Option<&str>,
    project_root: &std::path::Path,
) {
    match arg {
        Some(text) if text.starts_with("global ") => {
            let entry = text.strip_prefix("global ").unwrap().trim();
            if entry.is_empty() {
                warn_msg("Usage: /memory global <text>".into());
            } else {
                match koda_core::memory::append_global(entry) {
                    Ok(()) => ok_msg("Saved to global memory".into()),
                    Err(e) => err_msg(format!("Error: {e}")),
                }
            }
        }
        Some(text) if text.starts_with("add ") => {
            let entry = text.strip_prefix("add ").unwrap().trim();
            if entry.is_empty() {
                warn_msg("Usage: /memory add <text>".into());
            } else {
                match koda_core::memory::append(project_root, entry) {
                    Ok(()) => ok_msg("Saved to project memory (MEMORY.md)".into()),
                    Err(e) => err_msg(format!("Error: {e}")),
                }
            }
        }
        _ => {
            let active = koda_core::memory::active_project_file(project_root);
            tui_output::write_blank();
            tui_output::write_line(&Line::styled("  \u{1f43b} Memory", BOLD));
            tui_output::write_blank();
            match active {
                Some(f) => tui_output::write_line(&Line::from(vec![
                    Span::raw("  Project: "),
                    Span::styled(f, CYAN),
                ])),
                None => {
                    dim_msg("Project: (none \u{2014} will create MEMORY.md on first write)".into())
                }
            }
            tui_output::write_line(&Line::from(vec![
                Span::raw("  Global:  "),
                Span::styled("~/.config/koda/memory.md", CYAN),
            ]));
            tui_output::write_blank();
            dim_msg("Commands:".into());
            dim_msg("  /memory add <text>      Save to project MEMORY.md".into());
            dim_msg("  /memory global <text>   Save to global memory".into());
            tui_output::write_blank();
            dim_msg(
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
