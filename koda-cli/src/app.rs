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

    #[command(flatten)]
    session: SessionFlags,

    #[command(flatten)]
    flags: ConfigFlags,

    /// Override a config.toml value for this run: `-c key=value`, with a
    /// dotted key (`-c model_providers.lm-studio.base_url=http://...`).
    /// The value is read as TOML, else taken as text, as Codex's `-c` is,
    /// and replaces what is at that key. Repeatable; never saved. Flags
    /// such as --reasoning-effort are `-c` values too, and win over these.
    #[arg(
        short = 'c',
        long = "config",
        value_name = "KEY=VALUE",
        value_parser = koda_config::parse_override,
        action = clap::ArgAction::Append,
        global = true
    )]
    config: Vec<koda_config::Override>,
}

/// The flags that pick a session: which one, and its model. `koda` and
/// `koda exec` take them; `koda serve`'s client sends them in
/// `session/start`, as Codex's do.
#[derive(clap::Args, Debug, Default, Clone)]
struct SessionFlags {
    /// Session ID to resume (omit to start a new session)
    #[arg(
        short = 's',
        long = "resume",
        alias = "session",
        value_name = "SESSION"
    )]
    resume: Option<String>,

    /// LLM provider base URL override
    #[arg(long)]
    base_url: Option<String>,

    /// Model name override
    #[arg(long)]
    model: Option<String>,

    /// LLM provider (openai, anthropic, lmstudio, llama-cpp, gemini, groq, grok, ollama)
    #[arg(long)]
    provider: Option<String>,
}

impl SessionFlags {
    /// Take what `koda` was given before the subcommand where the
    /// subcommand wasn't, as Codex's `exec` inherits its root options.
    fn inherit(&mut self, root: &Self) {
        inherit(&mut self.resume, &root.resume);
        inherit(&mut self.base_url, &root.base_url);
        inherit(&mut self.model, &root.model);
        inherit(&mut self.provider, &root.provider);
    }

    /// The flags given, by name.
    fn given(&self) -> Vec<&'static str> {
        [
            ("--resume", self.resume.is_some()),
            ("--base-url", self.base_url.is_some()),
            ("--model", self.model.is_some()),
            ("--provider", self.provider.is_some()),
        ]
        .into_iter()
        .filter_map(|(name, given)| given.then_some(name))
        .collect()
    }
}

/// The flags that are config values for this run: `koda`, `koda exec` and
/// `koda serve` take them.
#[derive(clap::Args, Debug, Default, Clone)]
struct ConfigFlags {
    /// Agent to use: a built-in, or a JSON file in .koda/agents/ or
    /// ~/.config/koda/agents/ (default: config.toml's `agent`, else
    /// `default`)
    #[arg(short, long)]
    agent: Option<String>,

    /// Project root directory (defaults to current directory)
    #[arg(long)]
    project_root: Option<PathBuf>,

    /// Deprecated, to go in 0.8: `-c model_max_output_tokens=N`.
    #[arg(long, hide = true)]
    max_tokens: Option<u32>,

    /// Context window in tokens, for servers that don't report theirs.
    /// Beats the agent config, the server's answer and the model table.
    #[arg(long, value_name = "TOKENS", value_parser = parse_context_window)]
    max_context_tokens: Option<u64>,

    /// Deprecated, to go in 0.8: `-c model_temperature=X`.
    #[arg(long, hide = true)]
    temperature: Option<f64>,

    /// Deprecated, to go in 0.8: `-c model_thinking_budget=N`.
    #[arg(long, hide = true)]
    thinking_budget: Option<u32>,

    /// Reasoning effort. Claude: low, medium, high, xhigh or max; by default
    /// the level Claude Code uses for the model. OpenAI-compatible servers:
    /// passed through as `reasoning_effort`.
    #[arg(long, visible_alias = "effort", value_name = "LEVEL")]
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
    #[arg(long, value_parser = ["safe", "auto", "plan"])]
    mode: Option<String>,

