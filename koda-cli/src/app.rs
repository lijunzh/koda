//! CLI argument parsing and application entry point.
//!
//! Consumed by [`crate::run()`] which is the single `pub` surface
//! the binary calls into.

use crate::*;
use koda_tui::tui_app;

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

    /// Images to attach to the first prompt, as Codex's `-i` does: a path,
    /// or several separated by commas. Repeatable.
    #[arg(short = 'i', long = "image", value_name = "FILE", value_delimiter = ',', action = clap::ArgAction::Append)]
    images: Vec<PathBuf>,

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

    /// Run on a local open-weight model, as Codex's `--oss` does. Koda
    /// starts llama.cpp's llama-server with the model (Qwen3.8-Flash-Next
    /// on a 128 GB Mac, else LFM3-1.7B, a 1 GB quick try), and stops it
    /// when Koda exits. `oss_model` in config.toml picks another
    #[arg(long)]
    oss: bool,

    /// The local provider for --oss: llama-cpp (the default, started and
    /// stopped by Koda), lm-studio or ollama (connected to)
    #[arg(long = "local-provider", value_name = "PROVIDER", requires = "oss")]
    local_provider: Option<String>,

    /// Run the session in a new managed Git worktree, as Codex's
    /// `--worktree` does: a detached checkout of the project's HEAD under
    /// Koda's config folder (worktrees/), outside the repository. Kept when
    /// Koda exits; remove it with `git worktree remove <path>`.
    #[arg(long)]
    worktree: bool,
}

impl SessionFlags {
    /// Take what `koda` was given before the subcommand where the
    /// subcommand wasn't, as Codex's `exec` inherits its root options.
    fn inherit(&mut self, root: &Self) {
        inherit(&mut self.resume, &root.resume);
        inherit(&mut self.base_url, &root.base_url);
        inherit(&mut self.model, &root.model);
        inherit(&mut self.provider, &root.provider);
        self.oss |= root.oss;
        inherit(&mut self.local_provider, &root.local_provider);
        self.worktree |= root.worktree;
    }

