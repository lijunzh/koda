//! REPL command handling and display helpers.
//!
//! Handles slash commands (/model, /provider, /help, /quit)
//! and the interactive provider/model pickers.

use crate::config::{KodaConfig, ProviderType};
use crate::providers::LlmProvider;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Action to take after processing a REPL command.
pub enum ReplAction {
    Quit,
    SwitchModel(String),
    PickModel,
    SetupProvider(ProviderType, String), // (provider_type, base_url)
    PickProvider,
    ShowHelp,
    ShowCost,
    ListSessions,
    DeleteSession(String),
    /// Inject text as if the user typed it (used by /diff review, /diff commit)
    InjectPrompt(String),
    /// Compact the conversation by summarizing history
    Compact,
    Handled,
    NotACommand,
}

/// Parse and handle a slash command. Returns the action for the main loop.
pub async fn handle_command(
    input: &str,
    _config: &KodaConfig,
    _provider: &Arc<RwLock<Box<dyn LlmProvider>>>,
) -> ReplAction {
    let parts: Vec<&str> = input.splitn(2, ' ').collect();
    let cmd = parts[0];
    let arg = parts.get(1).map(|s| s.trim());

    match cmd {
        "/quit" | "/exit" => ReplAction::Quit,

        "/copy" => {
            let response = crate::clipboard::get_last_response();
            if response.is_empty() {
                println!("  \x1b[90mNo response to copy yet.\x1b[0m");
                return ReplAction::Handled;
            }

            let blocks = crate::clipboard::extract_code_blocks(&response);

            let text_to_copy = match arg {
                // /copy all — copy full response
                Some("all") | None if blocks.is_empty() => response.clone(),
                // /copy <n> — copy specific code block
                Some(n) if n.parse::<usize>().is_ok() => {
                    let idx = n.parse::<usize>().unwrap();
                    if idx == 0 || idx > blocks.len() {
                        println!(
                            "  \x1b[31mBlock {idx} not found. {} code block(s) available.\x1b[0m",
                            blocks.len()
                        );
                        return ReplAction::Handled;
                    }
                    blocks[idx - 1].1.clone()
                }
                // /copy with code blocks — show picker
                _ => {
                    println!();
                    println!("  \x1b[1m\u{1f4cb} Code Blocks\x1b[0m");
                    println!();
                    for (i, (lang, code)) in blocks.iter().enumerate() {
                        let lang_str = lang.as_deref().unwrap_or("text");
                        let preview: String =
                            code.lines().next().unwrap_or("").chars().take(50).collect();
                        println!(
                            "  \x1b[36m{}\x1b[0m. \x1b[90m[{lang_str}]\x1b[0m {preview}",
                            i + 1
                        );
                    }
                    println!(
                        "  \x1b[36m{}\x1b[0m. \x1b[90m[full response]\x1b[0m",
                        blocks.len() + 1
                    );
                    println!();
                    println!(
                        "  \x1b[90m/copy <number> to copy a block, /copy all for full response\x1b[0m"
                    );
                    return ReplAction::Handled;
                }
            };

            match crate::clipboard::copy_to_clipboard(&text_to_copy) {
                Ok(()) => {
                    let chars = text_to_copy.len();
                    println!("  \x1b[32m\u{2713}\x1b[0m Copied to clipboard ({chars} chars)");
                }
                Err(e) => println!("  \x1b[31m{e}\x1b[0m"),
            }
            ReplAction::Handled
        }

        "/paste" => {
            match crate::clipboard::paste_from_clipboard() {
                Ok(content) => {
                    if content.is_empty() {
                        println!("  \x1b[90mClipboard is empty.\x1b[0m");
                    } else {
                        let lines = content.lines().count();
                        println!(
                            "  \x1b[32m\u{1f4cb}\x1b[0m Pasted {lines} line(s) from clipboard"
                        );
                        // Return as a NotACommand so it gets processed as user input
                        // But we need to return the content somehow...
                        // For now, just print it and let user manually reference it
                        println!();
                        for line in content.lines().take(20) {
                            println!("  \x1b[90m{line}\x1b[0m");
                        }
                        if lines > 20 {
                            println!("  \x1b[90m... ({} more lines)\x1b[0m", lines - 20);
                        }
                        println!();
                        println!(
                            "  \x1b[90mClipboard content shown above. Ask Koda about it!\x1b[0m"
                        );
                    }
                }
                Err(e) => println!("  \x1b[31m{e}\x1b[0m"),
            }
            ReplAction::Handled
        }

        "/model" => match arg {
            Some(model) => ReplAction::SwitchModel(model.to_string()),
            None => ReplAction::PickModel,
        },

        "/provider" => match arg {
            Some(name) => {
                let ptype = ProviderType::from_url_or_name("", Some(name));
                let base_url = ptype.default_base_url().to_string();
                ReplAction::SetupProvider(ptype, base_url)
            }
            None => ReplAction::PickProvider,
        },

        "/help" => ReplAction::ShowHelp,

        "/cost" => ReplAction::ShowCost,

        "/diff" => {
            // Run git diff
            let output = std::process::Command::new("git")
                .args(["diff", "--stat"])
                .output();

            let diff_stat = match output {
                Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
                Ok(o) => {
                    let err = String::from_utf8_lossy(&o.stderr);
                    println!("  \x1b[31mGit error: {err}\x1b[0m");
                    return ReplAction::Handled;
                }
                Err(e) => {
                    println!("  \x1b[31mFailed to run git: {e}\x1b[0m");
                    return ReplAction::Handled;
                }
            };

            if diff_stat.trim().is_empty() {
                // Try staged changes
                let staged = std::process::Command::new("git")
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
                    });

                if staged.is_none() {
                    println!("  \x1b[90mNo uncommitted changes.\x1b[0m");
                    return ReplAction::Handled;
                }
            }

            match arg {
                Some("review") => {
                    // Get full diff for LLM review
                    let full_diff = get_git_diff();
                    ReplAction::InjectPrompt(format!(
                        "Review these uncommitted changes. Point out bugs, improvements, and concerns:\n\n```diff\n{full_diff}\n```"
                    ))
                }
                Some("commit") => {
                    // Get full diff for commit message generation
                    let full_diff = get_git_diff();
                    ReplAction::InjectPrompt(format!(
                        "Write a conventional commit message for these changes. Use the format: type: description\n\nInclude a body with bullet points for each logical change.\n\n```diff\n{full_diff}\n```"
                    ))
                }
                _ => {
                    // Just show the summary
                    println!();
                    println!("  \x1b[1m\u{1f43b} Uncommitted Changes\x1b[0m");
                    println!();
                    for line in diff_stat.lines() {
                        println!("  \x1b[90m{line}\x1b[0m");
                    }
                    println!();
                    println!(
                        "  \x1b[90m/diff review   \u{2014} ask Koda to review the changes\x1b[0m"
                    );
                    println!("  \x1b[90m/diff commit   \u{2014} generate a commit message\x1b[0m");
                    ReplAction::Handled
                }
            }
        }

        "/compact" => ReplAction::Compact,

        "/agent" => {
            let project_root = std::env::current_dir().unwrap_or_default();
            let listing = crate::tools::agent::list_agents(&project_root);
            println!();
            println!("  \x1b[1m\u{1f43b} Sub-Agents\x1b[0m");
            println!();
            if listing.contains("No ") {
                println!("  \x1b[90m{listing}\x1b[0m");
            } else {
                println!("{listing}");
            }
            println!();
            println!("  \x1b[90mAsk Koda to invoke them, or use koda --agent <name>\x1b[0m");
            println!(
                "  \x1b[90mNeed a specialist? Ask Koda to create one for recurring tasks\x1b[0m"
            );
            ReplAction::Handled
        }

        "/sessions" => match arg {
            Some(sub) if sub.starts_with("delete ") => {
                let id = sub.strip_prefix("delete ").unwrap().trim().to_string();
                ReplAction::DeleteSession(id)
            }
            _ => ReplAction::ListSessions,
        },

        "/memory" => {
            let project_root = std::env::current_dir().unwrap_or_default();
            match arg {
                Some(text) if text.starts_with("global ") => {
                    let entry = text.strip_prefix("global ").unwrap().trim();
                    if entry.is_empty() {
                        println!("  Usage: /memory global <text>");
                    } else {
                        match crate::memory::append_global(entry) {
                            Ok(()) => println!("  \x1b[32m\u{2713}\x1b[0m Saved to global memory"),
                            Err(e) => println!("  \x1b[31mError: {e}\x1b[0m"),
                        }
                    }
                }
                Some(text) if text.starts_with("add ") => {
                    let entry = text.strip_prefix("add ").unwrap().trim();
                    if entry.is_empty() {
                        println!("  Usage: /memory add <text>");
                    } else {
                        match crate::memory::append(&project_root, entry) {
                            Ok(()) => println!(
                                "  \x1b[32m\u{2713}\x1b[0m Saved to project memory (MEMORY.md)"
                            ),
                            Err(e) => println!("  \x1b[31mError: {e}\x1b[0m"),
                        }
                    }
                }
                _ => {
                    // Show current memory status
                    let active = crate::memory::active_project_file(&project_root);
                    println!();
                    println!("  \x1b[1m\u{1f43b} Memory\x1b[0m");
                    println!();
                    match active {
                        Some(f) => println!("  Project: \x1b[36m{f}\x1b[0m"),
                        None => println!(
                            "  Project: \x1b[90m(none — will create MEMORY.md on first write)\x1b[0m"
                        ),
                    }
                    println!("  Global:  \x1b[36m~/.config/koda/memory.md\x1b[0m");
                    println!();
                    println!("  Commands:");
                    println!("    /memory add <text>      Save to project MEMORY.md");
                    println!("    /memory global <text>   Save to global memory");
                    println!();
                    println!(
                        "  \x1b[90mTip: the LLM can also call MemoryWrite to save insights automatically.\x1b[0m"
                    );
                }
            }
            ReplAction::Handled
        }

        _ => ReplAction::NotACommand,
    }
}

