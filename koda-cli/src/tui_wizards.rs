//! Native TUI wizard handlers — multi-step interactive flows.
//!
//! ## Wizards
//!
//! - **`/provider`** — provider selection menu (Anthropic, Gemini, etc.)
//! - **`/trust`** — set approval mode for the session
//! - **`/key`** — API key entry (masked input)
//!
//! All output flows through the [`crate::scroll_buffer::ScrollBuffer`] render cache.

use crate::scroll_buffer::ScrollBuffer;
use crate::tui_output;

use ratatui::text::{Line, Span};

use tui_output::{BOLD, CYAN, DIM};

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
                    Ok(()) => tui_output::ok_msg(buffer, "Saved to Koda's global notes".into()),
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
                        tui_output::ok_msg(buffer, "Saved to Koda's notes for this project".into())
                    }
                    Err(e) => tui_output::err_msg(buffer, format!("Error: {e}")),
                }
            }
        }
        _ => {
            let path_or = |p: anyhow::Result<std::path::PathBuf>| {
                p.map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "(no config directory)".into())
            };
            tui_output::blank(buffer);
            tui_output::emit_line(buffer, Line::styled("  \u{1f43b} Memory", BOLD));
            tui_output::blank(buffer);
            match koda_core::memory::active_project_file(project_root) {
                Some(f) => tui_output::emit_line(
                    buffer,
                    Line::from(vec![Span::raw("  Instructions: "), Span::styled(f, CYAN)]),
                ),
                None => tui_output::dim_msg(
                    buffer,
                    "Instructions: (none \u{2014} add an AGENTS.md to the project)".into(),
                ),
            }
            for (label, path) in [
                (
                    "  Notes:        ",
                    path_or(koda_core::memory::project_notes_path(project_root)),
                ),
                (
                    "  Global notes: ",
                    path_or(koda_core::memory::global_notes_path()),
                ),
            ] {
                tui_output::emit_line(
                    buffer,
                    Line::from(vec![Span::raw(label), Span::styled(path, CYAN)]),
                );
            }
            tui_output::blank(buffer);
            tui_output::dim_msg(buffer, "Commands:".into());
            tui_output::dim_msg(
                buffer,
                "  /memory add <text>      Save to Koda's notes for this project".into(),
            );
            tui_output::dim_msg(
                buffer,
                "  /memory global <text>   Save to Koda's global notes".into(),
            );
            tui_output::blank(buffer);
            tui_output::dim_msg(
                buffer,
                "Tip: the LLM can also call MemoryWrite to save insights automatically.".into(),
            );
        }
    }
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