    /// Add MCP servers for this run only, from a JSON file or inline JSON
    /// in the `{"mcpServers": {...}}` shape of Claude Code's `.mcp.json`.
    /// Repeatable; later servers win a name clash, and all of them win over
    /// servers stored with `koda mcp add`. Nothing is saved.
    #[arg(long, value_name = "FILE|JSON", action = clap::ArgAction::Append)]
    mcp_config: Vec<String>,
}

impl ConfigFlags {
    /// As [`SessionFlags::inherit`]; MCP servers given before the
    /// subcommand come first, so the subcommand's win a name clash.
    fn inherit(&mut self, root: &Self) {
        inherit(&mut self.agent, &root.agent);
        inherit(&mut self.project_root, &root.project_root);
        inherit(&mut self.max_tokens, &root.max_tokens);
        inherit(&mut self.max_context_tokens, &root.max_context_tokens);
        inherit(&mut self.temperature, &root.temperature);
        inherit(&mut self.thinking_budget, &root.thinking_budget);
        inherit(&mut self.reasoning_effort, &root.reasoning_effort);
        inherit(&mut self.mode, &root.mode);
        let mine = std::mem::take(&mut self.mcp_config);
        self.mcp_config = root.mcp_config.iter().cloned().chain(mine).collect();
    }

    /// The flags given other than `--project-root`, by name: what
    /// `koda mcp` refuses.
    fn given_beyond_project_root(&self) -> Vec<&'static str> {
        [
            ("--agent", self.agent.is_some()),
            ("--max-tokens", self.max_tokens.is_some()),
            ("--max-context-tokens", self.max_context_tokens.is_some()),
            ("--temperature", self.temperature.is_some()),
            ("--thinking-budget", self.thinking_budget.is_some()),
            ("--reasoning-effort", self.reasoning_effort.is_some()),
            ("--mode", self.mode.is_some()),
            ("--mcp-config", !self.mcp_config.is_empty()),
        ]
        .into_iter()
        .filter_map(|(name, given)| given.then_some(name))
        .collect()
    }

    /// Notices for the flags that go in 0.8, with what replaces each.
    fn deprecations(&self) -> Vec<String> {
        [
            (
                "--max-tokens",
                "model_max_output_tokens",
                self.max_tokens.is_some(),
            ),
            (
                "--temperature",
                "model_temperature",
                self.temperature.is_some(),
            ),
            (
                "--thinking-budget",
                "model_thinking_budget",
                self.thinking_budget.is_some(),
            ),
        ]
        .into_iter()
        .filter(|(_, _, given)| *given)
        .map(|(flag, key, _)| format!("{flag} is deprecated and goes in 0.8; use -c {key}=<value>"))
        .collect()
    }
}

