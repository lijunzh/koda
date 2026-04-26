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
#[derive(Debug)]
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
    /// `/agents` — list running background tasks (#996 Layers 1+F).
    ///
    /// Distinct from [`ListAgents`] (singular `/agent`) which lists
    /// configured sub-agent **definitions**. This one lists every
    /// **currently-running** background task: both background
    /// sub-agents (tracked in `KodaSession::bg_agents`) and background
    /// shell processes (tracked in `agent.tools.bg_registry`).
    ///
    /// [`ListAgents`]: ReplAction::ListAgents
    ListBackgroundTasks,
    /// `/cancel <id>` — fire one background task's cancel mechanism.
    ///
    /// Accepts three arg forms (parsed by [`parse_task_id`]):
    /// - `agent:N` — cancel a bg sub-agent
    /// - `process:N` — SIGTERM a bg shell process
    /// - `N` (bare numeric) — treated as `agent:N` for back-compat
    ///   with the original #1042 single-registry UX
    ///
    /// `None` means the user typed `/cancel` with no arg, or an arg
    /// that didn't parse — the handler renders a `Usage:` line.
    /// We deliberately keep the arg-parse fallible at this layer so
    /// the dispatch table stays a clean `&'static str -> ReplAction`
    /// without an error sum type leaking into the parser.
    ///
    /// [`parse_task_id`]: koda_core::tools::bg_task_tools::parse_task_id
    CancelBackgroundTask(Option<koda_core::tools::bg_task_tools::TaskId>),
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
    /// `/export [--summary] [<file.md>]` — export the full session transcript.
    ///
    /// Default: verbose (full fidelity — timestamps, token counts, all tool output).
    /// `--summary`: concise, human-readable format.
    Export { dest: Option<String>, summary: bool },
    /// `/mcp` or `/mcp list` — show MCP server list and status.
    McpList,
    /// `/mcp add <name> <command> [args...]` — add a stdio MCP server.
    McpAdd {
        /// Server name (user-chosen identifier).
        name: String,
        /// Command to spawn.
        command: String,
        /// Arguments for the command.
        args: Vec<String>,
    },
    /// `/mcp add-http <name> <url> [--token <bearer>]` — add an HTTP MCP server.
    McpAddHttp {
        /// Server name (user-chosen identifier).
        name: String,
        /// Server URL.
        url: String,
        /// Optional bearer token.
        bearer_token: Option<String>,
    },
    /// `/mcp remove <name>` — remove an MCP server.
    McpRemove {
        /// Server name to remove.
        name: String,
    },
    /// `/mcp reconnect <name>` — reconnect a failed/disconnected server.
    McpReconnect {
        /// Server name to reconnect.
        name: String,
    },
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

        // #996 Layer 1: runtime task management. `/agents` is plural
        // and disjoint from `/agent` — sub-agent **definitions** vs
        // running **tasks**.
        "/agents" => ReplAction::ListBackgroundTasks,

        "/cancel" => {
            // Empty / whitespace-only / unparseable arg → None.
            // The handler renders a Usage: line in those cases
            // rather than the parser silently dropping the command
            // (which would leave the user wondering what they
            // mistyped). `arg` already had its leading whitespace
            // trimmed by `splitn`, but `"   "` survives that trim,
            // so we re-check `is_empty` after a fresh trim.
            //
            // Phase F of #996: parse via the shared
            // `bg_task_tools::parse_task_id` so the TUI accepts the
            // same `agent:N` / `process:N` / bare-numeric forms as
            // the LLM-facing `CancelTask` tool. Bare numeric routes
            // to `Agent` for back-compat with the original #1042 UX.
            let parsed = arg.and_then(|s| {
                let s = s.trim();
                if s.is_empty() {
                    None
                } else {
                    koda_core::tools::bg_task_tools::parse_task_id(s).ok()
                }
            });
            ReplAction::CancelBackgroundTask(parsed)
        }

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
        "/export" => {
            // Parse optional `--summary` flag and destination.
            let (summary, dest) = match arg {
                Some(s) => {
                    let parts: Vec<&str> = s.splitn(2, ' ').collect();
                    if parts.first() == Some(&"--summary") {
                        (true, parts.get(1).map(|d| d.to_string()))
                    } else {
                        (false, Some(s.to_string()))
                    }
                }
                None => (false, None),
            };
            ReplAction::Export { dest, summary }
        }

        "/mcp" => parse_mcp_subcommand(arg),

        _ => ReplAction::NotACommand,
    }
}