/// Available providers for the interactive picker.
pub const PROVIDERS: &[(&str, &str, &str)] = &[
    ("lmstudio", "LM Studio", "Local models, no API key needed"),
    ("openai", "OpenAI", "GPT-4o, GPT-4, GPT-3.5"),
    ("anthropic", "Anthropic", "Claude Sonnet, Haiku, Opus"),
    ("gemini", "Google Gemini", "Gemini 2.0 Flash, Pro"),
    ("groq", "Groq", "Llama 3.3, Mixtral (fast)"),
    ("grok", "Grok (xAI)", "Grok-3, Grok-2"),
];

// \u{2500}\u{2500} Display Helpers \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}

/// Print the startup banner with a two-column layout (Claude-style):
/// Title embedded in top border, left = mascot + info, right = tips + recent.
pub fn print_banner(config: &KodaConfig, _session_id: &str, recent_activity: &[String]) {
    let ver = env!("CARGO_PKG_VERSION");
    let cwd = pretty_cwd();

    // ── Column widths ────────────────────────────────────────
    let left_width: usize = 34;
    let right_width: usize = 56;
    let divider_width: usize = 3; // " │ "
    let total = left_width + divider_width + right_width;

    // ── Top border with embedded title ───────────────────────
    let title = format!(
        " \x1b[1;36m\u{1f43b} Koda\x1b[0m\x1b[36m v{ver} ",
        ver = ver
    );
    let title_visible = visible_len(&title);
    let remaining = (total + 2).saturating_sub(title_visible + 2); // +2 for padding
    let top_border = format!("  \x1b[36m╭──{}{}╮\x1b[0m", title, "─".repeat(remaining),);

    // ── Left column: welcome + ASCII art + info ──────────────
    let left: Vec<String> = vec![
        String::new(),
        "   \x1b[1mWelcome back!\x1b[0m".to_string(),
        String::new(),
        format!("   \x1b[36m{}\x1b[0m", config.model),
        format!("   \x1b[36m{}\x1b[0m", config.provider_type),
        format!("   \x1b[34m{}\x1b[0m", cwd),
    ];

    // ── Right column: tips + recent activity ─────────────────
    let sep_line = format!("\x1b[90m{}\x1b[0m", "─".repeat(right_width));

    let mut right: Vec<String> = vec![
        "\x1b[1;36mTips for getting started\x1b[0m".to_string(),
        "  \x1b[90m/model\x1b[0m      pick a model".to_string(),
        "  \x1b[90m/provider\x1b[0m   switch provider".to_string(),
        "  \x1b[90m/help\x1b[0m       all commands".to_string(),
        sep_line,
    ];

    right.push("\x1b[1;36mRecent activity\x1b[0m".to_string());
    if recent_activity.is_empty() {
        right.push("  \x1b[90mNo recent activity\x1b[0m".to_string());
    } else {
        for msg in recent_activity.iter().take(3) {
            let truncated = truncate_visible(msg.lines().next().unwrap_or(""), 52);
            right.push(format!("  \x1b[90m•\x1b[0m {truncated}"));
        }
    }

    // ── Render ───────────────────────────────────────────────
    let rows = left.len().max(right.len());

    println!();
    println!("{top_border}");

    for i in 0..rows {
        let l = left.get(i).map(|s| s.as_str()).unwrap_or("");
        let r = right.get(i).map(|s| s.as_str()).unwrap_or("");
        let l_pad = left_width.saturating_sub(visible_len(l));
        let r_pad = right_width.saturating_sub(visible_len(r));
        println!(
            "  \x1b[36m│\x1b[0m {l}{} \x1b[90m│\x1b[0m {r}{} \x1b[36m│\x1b[0m",
            " ".repeat(l_pad),
            " ".repeat(r_pad),
        );
    }

    // bottom border
    println!("  \x1b[36m╰{}╯\x1b[0m", "─".repeat(total + 2));
    println!();
}