fn inherit<T: Clone>(mine: &mut Option<T>, root: &Option<T>) {
    if mine.is_none() {
        mine.clone_from(root);
    }
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
    /// Serve the client protocol on stdin and stdout, for a GUI, an editor
    /// or a script.
    ///
    /// One JSON-RPC message per line, as `codex app-server` does: the
    /// client drives sessions with requests and answers Koda's approvals.
    /// Runs until stdin closes or a signal stops it.
    ///
    /// The client picks each session's model in `session/start`, so
    /// --model, --provider, --base-url and --resume aren't taken here; -c
    /// and the config flags are.
    Serve(ServeArgs),
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

/// `koda serve`'s own flags.
#[derive(clap::Args, Debug)]
struct ServeArgs {
    /// Serve on stdin and stdout, the one transport so far. Required, so
    /// that `koda serve` alone stays free for the shared server.
    #[arg(long)]
    stdio: bool,

    #[command(flatten)]
    flags: ConfigFlags,
}

/// `koda exec`'s flags: its own, and the same session and config flags as
/// `koda`'s.
#[derive(clap::Args, Debug)]
struct ExecArgs {
    #[command(flatten)]
    session: SessionFlags,

    #[command(flatten)]
    flags: ConfigFlags,

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
Run `koda` in a terminal, `koda exec --json <prompt>` from a script, or
`koda serve --stdio` to drive Koda from a program.
See https://lijunzh.github.io/koda/acp.html";

pub(crate) async fn run() -> Result<()> {
    let mut cli = Cli::parse();

    if matches!(cli.command, Some(Command::Server { .. })) {
        eprintln!("{SERVER_REMOVED}");
        std::process::exit(1);
    }

    // A one-shot run is `koda exec`, as in Codex. `koda` alone opens the TUI.
    // Each subcommand takes the flags that apply to it, as Codex's do; a
    // flag given before `exec` carries over to it, and one given before a
    // subcommand that can't use it is refused rather than ignored.
    let mut mcp_cli = None;
    let mut serving = false;
    let mut session = std::mem::take(&mut cli.session);
    let mut flags = std::mem::take(&mut cli.flags);
    let headless_run = match cli.command.take() {
        Some(Command::Exec(mut args)) => {
            args.session.inherit(&session);
            args.flags.inherit(&flags);
            session = std::mem::take(&mut args.session);
            flags = std::mem::take(&mut args.flags);
            Some(exec_run(args)?)
        }
        Some(Command::Serve(mut args)) => {
            if let Some(error) = serve_refusal(&args, &session, cli.positional_prompt.is_some()) {
                eprintln!("error: {error}");
                std::process::exit(2);
            }
            args.flags.inherit(&flags);
            flags = std::mem::take(&mut args.flags);
            serving = true;
            None
        }
        Some(Command::Mcp(mcp)) => {
            if let Some(error) = mcp_refusal(&session, &flags, cli.positional_prompt.is_some()) {
                eprintln!("error: {error}");
                std::process::exit(2);
            }
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

    // Every flag but the model choice is a config value for this run, as
    // Codex's flags are: after `-c`, so a flag wins over a `-c` value.
    let mut overrides = std::mem::take(&mut cli.config);
    for arg in &flags.mcp_config {
        overrides.extend(koda_config::mcp_config_overrides(arg).map_err(anyhow::Error::msg)?);
    }
    overrides.extend(flag_overrides(&flags));
    crate::user_config::set_overrides(overrides);
    for notice in flags.deprecations() {
        eprintln!("warning: {notice}");
    }

    // Resolve project root
    let project_root = flags
        .project_root
        .take()
        .unwrap_or_else(|| std::env::current_dir().expect("Failed to get current directory"));
    let project_root = std::fs::canonicalize(&project_root)?;

    // A first run is one with no config directory yet. Ask before anything
    // below creates it: the log database and the session database both live
    // there, so asking after them (as Koda did since #693) never found a
    // first run and onboarding never opened.
    let first_run = onboarding::is_first_run();

    // The config file, with this run's `-c` values and flags: a broken file
    // or an unknown `-c` key stops the start here, before any terminal
    // setup.
    let user_config = crate::user_config::file()?
        .load()
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Asked for in the TUI, Plan is refused outright rather than turned into
    // Safe, since the person typed it (`--mode plan` or `-c mode=plan`) and
    // can fix the command. `koda exec --mode plan "..."` may only read, as
    // `codex exec`'s read-only sandbox allows. Plan from config.toml becomes
    // Safe in the TUI, with a warning the host sends.
    let run_mode = user_config.flag_value("mode").and_then(|v| v.as_str());
    if headless_run.is_none() && mcp_cli.is_none() && run_mode == Some("plan") {
        eprintln!(
            "error: --mode plan is for one-shot runs (koda exec --mode plan \"...\"). \
             Interactive sessions use --mode safe or --mode auto."
        );
        std::process::exit(1);
    }

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

    // `koda mcp`: the stored servers, through a host, then exit.
    if let Some(mcp) = mcp_cli {
        return mcp.run(project_root).await;
    }

    // `koda serve --stdio`: stdout is the protocol, so warnings go to
    // stderr, as `koda exec --json`'s do.
    if serving {
        for warning in crate::retired_env::warnings() {
            eprintln!("warning: {warning}");
        }
        let served = koda_client::stdio::serve(
            koda_client::stdio::ServeArgs {
                config_file: crate::user_config::file()?,
                project_root,
                home: config_dir,
            },
            tokio::io::stdin(),
            tokio::io::stdout(),
            stop_signal(),
        )
        .await;
        crate::logging::flush().await;
        drop(_logging);
        // `exit`, since a client may hold stdin open after a signal, and
        // Tokio's stdin reader would keep the process waiting for it.
        match served {
            Ok(()) => std::process::exit(0),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }

    // The model choice goes to `session/start`, as Codex's TUI passes its
    // model to `thread/start`; the host resolves an alias (`claude-haiku`
    // picks Anthropic) and fills in the rest from the saved choice.
    let named = tui_context::settings::Named {
        provider: session.provider.take(),
        base_url: session.base_url.take(),
        model: session.model.take(),
    };

    // Headless mode: skip onboarding, banner, version check
    if let Some(mut run) = headless_run {
        // Stderr, so `--json` stdout stays the protocol.
        for warning in crate::retired_env::warnings() {
            eprintln!("warning: {warning}");
        }
        run.named = headless::Named {
            provider: named.provider,
            base_url: named.base_url,
            model: named.model,
        };
        // An unknown id stops the run before its turn (`session_id`). A new
        // session is the host's to create.
        let exit_code = headless::run_headless(project_root, session.resume, run).await?;
        // `exit` runs no destructors: store the queued lines first, the last
        // of which say how the run ended, and write out the file's.
        crate::logging::flush().await;
        drop(_logging);
        std::process::exit(exit_code);
    }

    // Interactive mode: full REPL experience
    let version_check = crate::updates::spawn_version_check();

    // Onboarding asks which provider to use, so a start that already chose
    // one skips it, as Codex shows only the screens its resolved setup still
    // needs: `--provider`, `--base-url` (the host takes the provider from
    // the address) or `-c model_provider`; a model alias picks one too,
    // which the TUI learns once the session opens. A model id alone doesn't
    // say which provider.
    let first_run = first_run
        && named.provider.is_none()
        && named.base_url.is_none()
        && user_config.flag_value("model_provider").is_none();

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
        named,
        session.resume,
        cli.positional_prompt,
        version_check,
        first_run,
    )
    .await
}

/// Why `koda serve` can't run as asked: the transport left out, or a flag
/// that names a session's model or prompt, which the client sends in
/// `session/start` and `turn/start`.
fn serve_refusal(args: &ServeArgs, session: &SessionFlags, prompt: bool) -> Option<String> {
    if !args.stdio {
        return Some(
            "koda serve needs --stdio, the one transport so far: koda serve --stdio".into(),
        );
    }
    let mut given = session.given();
    if prompt {
        given.push("a prompt");
    }
    given.first().map(|flag| {
        format!(
            "koda serve doesn't take {flag}: the client sends it in session/start or \
             turn/start. Set a default with -c (as in -c model=...)."
        )
    })
}

/// Why `koda mcp` refuses a flag given before it: only `--project-root`
/// and `-c` apply to the stored servers.
fn mcp_refusal(session: &SessionFlags, flags: &ConfigFlags, prompt: bool) -> Option<String> {
    let mut given = session.given();
    given.extend(flags.given_beyond_project_root());
    if prompt {
        given.push("a prompt");
    }
    given.first().map(|flag| {
        format!(
            "koda mcp doesn't take {flag}; it manages the stored servers. \
             Only --project-root and -c apply."
        )
    })
}

/// Ctrl-C or, on Unix, SIGTERM: what stops `koda serve`.
async fn stop_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// The flags that set a config value, as `-c` values:
/// `--reasoning-effort` is `model_reasoning_effort`, `--max-tokens` is
/// `model_max_output_tokens`, and so on. The host reads them with the file,
/// and checks an effort or a thinking budget against the session's model.
fn flag_overrides(cli: &ConfigFlags) -> Vec<koda_config::Override> {
    use koda_config::Override;
    let mut overrides = Vec::new();
    if let Some(agent) = &cli.agent {
        overrides.push(Override::new("agent", agent.as_str()));
    }
    if let Some(tokens) = cli.max_tokens {
        overrides.push(Override::new("model_max_output_tokens", i64::from(tokens)));
    }
    if let Some(tokens) = cli.max_context_tokens {
        // At most i64::MAX: TOML's integers are signed.
        let tokens = i64::try_from(tokens).unwrap_or(i64::MAX);
        overrides.push(Override::new("model_context_window", tokens));
    }
    if let Some(temperature) = cli.temperature {
        overrides.push(Override::new("model_temperature", temperature));
    }
    if let Some(budget) = cli.thinking_budget {
        overrides.push(Override::new("model_thinking_budget", i64::from(budget)));
    }
    if let Some(effort) = &cli.reasoning_effort {
        overrides.push(Override::new("model_reasoning_effort", effort.as_str()));
    }
    if let Some(mode) = &cli.mode {
        overrides.push(Override::new("mode", mode.as_str()));
    }
    overrides
}

/// Parse `--max-context-tokens`: a positive whole number, `_` allowed.
/// Anything else is an error, never a silent fallback to the provider's
/// answer or the model table, since a mistyped pin would otherwise go
/// unnoticed.
fn parse_context_window(raw: &str) -> std::result::Result<u64, String> {
    match raw.trim().replace('_', "").parse::<u64>() {
        Ok(0) => Err(format!(
            "context window (--max-context-tokens) must be a positive \
             number of tokens, got '{raw}'"
        )),
        Ok(n) => Ok(n),
        Err(_) => Err(format!(
            "context window (--max-context-tokens) must be a positive \
             whole number of tokens, got '{raw}'"
        )),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A flag given before `exec` carries over to it; one given after wins.
    #[test]
    fn exec_takes_the_flags_given_before_it() {
        let cli = Cli::try_parse_from([
            "koda",
            "--model",
            "root-model",
            "--mode",
            "safe",
            "--mcp-config",
            "a.json",
            "exec",
            "--model",
            "own-model",
            "--mcp-config",
            "b.json",
            "hi",
        ])
        .unwrap();
        let Some(Command::Exec(mut args)) = cli.command else {
            panic!("not exec");
        };
        args.session.inherit(&cli.session);
        args.flags.inherit(&cli.flags);
        assert_eq!(args.session.model.as_deref(), Some("own-model"));
        assert_eq!(args.flags.mode.as_deref(), Some("safe"));
        assert_eq!(args.flags.mcp_config, ["a.json", "b.json"]);
    }

    /// `koda mcp` takes no session or model flag: after it, clap refuses
    /// one; before it, Koda does, rather than ignoring it.
    #[test]
    fn mcp_refuses_flags_it_cannot_use() {
        assert!(Cli::try_parse_from(["koda", "mcp", "list", "--temperature", "2"]).is_err());
        assert!(Cli::try_parse_from(["koda", "mcp", "list", "--model", "x"]).is_err());
        let cli = Cli::try_parse_from(["koda", "--model", "x", "mcp", "list"]).unwrap();
        let error = mcp_refusal(&cli.session, &cli.flags, false).unwrap();
        assert!(error.contains("--model"), "{error}");
        let cli =
            Cli::try_parse_from(["koda", "--project-root", "/tmp", "-c", "a=1", "mcp", "list"])
                .unwrap();
        assert_eq!(mcp_refusal(&cli.session, &cli.flags, false), None);
    }

    /// `koda serve` takes the config flags, but not the ones its client
    /// sends in `session/start`, before or after `serve`.
    #[test]
    fn serve_takes_config_flags_but_not_the_session_s() {
        assert!(Cli::try_parse_from(["koda", "serve", "--stdio", "--model", "x"]).is_err());
        let cli = Cli::try_parse_from(["koda", "serve", "--stdio", "--mode", "safe"]).unwrap();
        let Some(Command::Serve(args)) = &cli.command else {
            panic!("not serve");
        };
        assert_eq!(args.flags.mode.as_deref(), Some("safe"));
        assert_eq!(serve_refusal(args, &cli.session, false), None);
        let cli = Cli::try_parse_from(["koda", "--resume", "abc", "serve", "--stdio"]).unwrap();
        let Some(Command::Serve(args)) = &cli.command else {
            panic!("not serve");
        };
        let error = serve_refusal(args, &cli.session, false).unwrap();
        assert!(error.contains("--resume"), "{error}");
    }

    /// The flags going in 0.8 still set their key, with a warning naming
    /// the `-c` that replaces each.
    #[test]
    fn deprecated_flags_warn_with_their_replacement() {
        let cli =
            Cli::try_parse_from(["koda", "--temperature", "0.5", "--max-tokens", "9"]).unwrap();
        let notices = cli.flags.deprecations();
        assert_eq!(notices.len(), 2);
        assert!(
            notices[0].contains("-c model_max_output_tokens="),
            "{notices:?}"
        );
        assert!(notices[1].contains("-c model_temperature="), "{notices:?}");
        assert!(
            Cli::try_parse_from(["koda", "--effort", "high"])
                .unwrap()
                .flags
                .reasoning_effort
                .is_some()
        );
    }

    /// The config the flags give, over an empty file.
    fn config_of(args: &[&str]) -> koda_config::ConfigToml {
        let mut cli = Cli::try_parse_from(args).unwrap();
        let mut overrides = std::mem::take(&mut cli.config);
        overrides.extend(flag_overrides(&cli.flags));
        let dir = tempfile::tempdir().unwrap();
        koda_config::ConfigFile::in_dir(dir.path())
            .with_overrides(overrides)
            .load()
            .unwrap()
            .config
    }

    #[test]
    fn each_flag_is_its_config_key() {
        let config = config_of(&[
            "koda",
            "--agent",
            "reviewer",
            "--max-tokens",
            "512",
            "--max-context-tokens",
            "65_536",
            "--temperature",
            "0.5",
            "--thinking-budget",
            "2048",
            "--effort",
            "high",
            "--mode",
            "safe",
        ]);
        assert_eq!(config.agent.as_deref(), Some("reviewer"));
        assert_eq!(config.model_max_output_tokens, Some(512));
        assert_eq!(config.model_context_window, Some(65_536));
        assert_eq!(config.model_temperature, Some(0.5));
        assert_eq!(config.model_thinking_budget, Some(2048));
        assert_eq!(config.model_reasoning_effort.as_deref(), Some("high"));
        assert_eq!(config.mode, Some(koda_config::Mode::Safe));
        // Unset flags set nothing.
        assert_eq!(config_of(&["koda"]), koda_config::ConfigToml::default());
    }

    #[test]
    fn a_flag_wins_over_a_c_value() {
        let config = config_of(&[
            "koda",
            "-c",
            "model_reasoning_effort=low",
            "--effort",
            "high",
        ]);
        assert_eq!(config.model_reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn the_context_window_flag_takes_positive_whole_numbers() {
        assert_eq!(parse_context_window("65536"), Ok(65_536));
        assert_eq!(parse_context_window(" 65_536 "), Ok(65_536));
        for bad in [
            "0",
            "",
            "abc",
            "64k",
            "-5",
            "1.5",
            "99999999999999999999999",
        ] {
            let err = parse_context_window(bad).expect_err(bad);
            assert!(err.contains("context window"), "{bad}: {err}");
            assert!(err.contains(&format!("'{bad}'")), "{bad}: {err}");
        }
    }
}
