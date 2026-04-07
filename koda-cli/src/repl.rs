//! REPL command dispatch — parses slash commands and returns actions.
//!
//! This module is shared between the TUI and headless entry points.
//! It parses the command string and returns a [`ReplAction`] enum that
//! the caller translates into UI-specific behavior.
//!
//! See [`crate`] module docs for the full command table.

use koda_core::config::{KodaConfig, ProviderType};
use koda_core::providers::LlmProvider;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Action to take after processing a REPL command.
///
/// Returned by [`handle_command`] and translated into UI-specific
/// behavior by the TUI event loop (`tui_app.rs`) or the headless runner.
/// Variants with no data are signals; variants with data carry parsed arguments.
pub enum ReplAction {
    /// `/exit` — terminate the session.
    Quit,
    /// `/model <name>` — switch to a specific model or alias.
    SwitchModel(String),
    /// `/model` (no arg) — open the interactive model picker.
    PickModel,
    /// `/provider <name>` — configure a specific provider.
    SetupProvider(ProviderType, String), // (provider_type, base_url)
    /// `/provider` (no arg) — open the interactive provider picker.
    PickProvider,
    /// `/help` — show the help panel.
    ShowHelp,
    /// `/sessions` (no arg) — open the session list picker.
    ListSessions,
    /// `/sessions resume <id>` or `/sessions <id>` — resume a session.
    ResumeSession(String),
    /// `/sessions delete <id>` — delete a session.
    DeleteSession(String),
    /// Inject text as if the user typed it.
    ///
    /// Used by `/diff review` and `/diff commit` to inject a pre-built prompt.
    InjectPrompt(String),
    /// `/compact` — summarise conversation history to free context tokens.
    Compact,
    /// `/purge [<age>]` — delete archived messages older than `age`.
    ///
    /// Age is a string like `"90d"`, `"30d"`, etc. `None` means "all".
    Purge(Option<String>),
    /// `/expand [<n>]` — show the full output of the Nth-most-recent tool call.
    ///
    /// `n` defaults to 1 (most recent). Output is replayed untruncated.
    Expand(usize),
    /// `/verbose [on|off]` — toggle or set verbose tool output.
    ///
    /// `None` = toggle; `Some(true/false)` = set explicitly.
    Verbose(Option<bool>),
    /// `/agent` — open the sub-agent picker.
    ListAgents,
    /// `/diff` (no sub-command) — show the pending git diff summary.
    ShowDiff,
    /// `/memory [save|<text>]` — view/append memory files.
    MemoryCommand(Option<String>),
    /// `/undo` — revert file mutations from the last turn.
    Undo,
    /// `/skills [<query>]` — list available skills, optionally filtered.
    ListSkills(Option<String>),
    /// `/key` — open the API key manager.
    ManageKeys,
    /// Command was handled internally (UI action already taken).
    #[allow(dead_code)]
    Handled,
    /// Input was not a slash command — treat as a chat message.
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
        "/purge" => ReplAction::Purge(arg.map(|s| s.to_string())),

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

        "/skills" => ReplAction::ListSkills(arg.map(|s| s.to_string())),

        "/key" | "/keys" => ReplAction::ManageKeys,

        _ => ReplAction::NotACommand,
    }
}

/// Available providers for the interactive picker.
///
/// Tuple: (internal_key, display_name). Descriptions like "Local, no API key"
/// are derived from `ProviderType::requires_api_key()` at render time.
pub const PROVIDERS: &[(&str, &str)] = &[
    ("lmstudio", "LM Studio"),
    ("ollama", "Ollama"),
    ("openai", "OpenAI"),
    ("anthropic", "Anthropic"),
    ("deepseek", "DeepSeek"),
    ("gemini", "Google Gemini"),
    ("groq", "Groq"),
    ("grok", "Grok (xAI)"),
    ("mistral", "Mistral"),
    ("minimax", "MiniMax"),
    ("openrouter", "OpenRouter"),
    ("together", "Together"),
    ("fireworks", "Fireworks"),
    ("vllm", "vLLM"),
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