/// Count visible characters (strip ANSI escape sequences).
fn visible_len(s: &str) -> usize {
    let mut len = 0;
    let mut in_escape = false;
    for c in s.chars() {
        if c == '\x1b' {
            in_escape = true;
        } else if in_escape {
            if c == 'm' {
                in_escape = false;
            }
        } else {
            // emoji/wide chars count as 2
            len += if c > '\u{FFFF}' { 2 } else { 1 };
        }
    }
    len
}

/// Truncate a string to `max` visible characters, appending "…" if needed.
fn truncate_visible(s: &str, max: usize) -> String {
    let mut visible = 0;
    let mut end = s.len();
    for (i, c) in s.char_indices() {
        let w = if c > '\u{FFFF}' { 2 } else { 1 };
        if visible + w > max.saturating_sub(1) {
            end = i;
            break;
        }
        visible += w;
    }
    if end < s.len() {
        format!("{}…", &s[..end])
    } else {
        s.to_string()
    }
}

/// Format the REPL prompt: `[Koda 🐻] [model] (~/repo) ❯`
/// Shows a context warning when usage exceeds 75%.
pub fn format_prompt(model: &str) -> String {
    let cwd = pretty_cwd();
    let pct = crate::context::percentage();
    let ctx_warn = if pct >= 90 {
        format!(" \x1b[31m(\u{26a0} {pct}% context)\x1b[0m")
    } else if pct >= 75 {
        format!(" \x1b[33m(\u{26a0} {pct}% context)\x1b[0m")
    } else {
        String::new()
    };
    format!(
        "\x1b[36m[Koda \u{1f43b}]\x1b[0m \x1b[90m[{model}]\x1b[0m \x1b[34m({cwd})\x1b[0m{ctx_warn} \x1b[32m\u{276f}\x1b[0m "
    )
}

