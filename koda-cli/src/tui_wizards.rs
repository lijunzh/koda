//! Native TUI wizard handlers — multi-step interactive flows.
//!
//! ## Wizards
//!
//! - **`/compact`** — summarize old context, show token savings
//! - **`/provider`** — provider selection menu (Anthropic, Gemini, etc.)
//! - **`/trust`** — set approval mode for the session
//! - **`/key`** — API key entry (masked input)
//!
//! All output flows through the [`crate::scroll_buffer::ScrollBuffer`] render cache.

use crate::scroll_buffer::ScrollBuffer;
use crate::tui_output;

use crate::tui_types::MenuContent;

use koda_core::config::KodaConfig;
use koda_core::providers::LlmProvider;
use koda_core::session::KodaSession;
use ratatui::text::{Line, Span};
use std::sync::Arc;
use tokio::sync::RwLock;

use tui_output::{BOLD, CYAN, DIM};

// ── Compact (native TUI) ────────────────────────────

pub(crate) async fn handle_compact(
    buffer: &mut ScrollBuffer,
    session: &KodaSession,
    config: &KodaConfig,
    provider: &Arc<RwLock<Box<dyn LlmProvider>>>,
) {
    use koda_core::compact::{self, CompactSkip};

    tui_output::emit_line(buffer, Line::styled("  \u{1f43b} Compacting...", CYAN));

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
            tui_output::ok_msg(
                buffer,
                format!(
                    "Compacted {} messages \u{2192} ~{} tokens",
                    result.deleted, result.summary_tokens
                ),
            );
            tui_output::dim_msg(
                buffer,
                "Conversation context has been summarized. Continue as normal!".into(),
            );
        }
        Ok(Err(CompactSkip::PendingToolCalls)) => {
            tui_output::warn_msg(
                buffer,
                "Tool calls are still pending \u{2014} deferring compact.".into(),
            );
        }
        Ok(Err(CompactSkip::TooShort(n))) => {
            tui_output::dim_msg(
                buffer,
                format!("Conversation is too short to compact ({n} messages)."),
            );
        }
        Ok(Err(CompactSkip::HistoryTooLarge)) => {
            tui_output::warn_msg(
                buffer,
                "History is too large for this model to summarize without data loss.".into(),
            );
            tui_output::dim_msg(
                buffer,
                "Switch to a model with a larger context window, or start a new session.".into(),
            );
        }
        Err(e) => tui_output::err_msg(buffer, format!("Compact failed: {e:#}")),
    }
}

// ── Purge (native TUI) ───────────────────────────────

pub(crate) async fn handle_purge(
    buffer: &mut ScrollBuffer,
    session: &KodaSession,
    age_filter: Option<&str>,
    menu: &mut MenuContent,
) {
    use koda_core::persistence::Persistence;

    let min_age_days: u32 = match age_filter {
        Some(s) => {
            let s = s.trim().trim_end_matches('d');
            match s.parse() {
                Ok(d) => d,
                Err(_) => {
                    tui_output::err_msg(
                        buffer,
                        format!("Invalid age filter: '{s}'. Use e.g. /purge 90d"),
                    );
                    return;
                }
            }
        }
        None => 0,
    };

    let stats = match session.db.compacted_stats().await {
        Ok(s) => s,
        Err(e) => {
            tui_output::err_msg(buffer, format!("Failed to query stats: {e:#}"));
            return;
        }
    };

    if stats.message_count == 0 {
        tui_output::dim_msg(buffer, "No compacted messages to purge.".into());
        return;
    }

    let size_str = format_bytes(stats.size_bytes);
    let oldest_str = stats.oldest.as_deref().unwrap_or("unknown");
    let age_str = if min_age_days > 0 {
        format!(" older than {min_age_days} days")
    } else {
        String::new()
    };

    let detail = format!(
        "{} compacted messages across {} sessions ({size_str}), oldest from {oldest_str}{age_str}",
        stats.message_count, stats.session_count
    );

    tui_output::emit_line(
        buffer,
        Line::from(vec![
            Span::styled("  \u{1f9f9} ", BOLD),
            Span::styled(detail.clone(), CYAN),
        ]),
    );

    *menu = MenuContent::PurgeConfirm {
        min_age_days,
        detail,
    };
}

/// Execute the purge after user confirms via menu.
pub(crate) async fn execute_purge(
    buffer: &mut ScrollBuffer,
    session: &KodaSession,
    min_age_days: u32,
) {
    use koda_core::persistence::Persistence;

    match session.db.purge_compacted(min_age_days).await {
        Ok(deleted) => tui_output::ok_msg(buffer, format!("Purged {deleted} archived messages.")),
        Err(e) => tui_output::err_msg(buffer, format!("Purge failed: {e:#}")),
    }
}

