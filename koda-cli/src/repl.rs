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
    /// `/copy [<n>]` — copy the Nth-most-recent assistant response to clipboard.
    ///
    /// `n` defaults to 1 (most recent). `/copy 2` copies the second-to-last, etc.
    CopyResponse(usize),
    /// `/export [<file.md>]` — export the full session transcript.
    ///
    /// Without an argument: write to a auto-named file in the current directory.
    /// With a filename: write Markdown to that path.
    Export(Option<String>),
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

        "/copy" => {
            let n: usize = arg.and_then(|s| s.parse().ok()).unwrap_or(1).max(1);
            ReplAction::CopyResponse(n)
        }
        "/export" => ReplAction::Export(arg.map(|s| s.to_string())),

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

#[cfg(test)]
mod tests {
    use super::{ReplAction, handle_command};
    use koda_core::config::{KodaConfig, ProviderType};
    use koda_core::providers::mock::{MockProvider, MockResponse};
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn dispatch(input: &str) -> ReplAction {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let config = KodaConfig::default_for_testing(ProviderType::LMStudio);
        let provider: Arc<RwLock<Box<dyn koda_core::providers::LlmProvider>>> =
            Arc::new(RwLock::new(Box::new(MockProvider::new(vec![
                MockResponse::Text(String::new()),
            ]))));
        rt.block_on(handle_command(input, &config, &provider))
    }

    #[test]
    fn exit_command_returns_quit() {
        assert!(matches!(dispatch("/exit"), ReplAction::Quit));
    }

    #[test]
    fn model_bare_returns_pick_model() {
        assert!(matches!(dispatch("/model"), ReplAction::PickModel));
    }

    #[test]
    fn model_with_name_returns_switch_model() {
        assert!(matches!(
            dispatch("/model gpt-4o"),
            ReplAction::SwitchModel(_)
        ));
        if let ReplAction::SwitchModel(name) = dispatch("/model gpt-4o") {
            assert_eq!(name, "gpt-4o");
        }
    }

    #[test]
    fn provider_bare_returns_pick_provider() {
        assert!(matches!(dispatch("/provider"), ReplAction::PickProvider));
    }

    #[test]
    fn provider_with_name_returns_setup_provider() {
        assert!(matches!(
            dispatch("/provider openai"),
            ReplAction::SetupProvider(_, _)
        ));
    }

    #[test]
    fn help_returns_show_help() {
        assert!(matches!(dispatch("/help"), ReplAction::ShowHelp));
    }

    #[test]
    fn diff_bare_returns_show_diff() {
        assert!(matches!(dispatch("/diff"), ReplAction::ShowDiff));
    }

    #[test]
    fn diff_review_returns_inject_prompt() {
        assert!(matches!(
            dispatch("/diff review"),
            ReplAction::InjectPrompt(_)
        ));
    }

    #[test]
    fn diff_commit_returns_inject_prompt() {
        assert!(matches!(
            dispatch("/diff commit"),
            ReplAction::InjectPrompt(_)
        ));
    }

    #[test]
    fn sessions_bare_returns_list_sessions() {
        assert!(matches!(dispatch("/sessions"), ReplAction::ListSessions));
    }

    #[test]
    fn sessions_delete_returns_delete_session() {
        assert!(matches!(
            dispatch("/sessions delete abc123"),
            ReplAction::DeleteSession(_)
        ));
        if let ReplAction::DeleteSession(id) = dispatch("/sessions delete abc123") {
            assert_eq!(id, "abc123");
        }
    }

    #[test]
    fn sessions_resume_returns_resume_session() {
        assert!(matches!(
            dispatch("/sessions resume abc123"),
            ReplAction::ResumeSession(_)
        ));
        if let ReplAction::ResumeSession(id) = dispatch("/sessions resume abc123") {
            assert_eq!(id, "abc123");
        }
    }