/// Parse `/mcp` subcommands.
///
/// Syntax:
/// - `/mcp` or `/mcp list`       — list servers
/// - `/mcp add <name> <cmd> [args...]`  — add a stdio server
/// - `/mcp add-http <name> <url> [--token <t>]` — add an HTTP server
/// - `/mcp remove <name>`        — remove a server
fn parse_mcp_subcommand(arg: Option<&str>) -> ReplAction {
    let arg = match arg {
        Some(a) if !a.is_empty() => a,
        _ => return ReplAction::McpList,
    };

    let mut tokens = arg.split_whitespace();
    let sub = tokens.next().unwrap_or("");

    match sub {
        "list" | "status" => ReplAction::McpList,

        "add" => {
            let name = match tokens.next() {
                Some(n) => n.to_string(),
                None => return ReplAction::McpList, // show usage via list
            };
            let command = match tokens.next() {
                Some(c) => c.to_string(),
                None => return ReplAction::McpList,
            };
            let args: Vec<String> = tokens.map(String::from).collect();
            ReplAction::McpAdd {
                name,
                command,
                args,
            }
        }

        "add-http" | "add_http" => {
            let name = match tokens.next() {
                Some(n) => n.to_string(),
                None => return ReplAction::McpList,
            };
            let url = match tokens.next() {
                Some(u) => u.to_string(),
                None => return ReplAction::McpList,
            };
            // Parse optional --token <value>
            let bearer_token = parse_optional_flag(&mut tokens, "--token");
            ReplAction::McpAddHttp {
                name,
                url,
                bearer_token,
            }
        }

        "remove" | "rm" | "delete" => {
            let name = match tokens.next() {
                Some(n) => n.to_string(),
                None => return ReplAction::McpList,
            };
            ReplAction::McpRemove { name }
        }

        "reconnect" | "retry" | "restart" => {
            let name = match tokens.next() {
                Some(n) => n.to_string(),
                None => return ReplAction::McpList,
            };
            ReplAction::McpReconnect { name }
        }

        _ => ReplAction::McpList,
    }
}