/// Format byte count as human-readable (KB, MB, GB).
pub(crate) fn format_bytes(bytes: i64) -> String {
    const KB: i64 = 1024;
    const MB: i64 = 1024 * KB;
    const GB: i64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.1}GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1}MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0}KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes}B")
    }
}

// ── Agents (native TUI) ─────────────────────────────

pub(crate) fn handle_list_agents(buffer: &mut ScrollBuffer, project_root: &std::path::Path) {
    let agents = koda_core::tools::agent::list_agents(project_root);
    tui_output::blank(buffer);
    tui_output::emit_line(buffer, Line::styled("  \u{1f43b} Sub-Agents", BOLD));
    tui_output::blank(buffer);

    if agents.is_empty() {
        tui_output::dim_msg(buffer, "No sub-agents configured.".into());
    } else {
        for (name, desc, source) in &agents {
            let tag = match source.as_str() {
                "user" => " [user]",
                "project" => " [project]",
                _ => "",
            };
            tui_output::emit_line(
                buffer,
                Line::from(vec![
                    Span::styled(format!("  {name}"), CYAN),
                    Span::raw(format!(" \u{2014} {desc}")),
                    Span::styled(tag, DIM),
                ]),
            );
        }
    }

    tui_output::blank(buffer);
    tui_output::dim_msg(
        buffer,
        "Ask Koda to invoke them, or use koda --agent <name>".into(),
    );
    tui_output::dim_msg(
        buffer,
        "Need a specialist? Ask Koda to create one for recurring tasks".into(),
    );
}

// ── Skills ─────────────────────────────────────────

pub(crate) fn handle_list_skills(
    buffer: &mut ScrollBuffer,
    query: Option<&str>,
    tools: &koda_core::tools::ToolRegistry,
) {
    let skills = match query {
        Some(q) if !q.is_empty() => tools.search_skills(q),
        _ => tools.list_skills(),
    };

    tui_output::blank(buffer);
    tui_output::emit_line(buffer, Line::styled("  \u{1f4da} Skills", BOLD));
    tui_output::blank(buffer);

    if skills.is_empty() {
        match query {
            Some(q) => tui_output::dim_msg(buffer, format!("No skills matching '{q}'.")),
            None => tui_output::dim_msg(buffer, "No skills available.".into()),
        }
    } else {
        for (name, description, source) in &skills {
            let tag = match source.as_str() {
                "user" => " [user]",
                "project" => " [project]",
                _ => "",
            };
            tui_output::emit_line(
                buffer,
                Line::from(vec![
                    Span::styled(format!("  {name}"), CYAN),
                    Span::raw(format!(" \u{2014} {description}")),
                    Span::styled(tag, DIM),
                ]),
            );
        }
    }

    tui_output::blank(buffer);
    tui_output::dim_msg(
        buffer,
        "Ask Koda to activate a skill, or use ActivateSkill tool directly.".into(),
    );
    tui_output::dim_msg(
        buffer,
        "Create your own: .koda/skills/<name>/SKILL.md or ~/.config/koda/skills/<name>/SKILL.md"
            .into(),
    );
}

// ── Diff (native TUI) ───────────────────────────────

pub(crate) fn handle_diff(buffer: &mut ScrollBuffer) {
    let output = std::process::Command::new("git")
        .args(["diff", "--stat"])
        .output();

    let diff_stat = match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            tui_output::err_msg(buffer, format!("Git error: {err}"));
            return;
        }
        Err(e) => {
            tui_output::err_msg(buffer, format!("Failed to run git: {e}"));
            return;
        }
    };

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
        tui_output::dim_msg(buffer, "No uncommitted changes.".into());
        return;
    };

    tui_output::blank(buffer);
    tui_output::emit_line(
        buffer,
        Line::styled("  \u{1f43b} Uncommitted Changes", BOLD),
    );
    tui_output::blank(buffer);
    for line in stat.lines() {
        tui_output::dim_msg(buffer, line.to_string());
    }
    tui_output::blank(buffer);
    tui_output::dim_msg(
        buffer,
        "/diff review   \u{2014} ask Koda to review the changes".into(),
    );
    tui_output::dim_msg(
        buffer,
        "/diff commit   \u{2014} generate a commit message".into(),
    );
}