    #[test]
    fn sessions_bare_id_returns_resume_session() {
        assert!(matches!(
            dispatch("/sessions abc12345"),
            ReplAction::ResumeSession(_)
        ));
    }

    #[test]
    fn expand_returns_expand() {
        assert!(matches!(dispatch("/expand"), ReplAction::Expand(_)));
        if let ReplAction::Expand(n) = dispatch("/expand") {
            assert_eq!(n, 1);
        }
        if let ReplAction::Expand(n) = dispatch("/expand 3") {
            assert_eq!(n, 3);
        }
    }

    #[test]
    fn verbose_bare_returns_toggle() {
        assert!(matches!(dispatch("/verbose"), ReplAction::Verbose(None)));
    }

    #[test]
    fn verbose_on_returns_true() {
        assert!(matches!(
            dispatch("/verbose on"),
            ReplAction::Verbose(Some(true))
        ));
    }

    #[test]
    fn verbose_off_returns_false() {
        assert!(matches!(
            dispatch("/verbose off"),
            ReplAction::Verbose(Some(false))
        ));
    }

    #[test]
    fn memory_bare_returns_memory_command() {
        assert!(matches!(
            dispatch("/memory"),
            ReplAction::MemoryCommand(None)
        ));
    }

    #[test]
    fn memory_with_arg_returns_memory_command_some() {
        assert!(matches!(
            dispatch("/memory add test"),
            ReplAction::MemoryCommand(Some(_))
        ));
        assert!(matches!(
            dispatch("/memory global test"),
            ReplAction::MemoryCommand(Some(_))
        ));
    }

    #[test]
    fn compact_returns_compact() {
        assert!(matches!(dispatch("/compact"), ReplAction::Compact));
    }

    #[test]
    fn agent_returns_list_agents() {
        assert!(matches!(dispatch("/agent"), ReplAction::ListAgents));
    }

    #[test]
    fn undo_returns_undo() {
        assert!(matches!(dispatch("/undo"), ReplAction::Undo));
    }

    #[test]
    fn skills_bare_returns_list_skills_none() {
        assert!(matches!(dispatch("/skills"), ReplAction::ListSkills(None)));
    }

    #[test]
    fn skills_with_query_returns_list_skills_some() {
        assert!(matches!(
            dispatch("/skills review"),
            ReplAction::ListSkills(Some(_))
        ));
        if let ReplAction::ListSkills(Some(q)) = dispatch("/skills review") {
            assert_eq!(q, "review");
        }
    }

    #[test]
    fn key_command_manages_keys() {
        assert!(matches!(dispatch("/key"), ReplAction::ManageKeys));
        assert!(matches!(dispatch("/keys"), ReplAction::ManageKeys));
    }

    #[test]
    fn unknown_commands_fall_through() {
        assert!(matches!(dispatch("/foobar"), ReplAction::NotACommand));
        assert!(matches!(dispatch("/foo"), ReplAction::NotACommand));
        assert!(matches!(dispatch("/set"), ReplAction::NotACommand));
        assert!(matches!(dispatch("/config"), ReplAction::NotACommand));
        assert!(matches!(dispatch("/transcript"), ReplAction::NotACommand));
        assert!(matches!(dispatch("/export"), ReplAction::Export(None)));
        assert!(matches!(
            dispatch("/export koda.md"),
            ReplAction::Export(Some(_))
        ));
    }

    #[test]
    fn copy_defaults_to_last_response() {
        assert!(matches!(dispatch("/copy"), ReplAction::CopyResponse(1)));
    }

    #[test]
    fn copy_with_n_returns_nth() {
        assert!(matches!(dispatch("/copy 3"), ReplAction::CopyResponse(3)));
    }

    #[test]
    fn copy_with_zero_clamps_to_one() {
        // 0 is nonsensical — clamp to 1 via .max(1)
        assert!(matches!(dispatch("/copy 0"), ReplAction::CopyResponse(1)));
    }
}