/// Parse `--flag <value>` from remaining tokens. Consumes both tokens if found.
fn parse_optional_flag(tokens: &mut std::str::SplitWhitespace<'_>, flag: &str) -> Option<String> {
    // Peek: if the next token is the flag, consume it and the value.
    // Unfortunately SplitWhitespace doesn't support peek, so we collect remaining.
    // But this is called at the end of parsing so it's fine.
    let remaining: Vec<&str> = tokens.collect();
    let mut i = 0;
    while i < remaining.len() {
        if remaining[i] == flag && i + 1 < remaining.len() {
            return Some(remaining[i + 1].to_string());
        }
        i += 1;
    }
    None
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

    // ── #996 Layer 1: /agents and /cancel ───────────────────────────────────

    /// `/agents` (plural) → runtime task list. Must stay disjoint
    /// from `/agent` (singular) which is the sub-agent definition
    /// picker. Pinning both directions in one test guards against an
    /// accidental refactor that collapses them.
    #[test]
    fn agents_plural_returns_list_background_tasks() {
        assert!(matches!(
            dispatch("/agents"),
            ReplAction::ListBackgroundTasks
        ));
        // Singular still goes the other way.
        assert!(matches!(dispatch("/agent"), ReplAction::ListAgents));
    }

    /// Happy path: `/cancel 7` parses the bare numeric id as an
    /// agent task (back-compat with the original #1042 single-
    /// registry UX). This is the flow the user-facing fix depends
    /// on — if it ever silently returns `None`, `/cancel` is broken
    /// end-to-end.
    #[test]
    fn cancel_with_numeric_id_returns_some_agent() {
        use koda_core::tools::bg_task_tools::TaskId;
        assert!(matches!(
            dispatch("/cancel 7"),
            ReplAction::CancelBackgroundTask(Some(TaskId::Agent(7)))
        ));
        // Sanity: u32::MAX still parses (well within bg-agent
        // task_id space, which only ever increments from 1).
        assert!(matches!(
            dispatch("/cancel 4294967295"),
            ReplAction::CancelBackgroundTask(Some(TaskId::Agent(4_294_967_295)))
        ));
    }

    /// Phase F of #996: prefixed forms route to the correct registry.
    /// `/cancel agent:N` and `/cancel process:N` are the canonical
    /// forms going forward; bare numeric stays for back-compat.
    #[test]
    fn cancel_with_prefixed_id_routes_correctly() {
        use koda_core::tools::bg_task_tools::TaskId;
        assert!(matches!(
            dispatch("/cancel agent:3"),
            ReplAction::CancelBackgroundTask(Some(TaskId::Agent(3)))
        ));
        assert!(matches!(
            dispatch("/cancel process:42"),
            ReplAction::CancelBackgroundTask(Some(TaskId::Process(42)))
        ));
    }

    /// `/cancel` with no arg → `None` so the handler can render a
    /// helpful `Usage:` line. We deliberately do NOT fall through
    /// to `NotACommand` — that would treat `/cancel` as a chat
    /// message, which is the worst possible UX (the user would
    /// then wait for the model to respond to their slash command).
    #[test]
    fn cancel_bare_returns_none_for_handler_to_explain() {
        assert!(matches!(
            dispatch("/cancel"),
            ReplAction::CancelBackgroundTask(None)
        ));
    }

    /// Non-parseable args also land on `None`. Handler shows
    /// `Usage:`. Pinning these cases prevents silent regressions
    /// if someone refactors the parser into
    /// `arg.unwrap_or("0").parse()` (which would give
    /// `Some(TaskId::Agent(0))` for empty input — a very confusing
    /// "task 0 not found").
    #[test]
    fn cancel_with_unparseable_arg_returns_none() {
        assert!(matches!(
            dispatch("/cancel foo"),
            ReplAction::CancelBackgroundTask(None)
        ));
        // Negative numbers are not valid u32 — parser rejects.
        assert!(matches!(
            dispatch("/cancel -5"),
            ReplAction::CancelBackgroundTask(None)
        ));
        // Whitespace-only after the command — don't pass `Some(...)`.
        assert!(matches!(
            dispatch("/cancel    "),
            ReplAction::CancelBackgroundTask(None)
        ));
        // Phase F: malformed prefix forms also land on None.
        assert!(matches!(
            dispatch("/cancel agent:abc"),
            ReplAction::CancelBackgroundTask(None)
        ));
        assert!(matches!(
            dispatch("/cancel process:"),
            ReplAction::CancelBackgroundTask(None)
        ));
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
        assert!(matches!(
            dispatch("/export"),
            ReplAction::Export {
                dest: None,
                summary: false
            }
        ));
        assert!(matches!(
            dispatch("/export koda.md"),
            ReplAction::Export {
                dest: Some(_),
                summary: false
            }
        ));
        assert!(matches!(
            dispatch("/export --summary"),
            ReplAction::Export {
                dest: None,
                summary: true
            }
        ));
        assert!(matches!(
            dispatch("/export --summary notes.md"),
            ReplAction::Export {
                dest: Some(_),
                summary: true
            }
        ));
        // Verify the destination is correct when --summary has a filename.
        if let ReplAction::Export { dest, summary } = dispatch("/export --summary notes.md") {
            assert!(summary);
            assert_eq!(dest.as_deref(), Some("notes.md"));
        }
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

    // ── /mcp parsing ────────────────────────────────────────

    #[test]
    fn mcp_bare_returns_list() {
        assert!(matches!(dispatch("/mcp"), ReplAction::McpList));
    }

    #[test]
    fn mcp_list_subcommand() {
        assert!(matches!(dispatch("/mcp list"), ReplAction::McpList));
        assert!(matches!(dispatch("/mcp status"), ReplAction::McpList));
    }

    #[test]
    fn mcp_add_parses_name_and_command() {
        match dispatch("/mcp add playwright npx -y @anthropic/mcp-playwright") {
            ReplAction::McpAdd {
                name,
                command,
                args,
            } => {
                assert_eq!(name, "playwright");
                assert_eq!(command, "npx");
                assert_eq!(args, vec!["-y", "@anthropic/mcp-playwright"]);
            }
            other => panic!("expected McpAdd, got {other:?}"),
        }
    }

    #[test]
    fn mcp_add_no_args() {
        match dispatch("/mcp add mydb node") {
            ReplAction::McpAdd {
                name,
                command,
                args,
            } => {
                assert_eq!(name, "mydb");
                assert_eq!(command, "node");
                assert!(args.is_empty());
            }
            other => panic!("expected McpAdd, got {other:?}"),
        }
    }

    #[test]
    fn mcp_add_missing_command_shows_list() {
        // Missing command falls back to list (shows usage)
        assert!(matches!(
            dispatch("/mcp add playwright"),
            ReplAction::McpList
        ));
    }

    #[test]
    fn mcp_add_missing_name_shows_list() {
        assert!(matches!(dispatch("/mcp add"), ReplAction::McpList));
    }

    #[test]
    fn mcp_remove_parses_name() {
        match dispatch("/mcp remove playwright") {
            ReplAction::McpRemove { name } => assert_eq!(name, "playwright"),
            other => panic!("expected McpRemove, got {other:?}"),
        }
    }

    #[test]
    fn mcp_remove_aliases() {
        assert!(matches!(
            dispatch("/mcp rm playwright"),
            ReplAction::McpRemove { .. }
        ));
        assert!(matches!(
            dispatch("/mcp delete playwright"),
            ReplAction::McpRemove { .. }
        ));
    }

    #[test]
    fn mcp_unknown_subcommand_shows_list() {
        assert!(matches!(dispatch("/mcp foobar"), ReplAction::McpList));
    }

    // ── /mcp add-http parsing ─────────────────────────────────

    #[test]
    fn mcp_add_http_basic() {
        match dispatch("/mcp add-http myapi http://localhost:8080/mcp") {
            ReplAction::McpAddHttp {
                name,
                url,
                bearer_token,
            } => {
                assert_eq!(name, "myapi");
                assert_eq!(url, "http://localhost:8080/mcp");
                assert!(bearer_token.is_none());
            }
            other => panic!("expected McpAddHttp, got {other:?}"),
        }
    }

    #[test]
    fn mcp_add_http_with_token() {
        match dispatch("/mcp add-http myapi http://localhost:8080/mcp --token secret123") {
            ReplAction::McpAddHttp {
                name,
                url,
                bearer_token,
            } => {
                assert_eq!(name, "myapi");
                assert_eq!(url, "http://localhost:8080/mcp");
                assert_eq!(bearer_token.as_deref(), Some("secret123"));
            }
            other => panic!("expected McpAddHttp, got {other:?}"),
        }
    }

    #[test]
    fn mcp_add_http_underscore_alias() {
        assert!(matches!(
            dispatch("/mcp add_http myapi http://example.com"),
            ReplAction::McpAddHttp { .. }
        ));
    }

    #[test]
    fn mcp_add_http_missing_url_shows_list() {
        assert!(matches!(
            dispatch("/mcp add-http myapi"),
            ReplAction::McpList
        ));
    }

    #[test]
    fn mcp_add_http_missing_name_shows_list() {
        assert!(matches!(dispatch("/mcp add-http"), ReplAction::McpList));
    }

    // ── /mcp reconnect parsing ─────────────────────────────

    #[test]
    fn mcp_reconnect_parses_name() {
        match dispatch("/mcp reconnect playwright") {
            ReplAction::McpReconnect { name } => assert_eq!(name, "playwright"),
            other => panic!("expected McpReconnect, got {other:?}"),
        }
    }

    #[test]
    fn mcp_reconnect_aliases() {
        assert!(matches!(
            dispatch("/mcp retry playwright"),
            ReplAction::McpReconnect { .. }
        ));
        assert!(matches!(
            dispatch("/mcp restart playwright"),
            ReplAction::McpReconnect { .. }
        ));
    }

    #[test]
    fn mcp_reconnect_missing_name_shows_list() {
        assert!(matches!(dispatch("/mcp reconnect"), ReplAction::McpList));
    }
}