// ── Memory (native TUI) ──────────────────────────────

pub(crate) fn handle_memory(
    buffer: &mut ScrollBuffer,
    arg: Option<&str>,
    project_root: &std::path::Path,
) {
    match arg {
        Some(text) if text.starts_with("global ") => {
            let entry = text.strip_prefix("global ").unwrap().trim();
            if entry.is_empty() {
                tui_output::warn_msg(buffer, "Usage: /memory global <text>".into());
            } else {
                match koda_core::memory::append_global(entry) {
                    Ok(()) => tui_output::ok_msg(buffer, "Saved to global memory".into()),
                    Err(e) => tui_output::err_msg(buffer, format!("Error: {e}")),
                }
            }
        }
        Some(text) if text.starts_with("add ") => {
            let entry = text.strip_prefix("add ").unwrap().trim();
            if entry.is_empty() {
                tui_output::warn_msg(buffer, "Usage: /memory add <text>".into());
            } else {
                match koda_core::memory::append(project_root, entry) {
                    Ok(()) => {
                        tui_output::ok_msg(buffer, "Saved to project memory (MEMORY.md)".into())
                    }
                    Err(e) => tui_output::err_msg(buffer, format!("Error: {e}")),
                }
            }
        }
        _ => {
            let active = koda_core::memory::active_project_file(project_root);
            tui_output::blank(buffer);
            tui_output::emit_line(buffer, Line::styled("  \u{1f43b} Memory", BOLD));
            tui_output::blank(buffer);
            match active {
                Some(f) => tui_output::emit_line(
                    buffer,
                    Line::from(vec![Span::raw("  Project: "), Span::styled(f, CYAN)]),
                ),
                None => tui_output::dim_msg(
                    buffer,
                    "Project: (none \u{2014} will create MEMORY.md on first write)".into(),
                ),
            }
            tui_output::emit_line(
                buffer,
                Line::from(vec![
                    Span::raw("  Global:  "),
                    Span::styled("~/.config/koda/memory.md", CYAN),
                ]),
            );
            tui_output::blank(buffer);
            tui_output::dim_msg(buffer, "Commands:".into());
            tui_output::dim_msg(
                buffer,
                "  /memory add <text>      Save to project MEMORY.md".into(),
            );
            tui_output::dim_msg(
                buffer,
                "  /memory global <text>   Save to global memory".into(),
            );
            tui_output::blank(buffer);
            tui_output::dim_msg(
                buffer,
                "Tip: the LLM can also call MemoryWrite to save insights automatically.".into(),
            );
        }
    }
}

pub(crate) async fn save_provider(config: &KodaConfig, db: &koda_core::db::Database) {
    let _ = koda_core::last_provider::save_last_provider(
        db,
        &config.provider_type.to_string(),
        &config.base_url,
        &config.model,
    )
    .await;
}

// ── API key management (/key) ─────────────────────────

/// Key providers to show in /key, with their env var names.
const KEY_PROVIDERS: &[(&str, &str)] = &[
    ("Anthropic", "ANTHROPIC_API_KEY"),
    ("OpenAI", "OPENAI_API_KEY"),
    ("Google Gemini", "GEMINI_API_KEY"),
    ("Groq", "GROQ_API_KEY"),
    ("Grok (xAI)", "XAI_API_KEY"),
    ("DeepSeek", "DEEPSEEK_API_KEY"),
    ("Mistral", "MISTRAL_API_KEY"),
    ("MiniMax", "MINIMAX_API_KEY"),
    ("OpenRouter", "OPENROUTER_API_KEY"),
    ("Together", "TOGETHER_API_KEY"),
    ("Fireworks", "FIREWORKS_API_KEY"),
];

pub(crate) fn handle_keys(buffer: &mut ScrollBuffer) {
    tui_output::emit_line(buffer, Line::styled("  \u{1f511} API Keys", BOLD));
    tui_output::blank(buffer);

    for (name, env_var) in KEY_PROVIDERS {
        let is_set = koda_core::runtime_env::is_set(env_var);
        let status = if is_set {
            Span::styled("\u{2714} set", tui_output::GREEN)
        } else {
            Span::styled("  not set", DIM)
        };
        tui_output::emit_line(
            buffer,
            Line::from(vec![
                Span::raw(format!("  {name:<16}")),
                Span::styled(format!("{env_var:<24}"), DIM),
                status,
            ]),
        );
    }

    tui_output::blank(buffer);
    tui_output::dim_msg(
        buffer,
        "Pick a provider below to set or update its key.".into(),
    );
}
