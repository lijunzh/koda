//! CLI argument parsing and application entry point.
//!
//! Consumed by [`crate::run()`] which is the single `pub` surface
//! the binary calls into.

use crate::*;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

const LONG_ABOUT: &str = "Koda runs in two modes:

  INTERACTIVE   Run `koda` (no arguments) to open the full TUI.
                Type your question and press Enter.
                Type /help inside for keybindings and all commands.

  EXEC          `koda exec` runs one prompt without the TUI and exits.
                For scripts, pipes and CI pipelines.
                  koda exec \"explain this codebase\"
                  git diff | koda exec
                  koda exec --json \"fix the failing tests\"

Configuration precedence (highest wins):
  1. CLI flags     --model, --provider, --base-url, --mode, ...
  2. -c key=value  a config.toml key, for this run only
  3. Config file   ~/.config/koda/config.toml, which /model and
                   /provider write and you can edit
  4. Built-in defaults

API keys (ANTHROPIC_API_KEY, OPENAI_API_KEY, GEMINI_API_KEY, …)
follow the same order. Keys saved with /key are loaded from the
local keystore at startup and injected as env vars — shell env
vars always win over stored keys.";

const AFTER_HELP: &str = "Examples:
  koda                                     # interactive TUI (type /help inside)
  koda \"fix the failing test\"              # the TUI, starting with that prompt
  koda -s abc123                           # resume a saved session
  koda exec \"explain this codebase\"       # one-shot question, then exit
  koda exec - < prompt.txt                 # read the prompt from stdin
  git diff | koda exec                     # pipe a diff as the prompt
  koda exec --json \"fix the failing tests\" # the client protocol on stdout
  koda exec --model o3 \"refactor\"         # one-shot with a specific model
  koda exec -s abc123 \"continue\"          # continue a saved session";

