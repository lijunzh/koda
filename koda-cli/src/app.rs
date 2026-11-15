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
    /// ~/.config/koda/agents/ (default: config.toml's `agent`, else
    /// `default`)
    #[arg(short, long, global = true)]
    agent: Option<String>,

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
        value_parser = parse_context_window,
        global = true
    )]
    max_context_tokens: Option<u64>,

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
    /// The value is read as TOML, else taken as text, as Codex's `-c` is,
    /// and replaces what is at that key. Repeatable; never saved. Flags
    /// such as --effort are `-c` values too, and win over these.
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
    /// Serve the client protocol on stdin and stdout, for a GUI, an editor
    /// or a script.
    ///
    /// One JSON-RPC message per line, as `codex app-server` does: the
    /// client drives sessions with requests and answers Koda's approvals.
    /// Runs until stdin closes or a signal stops it.
    ///
    /// The client picks each session's model in `session/start`, so
    /// --model, --provider and --base-url don't apply; -c and the other
    /// config flags do.
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
    let mut mcp_cli = None;
    let mut serving = false;
    let headless_run = match cli.command.take() {
        Some(Command::Exec(args)) => Some(exec_run(args)?),
        Some(Command::Serve(args)) => {
            check_serve(&args, &cli);
            serving = true;
            None
        }
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

    // Every flag but the model choice is a config value for this run, as
    // Codex's flags are: after `-c`, so a flag wins over a `-c` value.
    let mut overrides = std::mem::take(&mut cli.config);
    for arg in &cli.mcp_config {
        overrides.extend(koda_config::mcp_config_overrides(arg).map_err(anyhow::Error::msg)?);
    }
    overrides.extend(flag_overrides(&cli));
    crate::user_config::set_overrides(overrides);

    // Resolve project root
    let project_root = cli
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
        provider: cli.provider.take(),
        base_url: cli.base_url.take(),
        model: cli.model.take(),
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
        let exit_code = headless::run_headless(project_root, cli.session, run).await?;
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
        cli.session,
        cli.positional_prompt,
        version_check,
        first_run,
    )
    .await
}

/// Refuse what `koda serve` can't use: the transport left out, or a flag
/// that names a session's model or prompt, which the client sends in
/// `session/start` and `turn/start`.
fn check_serve(args: &ServeArgs, cli: &Cli) {
    if !args.stdio {
        eprintln!(
            "error: koda serve needs --stdio, the one transport so far: \
             koda serve --stdio"
        );
        std::process::exit(2);
    }
    let named = [
        ("--model", cli.model.is_some()),
        ("--provider", cli.provider.is_some()),
        ("--base-url", cli.base_url.is_some()),
        ("--resume", cli.session.is_some()),
        ("a prompt", cli.positional_prompt.is_some()),
    ];
    if let Some((flag, _)) = named.iter().find(|(_, given)| *given) {
        eprintln!(
            "error: koda serve doesn't take {flag}: the client sends it in \
             session/start or turn/start. Set a default with -c (as in -c model=...)."
        );
        std::process::exit(2);
    }
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

/// The flags that set a config value, as `-c` values: `--effort` is
/// `model_reasoning_effort`, `--max-tokens` is `model_max_output_tokens`,
/// and so on. The host reads them with the file, and checks an effort or a
/// thinking budget against the session's model.
fn flag_overrides(cli: &Cli) -> Vec<koda_config::Override> {
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

    /// The config the flags give, over an empty file.
    fn config_of(args: &[&str]) -> koda_config::ConfigToml {
        let mut cli = Cli::try_parse_from(args).unwrap();
        let mut overrides = std::mem::take(&mut cli.config);
        overrides.extend(flag_overrides(&cli));
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
