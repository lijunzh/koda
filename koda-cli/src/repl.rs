//! REPL command handling and display helpers.
//!
//! Handles slash commands (/model, /provider, /help, /quit)
//! and the interactive provider/model pickers.

use koda_core::config::{KodaConfig, ProviderType};
use koda_core::providers::LlmProvider;
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
    ResumeSession(String),
    DeleteSession(String),
    /// Inject text as if the user typed it (used by /diff review, /diff commit)
    InjectPrompt(String),
    /// Compact the conversation by summarizing history
    Compact,
    /// Switch approval mode (with optional name, or interactive picker)
    SetTrust(Option<String>),
    /// MCP server management command
    McpCommand(String),
    /// Expand Nth most recent tool output (1 = last)
    Expand(usize),
    /// Toggle verbose tool output (None = toggle, Some = set)
    Verbose(Option<bool>),
    /// List available sub-agents
    ListAgents,
    /// Show git diff summary
    ShowDiff,
    /// Memory management command
    MemoryCommand(Option<String>),
    #[allow(dead_code)]
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
        "/exit" => ReplAction::Quit,

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

        "/trust" => match arg {
            Some(mode_name) => ReplAction::SetTrust(Some(mode_name.to_string())),
            None => ReplAction::SetTrust(None),
        },

        "/diff" => match arg {
            Some("review") => {
                let full_diff = get_git_diff();
                ReplAction::InjectPrompt(format!(
                    "Review these uncommitted changes. Point out bugs, improvements, and concerns:\n\n```diff\n{full_diff}\n```"
                ))
            }
            Some("commit") => {
                let full_diff = get_git_diff();
                ReplAction::InjectPrompt(format!(
                    "Write a conventional commit message for these changes. Use the format: type: description\n\nInclude a body with bullet points for each logical change.\n\n```diff\n{full_diff}\n```"
                ))
            }
            _ => ReplAction::ShowDiff,
        },

        "/compact" => ReplAction::Compact,

        "/mcp" => ReplAction::McpCommand(arg.unwrap_or("").to_string()),

        "/expand" => {
            let n: usize = arg.and_then(|s| s.parse().ok()).unwrap_or(1);
            ReplAction::Expand(n)
        }

        "/verbose" => match arg {
            Some("on") => ReplAction::Verbose(Some(true)),
            Some("off") => ReplAction::Verbose(Some(false)),
            _ => ReplAction::Verbose(None), // toggle
        },

        "/agent" => ReplAction::ListAgents,

        "/sessions" => match arg {
            Some(sub) if sub.starts_with("delete ") => {
                let id = sub.strip_prefix("delete ").unwrap().trim().to_string();
                ReplAction::DeleteSession(id)
            }
            Some(sub) if sub.starts_with("resume ") => {
                let id = sub.strip_prefix("resume ").unwrap().trim().to_string();
                ReplAction::ResumeSession(id)
            }
            // Bare ID shorthand: /sessions <id>
            Some(id) if !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') => {
                ReplAction::ResumeSession(id.to_string())
            }
            _ => ReplAction::ListSessions,
        },

        "/memory" => ReplAction::MemoryCommand(arg.map(|s| s.to_string())),

        _ => ReplAction::NotACommand,
    }
}

/// Available providers for the interactive picker.
pub const PROVIDERS: &[(&str, &str, &str)] = &[
    ("lmstudio", "LM Studio", "Local models, no API key needed"),
    ("ollama", "Ollama", "Local models, no API key needed"),
    ("openai", "OpenAI", "GPT-4o, o1, o3"),
    ("anthropic", "Anthropic", "Claude Sonnet, Opus"),
    ("deepseek", "DeepSeek", "DeepSeek-V3, R1"),
    ("gemini", "Google Gemini", "Gemini 2.0 Flash, Pro"),
    ("groq", "Groq", "Fast inference"),
    ("grok", "Grok (xAI)", "Grok-3, Grok-2"),
    ("mistral", "Mistral", "Mistral Large, Codestral"),
    ("minimax", "MiniMax", "MiniMax-01"),
    ("openrouter", "OpenRouter", "Meta-provider, 100+ models"),
    ("together", "Together", "Open-source model hosting"),
    ("fireworks", "Fireworks", "Fast inference"),
    ("vllm", "vLLM", "Local high-performance serving"),
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
        "  \x1b[90m/trust\x1b[0m      plan \x1b[90m\u{2192}\x1b[0m normal \x1b[90m\u{2192}\x1b[0m yolo".to_string(),
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
#[allow(dead_code)] // Used by legacy/headless paths
pub fn format_prompt(model: &str, mode: koda_core::approval::ApprovalMode) -> String {
    let cwd = pretty_cwd();
    let pct = koda_core::context::percentage();
    let ctx_warn = if pct >= 90 {
        format!(" \x1b[31m(\u{26a0} {pct}% context)\x1b[0m")
    } else if pct >= 75 {
        format!(" \x1b[33m(\u{26a0} {pct}% context)\x1b[0m")
    } else {
        String::new()
    };
    // Mode embedded in logo: [Koda 🐻] / [Koda 📋] / [Koda ⚡]
    let (logo_icon, logo_color) = match mode {
        koda_core::approval::ApprovalMode::Plan => ("\u{1f4cb}", "\x1b[33m"),
        koda_core::approval::ApprovalMode::Normal => ("\u{1f43b}", "\x1b[36m"),
        koda_core::approval::ApprovalMode::Yolo => ("\u{26a1}", "\x1b[31m"),
    };
    let mode_label = mode.label();
    format!(
        "{logo_color}[Koda {logo_icon} {mode_label}]\x1b[0m \x1b[90m[{model}]\x1b[0m \x1b[34m({cwd})\x1b[0m{ctx_warn} \x1b[32m\u{276f}\x1b[0m "
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
