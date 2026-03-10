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
    /// Undo last turn's file mutations
    Undo,
    /// Show learned intervention priors
    ShowPriors,
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

        "/undo" => ReplAction::Undo,

        "/priors" => ReplAction::ShowPriors,

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