/// Koda 🐻 - A high-performance AI coding agent built in Rust
#[derive(Parser, Debug)]
#[command(
    name = "koda",
    version,
    about,
    long_about = LONG_ABOUT,
    after_help = AFTER_HELP,
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Removed in 0.7.0 (`koda -p`): `koda exec` runs a prompt.
    #[arg(
        short,
        long,
        value_name = "PROMPT",
        hide = true,
        allow_hyphen_values = true
    )]
    prompt: Option<String>,

    /// Open the TUI and send this prompt, as `codex "prompt"` does. A
    /// one-shot run is `koda exec "prompt"`.
    #[arg(value_name = "PROMPT")]
    positional_prompt: Option<String>,

    /// Removed in 0.7.0, with `koda -p`.
    #[arg(long, value_name = "FORMAT", hide = true)]
    output_format: Option<String>,

    /// Agent to use: a built-in, or a JSON file in .koda/agents/ or
    /// ~/.config/koda/agents/
    #[arg(short, long, default_value = "default", global = true)]
    agent: String,

    /// Session ID to resume (omit to start a new session)
    #[arg(short, long = "resume", alias = "session", global = true)]
    session: Option<String>,

    /// Project root directory (defaults to current directory)
    #[arg(long, global = true)]
    project_root: Option<PathBuf>,

    /// LLM provider base URL override
    #[arg(long, global = true)]
    base_url: Option<String>,

    /// Model name override
    #[arg(long, global = true)]
    model: Option<String>,

    /// LLM provider (openai, anthropic, lmstudio, llama-cpp, gemini, groq, grok, ollama)
    #[arg(long, global = true)]
    provider: Option<String>,

    /// Maximum output tokens
    #[arg(long, global = true)]
    max_tokens: Option<u32>,

    /// Context window in tokens, for servers that don't report theirs.
    /// Beats the agent config, the server's answer and the model table.
    #[arg(
        long,
        value_name = "TOKENS",
        value_parser = koda_core::config::parse_max_context_tokens
    , global = true)]
    max_context_tokens: Option<usize>,

    /// Sampling temperature (0.0 - 2.0)
    #[arg(long, global = true)]
    temperature: Option<f64>,

    /// Gemini thinking budget in tokens. Claude models take --effort instead.
    #[arg(long, global = true)]
    thinking_budget: Option<u32>,

    /// Reasoning effort. Claude: low, medium, high, xhigh or max; by default
    /// the level Claude Code uses for the model. OpenAI-compatible servers:
    /// passed through as `reasoning_effort`.
    #[arg(long, alias = "effort", value_name = "LEVEL", global = true)]
    reasoning_effort: Option<String>,

    /// Trust mode: auto (the default, unless config.toml sets `mode`), safe,
    /// or plan (`koda exec` only).
    /// "auto" auto-approves mutating actions; the kernel sandbox
    /// contains them. Destructive ops (rm -rf, git reset --hard,
    /// git push --force) and outside-project writes still
    /// confirm. Auto requires the kernel sandbox; on unsandboxed
    /// platforms koda refuses to start with an actionable error
    /// (use --mode safe or install the platform sandbox backend).
    /// "safe" confirms every mutation before executing — use this
    /// in CI, locked-down workstations, or any context where you
    /// want a human in every approval loop.
    /// "plan" is read-only, for a `koda exec` run that should only
    /// look: reads, searches, read-only commands and MCP tools marked
    /// read-only run; everything else is refused, and the kernel
    /// sandbox keeps the project read-only.
    /// Sandbox with credential protection is always active.
    #[arg(long, value_parser = ["safe", "auto", "plan"], global = true)]
    mode: Option<String>,

    /// Override a config.toml value for this run: `-c key=value`, with a
    /// dotted key (`-c model_providers.lm-studio.base_url=http://...`).
    /// The value is read as TOML, else taken as text, as Codex's `-c` is.
    /// Repeatable; never saved. Flags such as --model win over it.
    #[arg(
        short = 'c',
        long = "config",
        value_name = "KEY=VALUE",
        value_parser = koda_config::parse_override,
        action = clap::ArgAction::Append,
        global = true
    )]
    config: Vec<koda_config::Override>,

    /// Add MCP servers for this run only, from a JSON file or inline JSON
    /// in the `{"mcpServers": {...}}` shape of Claude Code's `.mcp.json`.
    /// Repeatable; later servers win a name clash, and all of them win over
    /// servers stored with `koda mcp add`. Nothing is saved.
    #[arg(long, value_name = "FILE|JSON", action = clap::ArgAction::Append, global = true)]
    mcp_config: Vec<String>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run one prompt without the TUI, then exit.
    ///
    /// Koda answers its own requests: it approves what --mode allows,
    /// refuses destructive actions and anything outside the sandbox, and
    /// answers a question with nothing. The reply goes to stdout, progress
    /// to stderr. With --json, stdout carries the client protocol's
    /// notifications instead, one JSON-RPC message per line.
    Exec(ExecArgs),
    /// Manage the MCP servers every session starts: list, get, add, remove.
    ///
    /// Open TUI sessions take a change on `/mcp reconnect`.
    Mcp(crate::mcp_cmd::McpCli),
    // Without this, `server` would be read as a prompt and sent to the model.
    /// Removed: Koda no longer has a server mode
    #[command(hide = true)]
    Server {
        #[arg(hide = true, trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

/// `koda exec`'s own flags. The model, provider, mode and the rest are the
/// same flags as `koda`'s.
#[derive(clap::Args, Debug)]
struct ExecArgs {
    /// The prompt. "-" or none reads it from stdin.
    #[arg(value_name = "PROMPT")]
    prompt: Option<String>,

    /// Print the client protocol's notifications on stdout, one JSON-RPC
    /// message per line: the session, the turn and its items, requests and
    /// how Koda answered them, and `exec/summary` last.
    #[arg(long)]
    json: bool,

    /// With --json, also print streaming deltas: reply and reasoning text
    /// as it arrives, and command output line by line.
    #[arg(long, requires = "json")]
    json_deltas: bool,

    /// Write the last reply to this file.
    #[arg(short = 'o', long = "output-last-message", value_name = "FILE")]
    output_last_message: Option<PathBuf>,
}

const ONE_SHOT_REMOVED: &str = "`koda -p` and `--output-format` were removed in 0.7.0.
Run a prompt with `koda exec \"<prompt>\"`, or `koda exec --json` for the client \
protocol's notifications, one JSON-RPC message per line (`exec/summary` last).
See https://lijunzh.github.io/koda/headless.html";

const STDIN_NOT_A_TERMINAL: &str = "stdin is not a terminal. To run a piped prompt, use \
`koda exec`, as in `git diff | koda exec`.";

const SERVER_REMOVED: &str = "`koda server` has been removed. Koda no longer speaks ACP, \
so editors can't connect to it.
Run `koda` in a terminal, or `koda exec --json <prompt>` from a script.
See https://lijunzh.github.io/koda/acp.html";

pub(crate) async fn run() -> Result<()> {
    let mut cli = Cli::parse();
    crate::user_config::set_overrides(cli.config.clone());

    if matches!(cli.command, Some(Command::Server { .. })) {
        eprintln!("{SERVER_REMOVED}");
        std::process::exit(1);
    }

    // A one-shot run is `koda exec`, as in Codex. `koda` alone opens the TUI.
    let mut mcp_cli = None;
    let headless_run = match cli.command.take() {
        Some(Command::Exec(args)) => Some(exec_run(args)?),
        Some(Command::Mcp(mcp)) => {
            mcp_cli = Some(mcp);
            None
        }
        _ => {
            if cli.prompt.is_some() || cli.output_format.is_some() {
                eprintln!("{ONE_SHOT_REMOVED}");
                std::process::exit(2);
            }
            None
        }
    };

    // Resolve project root
    let project_root = cli
        .project_root
        .unwrap_or_else(|| std::env::current_dir().expect("Failed to get current directory"));
    let project_root = std::fs::canonicalize(&project_root)?;

    // A first run is one with no config directory yet. Ask before anything
    // below creates it: the log database and the session database both live
    // there, so asking after them (as Koda did since #693) never found a
    // first run and onboarding never opened.
    let first_run = onboarding::is_first_run();

    // The config file, with this run's `-c` values: a broken file or an
    // unknown `-c` key stops the start here, before any terminal setup.
    let user_config = crate::user_config::file()?
        .load()
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Logs go to the log database, and to `<log_dir>/koda.log` when the
    // config file sets `log_dir`, as Codex's do (see `logging`).
    let config_dir = koda_config::config_dir()?;
    let log_dir = user_config
        .config
        .log_dir
        .as_deref()
        .map(|dir| crate::logging::resolve_log_dir(dir, &config_dir));
    let _logging = crate::logging::init(&config_dir, log_dir).await?;

    tracing::info!("Koda starting. Project root: {:?}", project_root);

    // The mode: --mode, else the file's (or `-c mode=`), else auto. Resolved
    // here because Auto needs the sandbox, which is checked before start.
    let mode = cli
        .mode
        .clone()
        .or_else(|| user_config.config.mode.map(|m| m.as_str().to_string()))
        .unwrap_or_else(|| "auto".to_string());

    // Stored API keys still come from the database here; they move into
    // the host's start in 8d.
    let db = koda_core::db::Database::init(&config_dir).await?;

    // `koda mcp`: the stored servers, through a host, then exit.
    if let Some(mcp) = mcp_cli {
        let config = koda_core::config::KodaConfig::load(&project_root, &cli.agent)?;
        return mcp.run(config, project_root).await;
    }

    // Load and inject stored API keys (env vars take precedence)
    if let Err(e) = koda_core::keystore::inject_into_env(&db).await {
        tracing::warn!("Failed to load keystore: {e}");
    }

    // `--model claude-haiku` names an alias, as `/model` lists them. Taken
    // after the keystore so a Claude alias can ask for the newest id.
    let alias = match cli.model.as_deref() {
        Some(name) => {
            koda_core::model_alias::resolve_for_flag(
                name,
                cli.provider.as_deref(),
                cli.base_url.as_deref(),
            )
            .await
        }
        None => None,
    };
    if alias.is_some() {
        cli.model = None;
    }
    let with_alias = |config: koda_core::config::KodaConfig| match &alias {
        Some(a) => config.with_alias(a),
        None => config,
    };

    // `--mcp-config` servers, for this run only. A bad value stops the start:
    // the caller asked for these servers by name.
    let mut session_mcp = std::collections::HashMap::new();
    for arg in &cli.mcp_config {
        session_mcp.extend(koda_core::mcp::config::parse_mcp_config_arg(arg)?);
    }

    // Headless mode: skip onboarding, banner, version check
    if let Some(mut run) = headless_run {
        // Stderr, so `--json` stdout stays the protocol.
        for warning in crate::retired_env::warnings() {
            eprintln!("warning: {warning}");
        }
        run.named = headless::Named {
            // The catalog's own id, which the host reads (`lmstudio` is
            // `lm-studio`).
            provider: cli.provider.as_deref().map(|p| {
                koda_core::config::ProviderType::from_url_or_name("", Some(p)).to_string()
            }),
            base_url: cli.base_url.clone(),
            model: alias
                .as_ref()
                .map(|a| a.alias.to_string())
                .or_else(|| cli.model.clone()),
        };
        let config = koda_core::config::KodaConfig::load(&project_root, &cli.agent)?;
        let typed = (cli.reasoning_effort.clone(), cli.thinking_budget);
        let config = with_alias(config.with_overrides(cli.base_url, cli.model, cli.provider))
            .with_model_overrides(
                cli.max_tokens,
                cli.temperature,
                cli.thinking_budget,
                cli.reasoning_effort,
            )
            .with_context_override(cli.max_context_tokens)
            .with_trust(parse_top_level_trust_mode(&mode, true))
            .with_session_mcp_servers(session_mcp)
            .with_trust_named();
        check_typed_reasoning(typed.0.as_deref(), typed.1, &config)?;
        // An unknown id stops the run before its turn (`session_id`). A new
        // session is the host's to create.
        let exit_code = headless::run_headless(project_root, config, cli.session, run).await?;
        // `exit` runs no destructors: store the queued lines first, the last
        // of which say how the run ended, and write out the file's.
        crate::logging::flush().await;
        drop(_logging);
        std::process::exit(exit_code);
    }

    // Interactive mode: full REPL experience
    let version_check = koda_core::version::spawn_version_check();
    // Ask Anthropic for its newest models now, so the startup can say when
    // one is out that Koda doesn't support yet. The TUI joins this same
    // request (`model_alias` asks once per process); without a key it's a no-op.
    tokio::spawn(koda_core::model_alias::ensure_live_claude_ids());

    // Load configuration. The TUI also restores the last-used provider from
    // the DB, but only for the fields these flags leave unset.
    // An alias picks the provider as well as the model.
    let overrides = tui_context::ProviderOverrides {
        provider: cli.provider.is_some() || alias.is_some(),
        base_url: cli.base_url.is_some(),
        model: cli.model.is_some() || alias.is_some(),
    };
    let config = koda_core::config::KodaConfig::load(&project_root, &cli.agent)?;
    let typed = (cli.reasoning_effort.clone(), cli.thinking_budget);
    let config = with_alias(config.with_overrides(cli.base_url, cli.model, cli.provider))
        .with_model_overrides(
            cli.max_tokens,
            cli.temperature,
            cli.thinking_budget,
            cli.reasoning_effort,
        )
        .with_context_override(cli.max_context_tokens)
        .with_trust(parse_top_level_trust_mode(&mode, false))
        .with_session_mcp_servers(session_mcp)
        .with_trust_named();
    check_typed_reasoning(typed.0.as_deref(), typed.1, &config)?;

    // Onboarding asks which provider to use. A start that names one already
    // skips it, as Codex skips the screens a start doesn't need.
    let first_run = first_run && !overrides.provider;

    // Initialize database is already done above

    // The TUI needs a terminal; a piped prompt is `koda exec`'s. Checked
    // after the flags, so a bad one is reported first, and before a session
    // is created for nothing.
    if !atty_is_terminal() {
        eprintln!("{STDIN_NOT_A_TERMINAL}");
        std::process::exit(1);
    }

    // Run the main event loop
    tui_app::run(
        project_root,
        config,
        overrides,
        cli.session,
        cli.positional_prompt,
        version_check,
        first_run,
    )
    .await
}

/// Refuse an effort or thinking budget typed for this run that the Claude
/// model doesn't take. A person can fix the command; a setting from an agent
/// config is corrected at request time instead (`koda_core::model_effort`).
fn check_typed_reasoning(
    effort: Option<&str>,
    thinking_budget: Option<u32>,
    config: &koda_core::config::KodaConfig,
) -> Result<()> {
    if config.provider_type != koda_core::config::ProviderType::Anthropic {
        return Ok(());
    }
    let model = &config.model_settings.model;
    if let Some(level) = effort {
        koda_core::model_effort::check_typed(model, level).map_err(anyhow::Error::msg)?;
    }
    // Current Claude models take effort, not a token budget; the flag is
    // for Gemini.
    if thinking_budget.is_some() {
        anyhow::bail!(
            "{model} takes no --thinking-budget. Use --effort with one of: {}.",
            koda_core::model_effort::LEVELS.join(", ")
        );
    }
    Ok(())
}

/// The `--mode` asked for, as the config the host starts sessions from.
/// The host enforces the rules whichever client asks (Auto needs the
/// sandbox, with the setup hint in its refusal; a session a person answers
/// runs in Safe rather than Plan). Only the flag itself is checked here:
/// asked for in an interactive session, Plan is refused outright rather
/// than turned into Safe, since the person typed it and can fix the
/// command. A headless run keeps Plan: `koda exec --mode plan "..."` may
/// only read, as `codex exec`'s read-only sandbox allows.
fn parse_top_level_trust_mode(cli_mode: &str, headless: bool) -> koda_core::trust::TrustMode {
    let parsed = koda_core::trust::TrustMode::parse(cli_mode).unwrap_or_default();
    if parsed == koda_core::trust::TrustMode::Plan && !headless {
        eprintln!(
            "error: --mode plan is for one-shot runs (koda exec --mode plan \"...\"). \
             Interactive sessions use --mode safe or --mode auto."
        );
        std::process::exit(1);
    }
    parsed
}

/// `koda exec`'s run: its prompt, from the argument or stdin, and output.
fn exec_run(args: ExecArgs) -> Result<headless::Run> {
    let prompt = match args.prompt.as_deref() {
        Some("-") => read_stdin()?,
        Some(prompt) => prompt.to_string(),
        None if !atty_is_terminal() => read_stdin()?,
        None => String::new(),
    };
    if prompt.trim().is_empty() {
        anyhow::bail!("koda exec needs a prompt: as an argument, or on stdin");
    }
    Ok(headless::Run {
        prompt,
        output: if args.json {
            headless::Output::Events {
                deltas: args.json_deltas,
            }
        } else {
            headless::Output::Text
        },
        last_message: args.output_last_message,
        // Set once the flags are read.
        named: headless::Named::default(),
    })
}

fn read_stdin() -> Result<String> {
    use std::io::Read;
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("Failed to read from stdin")?;
    Ok(input.trim().to_string())
}

/// Check if stdin is a terminal (not piped).
fn atty_is_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}
