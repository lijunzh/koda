//! Native TUI wizard handlers for /provider, /compact, /trust.
//!
//! Extracted from tui_commands.rs to keep files under 600 lines.
//! All output rendered through tui_output::write_line(&).

use crate::tui_output;

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
use tui_output::{BOLD, CYAN, DIM};

// ── Provider (native TUI) ───────────────────────────────────

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
        Ok(Err(CompactSkip::HistoryTooLarge)) => {
            warn_msg("History is too large for this model to summarize without data loss.".into());
            dim_msg(
                "Switch to a model with a larger context window, or start a new session.".into(),
            );
        }
        Err(e) => err_msg(format!("Compact failed: {e:#}")),
    }
}

// ── Purge (native TUI) ───────────────────────────────────────────

pub(crate) async fn handle_purge(
    _terminal: &mut Term,
    session: &KodaSession,
    age_filter: Option<&str>,
) {
    use koda_core::persistence::Persistence;

    // Parse age filter: "90d" -> 90, "7d" -> 7, None -> 0 (all)
    let min_age_days: u32 = match age_filter {
        Some(s) => {
            let s = s.trim().trim_end_matches('d');
            match s.parse() {
                Ok(d) => d,
                Err(_) => {
                    err_msg(format!("Invalid age filter: '{s}'. Use e.g. /purge 90d"));
                    return;
                }
            }
        }
        None => 0,
    };

    // Show stats first
    let stats = match session.db.compacted_stats().await {
        Ok(s) => s,
        Err(e) => {
            err_msg(format!("Failed to query stats: {e:#}"));
            return;
        }
    };

    if stats.message_count == 0 {
        dim_msg("No compacted messages to purge.".into());
        return;
    }

    let size_str = format_bytes(stats.size_bytes);
    let oldest_str = stats.oldest.as_deref().unwrap_or("unknown");
    let age_str = if min_age_days > 0 {
        format!(" older than {min_age_days} days")
    } else {
        String::new()
    };

    tui_output::write_line(&Line::from(vec![
        Span::styled("  \u{1f9f9} ", BOLD),
        Span::styled(
            format!(
                "{} compacted messages across {} sessions ({size_str}), oldest from {oldest_str}{age_str}",
                stats.message_count, stats.session_count
            ),
            CYAN,
        ),
    ]));

    // Ask for confirmation
    tui_output::write_line(&Line::styled(
        "  Permanently delete? This cannot be undone. [y/N] ",
        DIM,
    ));

    // Read a single key
    use crossterm::event::{self, Event, KeyCode};
    let confirmed = loop {
        if let Ok(Event::Key(key)) = event::read() {
            break matches!(key.code, KeyCode::Char('y' | 'Y'));
        }
    };

    if !confirmed {
        dim_msg("Purge cancelled.".into());
        return;
    }

    match session.db.purge_compacted(min_age_days).await {
        Ok(deleted) => {
            ok_msg(format!("Purged {deleted} archived messages."));
        }
        Err(e) => {
            err_msg(format!("Purge failed: {e:#}"));
        }
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

// ── Skills ───────────────────────────────────────────────

#[allow(unused_variables)]
pub(crate) fn handle_list_skills(
    terminal: &mut Term,
    query: Option<&str>,
    tools: &koda_core::tools::ToolRegistry,
) {
    let skills = match query {
        Some(q) if !q.is_empty() => tools.search_skills(q),
        _ => tools.list_skills(),
    };

    tui_output::write_blank();
    tui_output::write_line(&Line::styled("  \u{1f4da} Skills", BOLD));
    tui_output::write_blank();

    if skills.is_empty() {
        match query {
            Some(q) => dim_msg(format!("No skills matching '{q}'.")),
            None => dim_msg("No skills available.".into()),
        }
    } else {
        for (name, description, source) in &skills {
            let tag = match source.as_str() {
                "user" => " [user]",
                "project" => " [project]",
                _ => "",
            };
            tui_output::write_line(&Line::from(vec![
                Span::styled(format!("  {name}"), CYAN),
                Span::raw(format!(" \u{2014} {description}")),
                Span::styled(tag, DIM),
            ]));
        }
    }

    tui_output::write_blank();
    dim_msg("Ask Koda to activate a skill, or use ActivateSkill tool directly.".into());
    dim_msg(
        "Create your own: .koda/skills/<name>/SKILL.md or ~/.config/koda/skills/<name>/SKILL.md"
            .into(),
    );
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

pub(crate) fn save_provider(config: &KodaConfig) {
    let mut s = koda_core::approval::Settings::load();
    let _ = s.save_last_provider(
        &config.provider_type.to_string(),
        &config.base_url,
        &config.model,
    );
}