/// Return a human-friendly current directory (collapse $HOME to ~).
fn pretty_cwd() -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE"))
        && let Ok(rest) = cwd.strip_prefix(&home)
    {
        return format!("~/{}", rest.display())
            .trim_end_matches('/')
            .to_string();
    }
    cwd.display().to_string()
}

/// Get the full git diff (unstaged + staged), capped for context window safety.
fn get_git_diff() -> String {
    const MAX_DIFF_CHARS: usize = 30_000;

    let unstaged = std::process::Command::new("git")
        .args(["diff"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();

    let staged = std::process::Command::new("git")
        .args(["diff", "--cached"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();

    let mut diff = String::new();
    if !unstaged.is_empty() {
        diff.push_str(&unstaged);
    }
    if !staged.is_empty() {
        if !diff.is_empty() {
            diff.push_str("\n# --- Staged changes ---\n\n");
        }
        diff.push_str(&staged);
    }

    if diff.len() > MAX_DIFF_CHARS {
        let mut end = MAX_DIFF_CHARS;
        while end > 0 && !diff.is_char_boundary(end) {
            end -= 1;
        }
        format!(
            "{}\n\n[TRUNCATED: diff was {} chars, showing first {}]",
            &diff[..end],
            diff.len(),
            MAX_DIFF_CHARS
        )
    } else {
        diff
    }
}