    /// The flags given, by name.
    fn given(&self) -> Vec<&'static str> {
        [
            ("--resume", self.resume.is_some()),
            ("--base-url", self.base_url.is_some()),
            ("--model", self.model.is_some()),
            ("--provider", self.provider.is_some()),
            ("--oss", self.oss),
            ("--local-provider", self.local_provider.is_some()),
            ("--worktree", self.worktree),
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
    /// or plan.
    /// "auto" auto-approves mutating actions; the kernel sandbox
    /// contains them. Destructive ops (rm -rf, git reset --hard,
    /// git push --force) and outside-project writes still
    /// confirm. Auto requires the kernel sandbox; on unsandboxed
    /// platforms koda refuses to start with an actionable error
    /// (use --mode safe or install the platform sandbox backend).
    /// "safe" confirms every mutation before executing, for when you
    /// want a person to approve each change.
    /// "plan" only looks: reads, searches, read-only commands and MCP
    /// tools marked read-only run; everything else is refused, and the
    /// kernel sandbox keeps the project read-only. In the TUI, Koda then
    /// proposes a plan and asks whether to carry it out.
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
    Exec(Box<ExecArgs>),
    /// Resume a saved session in the TUI, as `codex resume` does: by its
    /// id (or the start of one), the most recent with --last, or pick one
    /// from a list.
    Resume(ResumeArgs),
    /// Fork a saved session and open the copy in the TUI, as `codex fork`
    /// does: by its id (or the start of one), the most recent with --last,
    /// or pick one from a list. The original stays as it was.
    Fork(ResumeArgs),
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

/// `koda resume`'s flags: which session, a prompt, and `koda`'s own.
#[derive(clap::Args, Debug)]
struct ResumeArgs {
    #[command(flatten)]
    which: WhichSession,

    #[command(flatten)]
    session: SessionFlags,

    #[command(flatten)]
    flags: ConfigFlags,

    /// Images to attach to the prompt, as for `koda`.
    #[arg(short = 'i', long = "image", value_name = "FILE", value_delimiter = ',', action = clap::ArgAction::Append)]
    images: Vec<PathBuf>,
}

/// The session `koda resume` and `koda exec resume` open, and the prompt.
#[derive(clap::Args, Debug, Default)]
struct WhichSession {
    /// The session's id, or the start of one. Without it (or --last), the
    /// TUI opens a list to pick from.
    #[arg(value_name = "SESSION")]
    id: Option<String>,

    /// The most recent session in this project. With --last, a single
    /// argument is the prompt, as in Codex.
    #[arg(long)]
    last: bool,

    /// A prompt to send once the session opens.
    #[arg(value_name = "PROMPT")]
    prompt: Option<String>,
}

impl WhichSession {
    /// The session to open, and the prompt. With `--last`, a lone
    /// argument is the prompt, as `codex resume --last "…"` reads it.
    fn resolve(mut self) -> Result<(Option<koda_tui::session_id::Resume>, Option<String>)> {
        use koda_tui::session_id::Resume;
        if self.last {
            if self.prompt.is_some() {
                anyhow::bail!("give a session id or --last, not both");
            }
            return Ok((Some(Resume::Last), self.id.take()));
        }
        Ok((self.id.map(Resume::Id), self.prompt))
    }
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
#[command(args_conflicts_with_subcommands = true)]
struct ExecArgs {
    #[command(subcommand)]
    command: Option<ExecCommand>,

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

    #[command(flatten)]
    output: ExecOutput,
}

/// `koda exec`'s subcommands.
#[derive(Subcommand, Debug)]
enum ExecCommand {
    /// Run a prompt in a saved session, as `codex exec resume` does: by its
    /// id (or the start of one), or the most recent with --last.
    Resume(ExecResumeArgs),
}

/// `koda exec resume`'s flags: `koda exec`'s, and which session.
#[derive(clap::Args, Debug)]
struct ExecResumeArgs {
    #[command(flatten)]
    which: WhichSession,

    #[command(flatten)]
    session: SessionFlags,

    #[command(flatten)]
    flags: ConfigFlags,

    /// Print the client protocol's notifications on stdout, as `koda exec
    /// --json` does.
    #[arg(long)]
    json: bool,

    /// With --json, also print streaming deltas.
    #[arg(long, requires = "json")]
    json_deltas: bool,

    #[command(flatten)]
    output: ExecOutput,
}

/// What `koda exec` and `koda exec resume` both take besides the prompt.
#[derive(clap::Args, Debug, Default)]
struct ExecOutput {
    /// Write the last reply to this file.
    #[arg(short = 'o', long = "output-last-message", value_name = "FILE")]
    output_last_message: Option<PathBuf>,

    /// Images to attach to the prompt, as `codex exec -i` does: a path, or
    /// several separated by commas. Repeatable.
    #[arg(short = 'i', long = "image", value_name = "FILE", value_delimiter = ',', action = clap::ArgAction::Append)]
    images: Vec<PathBuf>,
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
See https://lijunzh.github.io/koda/upgrading-0.7.html#acp-is-gone";

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
    let root_images = std::mem::take(&mut cli.images);
    // The TUI's start: which session, its first prompt and images, and
    // whether to open the session list first (`koda resume` alone).
    let mut tui_prompt = cli.positional_prompt.take();
    let mut tui_images = Vec::new();
    let mut pick_session = false;
    let mut fork = false;
    if matches!(cli.command, Some(Command::Serve(_) | Command::Mcp(_))) && !root_images.is_empty() {
        eprintln!("error: --image goes with a prompt: koda, koda resume or koda exec");
        std::process::exit(2);
    }
    let mut resume = None;
    let headless_run = match cli.command.take() {
        Some(Command::Exec(args)) => match *args {
            ExecArgs {
                command: Some(ExecCommand::Resume(mut r)),
                ..
            } => {
                r.session.inherit(&session);
                r.flags.inherit(&flags);
                refuse_resume_flag(&r.session, "koda exec resume");
                let (which, prompt) = r.which.resolve()?;
                let Some(which) = which else {
                    anyhow::bail!("koda exec resume needs a session id, or --last");
                };
                resume = Some(which);
                session = std::mem::take(&mut r.session);
                flags = std::mem::take(&mut r.flags);
                let mut output = r.output;
                if output.images.is_empty() {
                    output.images = root_images;
                }
                Some(exec_run(prompt, r.json, r.json_deltas, output)?)
            }
            mut args => {
                args.session.inherit(&session);
                args.flags.inherit(&flags);
                session = std::mem::take(&mut args.session);
                flags = std::mem::take(&mut args.flags);
                resume = session.resume.take().map(koda_tui::session_id::Resume::Id);
                let mut output = args.output;
                if output.images.is_empty() {
                    output.images = root_images;
                }
                Some(exec_run(args.prompt, args.json, args.json_deltas, output)?)
            }
        },
        Some(command @ (Command::Resume(_) | Command::Fork(_))) => {
            let mut r = match command {
                Command::Resume(r) => r,
                Command::Fork(r) => {
                    fork = true;
                    r
                }
                _ => unreachable!("matched above"),
            };
            r.session.inherit(&session);
            r.flags.inherit(&flags);
            refuse_resume_flag(&r.session, if fork { "koda fork" } else { "koda resume" });
            let (which, prompt) = r.which.resolve()?;
            pick_session = which.is_none();
            resume = which;
            tui_prompt = prompt.or(tui_prompt);
            session = std::mem::take(&mut r.session);
            flags = std::mem::take(&mut r.flags);
            tui_images = if r.images.is_empty() {
                root_images
            } else {
                r.images
            };
            None
        }
        Some(Command::Serve(mut args)) => {
            if let Some(error) = serve_refusal(&args, &session, tui_prompt.is_some()) {
                eprintln!("error: {error}");
                std::process::exit(2);
            }
            args.flags.inherit(&flags);
            flags = std::mem::take(&mut args.flags);
            serving = true;
            None
        }
        Some(Command::Mcp(mcp)) => {
            if let Some(error) = mcp_refusal(&session, &flags, tui_prompt.is_some()) {
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
            resume = session.resume.take().map(koda_tui::session_id::Resume::Id);
            tui_images = root_images;
            None
        }
    };
    // Read before anything starts, so a bad path stops the run first.
    let tui_images = koda_tui::input::load_images(&tui_images)?;

    // Every flag but the model choice is a config value for this run, as
    // Codex's flags are: after `-c`, so a flag wins over a `-c` value.
    let mut overrides = std::mem::take(&mut cli.config);
    for arg in &flags.mcp_config {
        overrides.extend(koda_config::mcp_config_overrides(arg).map_err(anyhow::Error::msg)?);
    }
    overrides.extend(flag_overrides(&flags));

    // `--oss`: pick the local provider and model now, so the model's effort
    // joins this run's config values before they're fixed.
    let oss_plan = if session.oss {
        if session.provider.is_some() || session.base_url.is_some() {
            eprintln!(
                "error: --oss picks the provider and its address; leave out --provider \
                 and --base-url (use --local-provider to choose llama-cpp, lm-studio or \
                 ollama)"
            );
            std::process::exit(2);
        }
        let config = koda_config::ConfigFile::in_dir(&koda_config::config_dir()?)
            .with_overrides(overrides.clone())
            .load()
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .config;
        let plan = koda_oss::plan(
            session.local_provider.as_deref(),
            config.oss_provider.as_deref(),
            session.model.as_deref(),
            config.oss_model.as_deref(),
            koda_oss::unified_memory(),
        )?;
        if let Some(effort) = plan.model.as_ref().and_then(|m| m.effort)
            && config.model_reasoning_effort.is_none()
        {
            overrides.push(
                koda_config::parse_override(&format!("model_reasoning_effort={effort}"))
                    .map_err(anyhow::Error::msg)?,
            );
        }
        Some((plan, config.oss_check_gpu.unwrap_or(true)))
    } else {
        None
    };
    koda_tui::user_config::set_overrides(overrides);
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
    let user_config = koda_tui::user_config::file()?
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

    // `--worktree`: a new session in a new checkout, as Codex's flag is
    // for new sessions only.
    let worktree = if session.worktree {
        if resume.is_some() || fork || pick_session {
            eprintln!(
                "error: --worktree starts a new session in a new worktree (koda --worktree, \
                 koda exec --worktree); it can't resume or fork one"
            );
            std::process::exit(2);
        }
        let checkout = koda_git::worktree::create(&config_dir.join("worktrees"), &project_root)
            .map_err(|e| anyhow::anyhow!("--worktree: {e}"))?;
        eprintln!(
            "Working in a new worktree: {} (a detached checkout of {} at {})",
            checkout.cwd.display(),
            checkout.source_root.display(),
            &checkout.head_sha[..checkout.head_sha.len().min(10)]
        );
        Some(WorktreeSession::new(checkout))
    } else {
        None
    };
    let project_root = match &worktree {
        Some(worktree) => worktree.checkout.cwd.clone(),
        None => project_root,
    };

    tracing::info!("Koda starting. Project root: {:?}", project_root);

    // `koda mcp`: the stored servers, through a host, then exit.
    if let Some(mcp) = mcp_cli {
        return mcp.run(project_root).await;
    }

    // `koda serve --stdio`: stdout is the protocol, so warnings go to
    // stderr, as `koda exec --json`'s do.
    if serving {
        for warning in koda_tui::retired_env::warnings() {
            eprintln!("warning: {warning}");
        }
        let served = koda_client::stdio::serve(
            koda_client::stdio::ServeArgs {
                config_file: koda_tui::user_config::file()?,
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

    // `--oss`: start the model's server on llama.cpp (or join the one
    // running) before the session opens, and point the session at it. It
    // is stopped when Koda exits, unless another `koda --oss` still uses it.
    let mut oss_server: Option<koda_oss::Server> = None;
    if let Some((plan, check_gpu)) = &oss_plan {
        if let Some(model) = &plan.model {
            for line in koda_oss::start_notice(model, koda_oss::unified_memory()) {
                eprintln!("{line}");
            }
            let server = koda_oss::Server::start(model, &config_dir, *check_gpu).await?;
            session.base_url = Some(server.base_url());
            // The llama-cpp provider sends `KODA_API_KEY` to the server.
            koda_config::runtime_env::set("KODA_API_KEY", server.api_key());
            // The server serves one model; Koda reads its id from it.
            session.model = None;
            // A signal ends the TUI with an exit that runs no destructors;
            // the server is left first.
            koda_tui::set_signal_exit_hook(koda_oss::leave_active);
            oss_server = Some(server);
        }
        session.provider = Some(plan.provider.provider_id().to_string());
    }

    // The model choice goes to `session/start`, as Codex's TUI passes its
    // model to `thread/start`; the host resolves an alias (`claude-haiku`
    // picks Anthropic) and fills in the rest from the saved choice.
    let named = tui_app::Named {
        provider: session.provider.take(),
        base_url: session.base_url.take(),
        model: session.model.take(),
    };

    // Headless mode: skip onboarding, banner, version check
    if let Some(mut run) = headless_run {
        // Stderr, so `--json` stdout stays the protocol.
        for warning in koda_tui::retired_env::warnings() {
            eprintln!("warning: {warning}");
        }
        run.named = headless::Named {
            provider: named.provider,
            base_url: named.base_url,
            model: named.model,
        };
        // An unknown id stops the run before its turn (`session_id`). A new
        // session is the host's to create.
        let on_session = worktree.as_ref().map(WorktreeSession::binder);
        let exit_code = headless::run_headless(project_root, resume, run, on_session).await?;
        if let Some(worktree) = &worktree {
            worktree.kept_notice();
        }
        // `exit` below runs no destructors, so leave the --oss server here.
        if let Some(server) = oss_server.take() {
            server.release();
        }
        // `exit` runs no destructors: store the queued lines first, the last
        // of which say how the run ended, and write out the file's.
        crate::logging::flush().await;
        drop(_logging);
        std::process::exit(exit_code);
    }

    // Interactive mode: full REPL experience
    let version_check = koda_tui::updates::spawn_version_check();

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
    let ran = tui_app::run(
        project_root,
        named,
        koda_tui::tui_app::Start {
            resume,
            prompt: tui_prompt,
            images: tui_images,
            pick_session,
            fork,
            on_session: worktree.as_ref().map(WorktreeSession::binder),
        },
        version_check,
        first_run,
    )
    .await;
    if let Some(worktree) = &worktree {
        worktree.kept_notice();
    }
    ran
}

/// A `--worktree` session's checkout, and what Koda says about it.
struct WorktreeSession {
    checkout: koda_git::worktree::ManagedWorktree,
}

impl WorktreeSession {
    fn new(checkout: koda_git::worktree::ManagedWorktree) -> Self {
        Self { checkout }
    }

    /// Record the session that opens in the checkout, as Codex binds its
    /// thread to the worktree.
    fn binder(&self) -> koda_tui::tui_app::SessionHook {
        let root = self.checkout.root.clone();
        Box::new(move |session_id| {
            if let Err(e) = koda_git::worktree::bind_session(&root, session_id) {
                tracing::warn!(
                    "Couldn't record session {session_id} for {}: {e}",
                    root.display()
                );
            }
        })
    }

    /// Where the checkout stays, and how to remove it. Koda doesn't remove
    /// it, as Codex's CLI doesn't: the work in it is the user's.
    fn kept_notice(&self) {
        eprintln!(
            "The worktree stays at {}. To remove it once you're done with it: \
             git -C {} worktree remove {}",
            self.checkout.root.display(),
            self.checkout.source_root.display(),
            self.checkout.root.display()
        );
    }
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

/// `koda exec`'s run: its prompt, from the argument or stdin, its images,
/// and output.
fn exec_run(
    prompt: Option<String>,
    json: bool,
    json_deltas: bool,
    output: ExecOutput,
) -> Result<headless::Run> {
    let prompt = match prompt.as_deref() {
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
        images: koda_tui::input::load_images(&output.images)?,
        output: if json {
            headless::Output::Events {
                deltas: json_deltas,
            }
        } else {
            headless::Output::Text
        },
        last_message: output.output_last_message,
        // Set once the flags are read.
        named: headless::Named::default(),
    })
}

/// `koda resume` and `koda exec resume` name the session themselves.
fn refuse_resume_flag(session: &SessionFlags, command: &str) {
    if session.resume.is_some() {
        eprintln!("error: {command} takes the session as its argument, not --resume");
        std::process::exit(2);
    }
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

    /// `koda resume` and `koda exec resume` read their arguments as Codex's
    /// do: an id and a prompt, or `--last` and a prompt.
    #[test]
    fn fork_reads_its_session_as_resume_does() {
        let cli = Cli::try_parse_from(["koda", "fork", "--last", "go on"]).unwrap();
        let Some(Command::Fork(r)) = cli.command else {
            panic!("not a fork");
        };
        let (which, prompt) = r.which.resolve().unwrap();
        assert!(matches!(which, Some(koda_tui::session_id::Resume::Last)));
        assert_eq!(prompt.as_deref(), Some("go on"));

        let cli = Cli::try_parse_from(["koda", "fork", "abc", "--model", "m"]).unwrap();
        let Some(Command::Fork(r)) = cli.command else {
            panic!("not a fork");
        };
        assert_eq!(r.session.model.as_deref(), Some("m"));
    }

    #[test]
    fn resume_reads_an_id_or_last_then_a_prompt() {
        use koda_tui::session_id::Resume;
        let which = |args: &[&str]| -> (Option<Resume>, Option<String>) {
            match Cli::try_parse_from(args).unwrap().command {
                Some(Command::Resume(r)) => r.which.resolve().unwrap(),
                Some(Command::Exec(args)) => match *args {
                    ExecArgs {
                        command: Some(ExecCommand::Resume(r)),
                        ..
                    } => r.which.resolve().unwrap(),
                    other => panic!("{other:?}"),
                },
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(which(&["koda", "resume"]), (None, None));
        assert_eq!(
            which(&["koda", "resume", "abc", "go on"]),
            (Some(Resume::Id("abc".into())), Some("go on".into()))
        );
        assert_eq!(
            which(&["koda", "resume", "--last", "go on"]),
            (Some(Resume::Last), Some("go on".into()))
        );
        assert_eq!(
            which(&["koda", "exec", "resume", "--last", "go on", "--json"]),
            (Some(Resume::Last), Some("go on".into()))
        );
        // A prompt that starts with the word is still a prompt.
        let cli = Cli::try_parse_from(["koda", "exec", "resume the work"]).unwrap();
        let Some(Command::Exec(args)) = cli.command else {
            panic!("not exec");
        };
        assert!(args.command.is_none());
    }

    /// `-i` takes a path or several, comma-separated, and repeats.
    #[test]
    fn image_flag_takes_paths_and_lists() {
        let cli =
            Cli::try_parse_from(["koda", "-i", "a.png,b.png", "-i", "c.png", "look"]).unwrap();
        assert_eq!(cli.images.len(), 3);
        assert_eq!(cli.positional_prompt.as_deref(), Some("look"));
        let cli = Cli::try_parse_from(["koda", "exec", "-i", "a.png", "look"]).unwrap();
        let Some(Command::Exec(args)) = cli.command else {
            panic!("not exec");
        };
        assert_eq!(args.output.images.len(), 1);
        assert_eq!(args.prompt.as_deref(), Some("look"));
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
        assert_eq!(config.mode, Some(koda_config::TrustMode::Safe));
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
