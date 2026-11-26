//! CLI argument parsing and application entry point.
//!
//! Consumed by [`crate::run()`] which is the single `pub` surface
//! the binary calls into.

use crate::*;
use koda_tui::tui_app;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

const ABOUT: &str = "Koda, an AI coding agent for your terminal.

Run `koda` to start an interactive session, or `koda exec` to run one prompt and exit.";

const USAGE: &str = "koda [OPTIONS] [PROMPT]
       koda [OPTIONS] <COMMAND> [ARGS]";

const AFTER_HELP: &str = "Examples:
  koda                               Start an interactive session
  koda \"fix the failing test\"        Start one with a first prompt
  koda resume --last                 Continue the most recent session
  koda exec \"explain this codebase\"  Run one prompt and print the reply
  git diff | koda exec               Read the prompt from stdin

Documentation: https://lijunzh.github.io/koda/";

/// The command line: `koda`, its subcommands, and their flags.
#[derive(Parser, Debug)]
#[command(
    name = "koda",
    version,
    about = ABOUT,
    override_usage = USAGE,
    after_help = AFTER_HELP,
    max_term_width = 100,
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

    /// Prompt to start the interactive session with.
    #[arg(value_name = "PROMPT")]
    positional_prompt: Option<String>,

    /// Removed in 0.7.0, with `koda -p`.
    #[arg(long, value_name = "FORMAT", hide = true)]
    output_format: Option<String>,

    #[command(flatten)]
    session: SessionFlags,

    #[command(flatten)]
    flags: ConfigFlags,

    /// Attach images to the first prompt (repeatable, or comma-separated).
    #[arg(short = 'i', long = "image", value_name = "FILE", value_delimiter = ',', action = clap::ArgAction::Append, display_order = 8)]
    images: Vec<PathBuf>,

    /// Set a config.toml value for this run.
    ///
    /// Use a dotted key for a nested value, for example
    /// `-c model_providers.lm-studio.base_url=http://localhost:1234/v1`. The
    /// value is read as TOML, or as plain text if it isn't valid TOML.
    /// Repeatable; nothing is saved. A flag that sets the same value, such as
    /// --model, wins.
    #[arg(
        display_order = 11,
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
/// `session/start` instead.
#[derive(clap::Args, Debug, Default, Clone)]
struct SessionFlags {
    /// Resume a saved session by id (as `koda resume <id>` does).
    #[arg(
        display_order = 15,
        short = 's',
        long = "resume",
        alias = "session",
        value_name = "SESSION"
    )]
    resume: Option<String>,

    /// Provider endpoint, for a local server or a gateway.
    #[arg(long, value_name = "URL", display_order = 3)]
    base_url: Option<String>,

    /// Model to use: an id, or an alias such as `claude-opus`.
    #[arg(short = 'm', long, display_order = 1)]
    model: Option<String>,

    /// Provider: anthropic, openai, gemini, groq, grok, ollama, lm-studio or llama-cpp.
    #[arg(long, display_order = 2)]
    provider: Option<String>,

    /// Run on a local model that Koda starts with llama.cpp.
    ///
    /// The model is Qwen3.8-Flash-Next on a Mac with 128 GB of memory, and
    /// LFM3-1.7B (a 1 GB download, for a quick try) elsewhere; set
    /// `oss_model` in config.toml to choose another. Koda stops the model
    /// server when it exits.
    #[arg(long, display_order = 4)]
    oss: bool,

    /// Local provider for --oss: llama-cpp (the default; Koda starts it), lm-studio or ollama.
    ///
    /// lm-studio and ollama must already be running.
    #[arg(
        long = "local-provider",
        value_name = "PROVIDER",
        requires = "oss",
        display_order = 5
    )]
    local_provider: Option<String>,

    /// Work in a new Git worktree of the project.
    ///
    /// The worktree is a detached checkout of HEAD, outside the repository,
    /// under ~/.config/koda/worktrees/. It stays after Koda exits; remove it
    /// with `git worktree remove <path>`. With `koda fork <id>`, the copy
    /// opens in a new worktree of the repository that session worked in.
    #[arg(long, display_order = 10)]
    worktree: bool,

    /// Run in this process instead of on the background server.
    ///
    /// By default an interactive session runs on `koda serve`, which Koda
    /// starts when needed, so a session goes on after its window closes.
    #[arg(long, conflicts_with = "remote", display_order = 16)]
    no_daemon: bool,

    /// Connect to a background server at this address (unix://PATH).
    ///
    /// Koda doesn't start or replace a server given this way.
    #[arg(long, value_name = "ADDR", display_order = 17)]
    remote: Option<String>,
}

impl SessionFlags {
    /// Take what `koda` was given before the subcommand where the
    /// subcommand wasn't: `koda --model x exec` is `koda exec --model x`.
    fn inherit(&mut self, root: &Self) {
        inherit(&mut self.resume, &root.resume);
        inherit(&mut self.base_url, &root.base_url);
        inherit(&mut self.model, &root.model);
        inherit(&mut self.provider, &root.provider);
        self.oss |= root.oss;
        inherit(&mut self.local_provider, &root.local_provider);
        self.worktree |= root.worktree;
        self.no_daemon |= root.no_daemon;
        inherit(&mut self.remote, &root.remote);
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
            ("--no-daemon", self.no_daemon),
            ("--remote", self.remote.is_some()),
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
    /// Agent to run: a built-in one, or a JSON file in .koda/agents/ or ~/.config/koda/agents/.
    ///
    /// The default is config.toml's `agent`, else `default`.
    #[arg(short, long, display_order = 13)]
    agent: Option<String>,

    /// Directory to work in (default: the current directory).
    #[arg(
        short = 'C',
        long = "cd",
        visible_alias = "project-root",
        value_name = "DIR",
        display_order = 9
    )]
    project_root: Option<PathBuf>,

    /// Deprecated, to go in 0.8: `-c model_max_output_tokens=N`.
    #[arg(long, hide = true)]
    max_tokens: Option<u32>,

    /// Context window size in tokens, for a server that doesn't report one.
    ///
    /// Overrides the agent file, the server's report and Koda's model table.
    #[arg(
        long,
        value_name = "TOKENS",
        value_parser = parse_context_window,
        display_order = 14
    )]
    max_context_tokens: Option<u64>,

    /// Deprecated, to go in 0.8: `-c model_temperature=X`.
    #[arg(long, hide = true)]
    temperature: Option<f64>,

    /// Deprecated, to go in 0.8: `-c model_thinking_budget=N`.
    #[arg(long, hide = true)]
    thinking_budget: Option<u32>,

    /// How much the model reasons: low, medium, high, xhigh or max.
    ///
    /// Claude models take these levels, and each has its own default. For an
    /// OpenAI-compatible server the value is sent as `reasoning_effort`.
    #[arg(
        long,
        visible_alias = "effort",
        value_name = "LEVEL",
        display_order = 6
    )]
    reasoning_effort: Option<String>,

    /// What Koda may do without asking: auto (the default), safe or plan.
    ///
    /// auto: changes run without asking, inside the sandbox. Destructive
    /// commands (such as rm -rf or git push --force) and writes outside the
    /// project still ask. Auto needs the sandbox; where there is none, use
    /// safe.
    ///
    /// safe: every change asks first.
    ///
    /// plan: read only. Koda looks around, proposes a plan, and asks before
    /// carrying it out.
    ///
    /// config.toml's `mode` sets the default.
    #[arg(long, value_parser = ["auto", "safe", "plan"], display_order = 7)]
    mode: Option<String>,

    /// Add MCP servers for this run only, from a JSON file or inline JSON.
    ///
    /// Use the `{"mcpServers": {...}}` format of `.mcp.json` files.
    /// Repeatable; a later server wins a name clash, and these win over
    /// servers added with `koda mcp add`. Nothing is saved.
    #[arg(
        long,
        value_name = "FILE|JSON",
        action = clap::ArgAction::Append,
        display_order = 12
    )]
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
    /// Run one prompt without an interactive session, then exit.
    ///
    /// The reply goes to stdout and progress to stderr. Koda answers its own
    /// approval requests: it allows what --mode allows, refuses destructive
    /// commands and anything outside the sandbox, and leaves a question
    /// unanswered. With --json, stdout carries one JSON-RPC message per line
    /// instead of the reply.
    Exec(Box<ExecArgs>),
    /// Resume a saved session (pick from a list, or --last for the most recent).
    ///
    /// Give a session's id, or its first characters. A prompt after the id is
    /// sent once the session opens.
    Resume(ResumeArgs),
    /// Copy a saved session and continue in the copy (pick from a list, or --last).
    ///
    /// The original session is left unchanged. With --worktree and an id, the
    /// copy opens in a new Git worktree.
    Fork(ResumeArgs),
    /// Manage the MCP servers every session starts.
    ///
    /// Sessions already open pick up a change after `/mcp reconnect`.
    Mcp(crate::mcp_cmd::McpCli),
    /// Run the background server that interactive sessions use.
    ///
    /// By default it listens on a Unix socket, ~/.config/koda/serve/koda.sock,
    /// and serves several clients at once; `koda` starts it when needed. With
    /// --stdio it serves one client on stdin and stdout. Either way the
    /// client speaks JSON-RPC, one message per line.
    ///
    /// The client chooses each session's model, so --model, --provider,
    /// --base-url and --resume aren't accepted here; -c and the other config
    /// flags are.
    Serve(ServeArgs),
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

    /// Attach images to the prompt (repeatable, or comma-separated).
    #[arg(short = 'i', long = "image", value_name = "FILE", value_delimiter = ',', action = clap::ArgAction::Append, display_order = 8)]
    images: Vec<PathBuf>,
}

/// The session `koda resume` and `koda exec resume` open, and the prompt.
#[derive(clap::Args, Debug, Default)]
struct WhichSession {
    /// Session id, or its first characters. Without one (or --last), pick from a list.
    #[arg(value_name = "SESSION")]
    id: Option<String>,

    /// The most recent session in this project.
    ///
    /// With --last, a single argument is the prompt.
    #[arg(long)]
    last: bool,

    /// Prompt to send once the session opens.
    #[arg(value_name = "PROMPT")]
    prompt: Option<String>,
}

impl WhichSession {
    /// The session to open, and the prompt. With `--last`, a lone
    /// argument is the prompt: `koda resume --last "…"`.
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
    /// Serve one client on stdin and stdout.
    #[arg(long, conflicts_with = "listen")]
    stdio: bool,

    /// Where to listen: unix:// (the default socket), unix://PATH, or stdio://.
    ///
    /// The default socket is ~/.config/koda/serve/koda.sock. stdio:// is the
    /// same as --stdio.
    #[arg(long, value_name = "URL")]
    listen: Option<String>,

    /// Stop the background server at the default socket, or the one --listen names.
    ///
    /// Its open sessions close; they stay saved.
    #[arg(long, conflicts_with = "stdio")]
    stop: bool,

    /// Stop once idle (no client, no work) this many seconds. The TUI
    /// starts its server this way.
    #[arg(long, hide = true, value_name = "SECS", conflicts_with = "stdio")]
    idle_exit_secs: Option<u64>,

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

    /// Prompt to run; `-` or none reads it from stdin.
    #[arg(value_name = "PROMPT")]
    prompt: Option<String>,

    /// Print one JSON-RPC message per line on stdout instead of the reply.
    ///
    /// The messages are the session, the turn and its items, the requests
    /// Koda answered and how, and `exec/summary` last.
    #[arg(long)]
    json: bool,

    /// With --json, also print text and command output as it streams.
    #[arg(long, requires = "json")]
    json_deltas: bool,

    #[command(flatten)]
    output: ExecOutput,
}

/// `koda exec`'s subcommands.
#[derive(Subcommand, Debug)]
enum ExecCommand {
    /// Run a prompt in a saved session (by id, or --last for the most recent).
    Resume(ExecResumeArgs),
    /// Copy a saved session and run a prompt in the copy.
    ///
    /// Without a prompt, only copy it and print the copy's id. With
    /// --worktree, the copy runs in a new worktree of the repository the
    /// session worked in.
    Fork(ExecForkArgs),
}

/// `koda exec fork`'s arguments: the session, the prompt, and `koda exec
/// resume`'s flags.
#[derive(clap::Args, Debug)]
struct ExecForkArgs {
    /// Session id, or its first characters.
    #[arg(value_name = "SESSION_ID")]
    session_id: String,

    /// Prompt to run in the copy; `-` reads it from stdin, and none only copies.
    #[arg(value_name = "PROMPT")]
    prompt: Option<String>,

    #[command(flatten)]
    session: SessionFlags,

    #[command(flatten)]
    flags: ConfigFlags,

    /// Print one JSON-RPC message per line on stdout instead of the reply.
    #[arg(long)]
    json: bool,

    /// With --json, also print text and command output as it streams.
    #[arg(long, requires = "json")]
    json_deltas: bool,

    #[command(flatten)]
    output: ExecOutput,
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

    /// Print one JSON-RPC message per line on stdout instead of the reply.
    #[arg(long)]
    json: bool,

    /// With --json, also print text and command output as it streams.
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

    /// A JSON Schema file the last reply must match; the run exits 1 if it
    /// doesn't.
    #[arg(long = "output-schema", value_name = "FILE")]
    output_schema: Option<PathBuf>,

    /// Attach images to the prompt (repeatable, or comma-separated).
    #[arg(short = 'i', long = "image", value_name = "FILE", value_delimiter = ',', action = clap::ArgAction::Append, display_order = 8)]
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
    let mut serving: Option<ServeArgs> = None;
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
            ExecArgs {
                command: Some(ExecCommand::Fork(mut f)),
                ..
            } => {
                f.session.inherit(&session);
                f.flags.inherit(&flags);
                refuse_resume_flag(&f.session, "koda exec fork");
                fork = true;
                resume = Some(koda_tui::session_id::Resume::Id(f.session_id));
                session = std::mem::take(&mut f.session);
                flags = std::mem::take(&mut f.flags);
                let mut output = f.output;
                if output.images.is_empty() {
                    output.images = root_images;
                }
                Some(match f.prompt {
                    // Only the fork, as `codex exec fork` without a prompt.
                    None => {
                        if !output.images.is_empty() || output.output_last_message.is_some() {
                            anyhow::bail!("koda exec fork takes --image and -o only with a prompt");
                        }
                        exec_fork_only(f.json)
                    }
                    prompt => exec_run(prompt, f.json, f.json_deltas, output)?,
                })
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
            serving = Some(args);
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
    // `koda exec` keeps its host in its own process, as Codex's does.
    if headless_run.is_some() && (session.no_daemon || session.remote.is_some()) {
        eprintln!(
            "error: koda exec runs its host in its own process; --no-daemon and --remote \
             are for the TUI"
        );
        std::process::exit(2);
    }
    if let Some(remote) = &session.remote
        && remote.strip_prefix("unix://").is_none_or(str::is_empty)
    {
        eprintln!("error: --remote {remote}: expected unix://PATH");
        std::process::exit(2);
    }
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
    // A run with `-c` values (or flags that are ones) keeps its host in this
    // process, as Codex's TUI does: the shared server's config is its own.
    let run_overrides = !overrides.is_empty();
    koda_tui::user_config::set_overrides(overrides);
    for notice in flags.deprecations() {
        eprintln!("warning: {notice}");
    }

    // Resolve project root
    let project_root_given = flags.project_root.is_some();
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
    // A panic in `koda serve` or `koda exec` goes into the log, as one in
    // the TUI does: a release build aborts on it, and a server the TUI
    // started has nowhere to print it.
    if serving.is_some() || headless_run.is_some() {
        koda_tui::panic_log::install();
    }

    // `--worktree`: a new session, or a fork of one (`koda fork <id>
    // --worktree`), in a new checkout, as Codex's flag is for new sessions
    // and explicit forks only.
    let worktree = if session.worktree {
        if (resume.is_some() || pick_session) && !fork {
            eprintln!(
                "error: --worktree can't resume a session; to carry one into a new worktree, \
                 fork it: koda fork <id> --worktree, or koda exec fork <id> --worktree"
            );
            std::process::exit(2);
        }
        // A fork's checkout is of the repository the session worked in,
        // as Codex takes a forked thread's cwd, unless --project-root
        // names another.
        let source = if fork {
            let Some(koda_tui::session_id::Resume::Id(typed)) = &resume else {
                eprintln!("error: koda fork --worktree needs a session id");
                std::process::exit(2);
            };
            let (id, root) = fork_source(&config_dir, typed, &project_root).await?;
            resume = Some(koda_tui::session_id::Resume::Id(id));
            if project_root_given {
                project_root.clone()
            } else {
                root
            }
        } else {
            project_root.clone()
        };
        let checkout = koda_git::worktree::create(&config_dir.join("worktrees"), &source)
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

    // `koda serve`: on stdio, stdout is the protocol, so warnings go to
    // stderr, as `koda exec --json`'s do; on a socket, clients come and go
    // until a signal stops it.
    if let Some(serve) = serving {
        for warning in koda_tui::retired_env::warnings() {
            eprintln!("warning: {warning}");
        }
        let args = koda_client::stdio::ServeArgs {
            config_file: koda_tui::user_config::file()?,
            project_root,
            home: config_dir.clone(),
        };
        let served = match listen_at(&serve, &config_dir).map_err(anyhow::Error::msg)? {
            Listen::Stdio => koda_client::stdio::serve(
                args,
                tokio::io::stdin(),
                tokio::io::stdout(),
                stop_signal(),
            )
            .await
            .map_err(|e| e.to_string()),
            Listen::Unix(socket) if serve.stop => stop_server(&socket).await,
            Listen::Unix(socket) => {
                eprintln!("Koda is listening at {}", socket.display());
                koda_client::socket::serve(
                    args,
                    socket,
                    serve.idle_exit_secs.map(std::time::Duration::from_secs),
                    stop_signal(),
                )
                .await
                .map_err(|e| e.to_string())
            }
        };
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
            let home = std::env::var_os("HOME").map(PathBuf::from);
            if let Some(line) = koda_oss::download_notice(model, home.as_deref()) {
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
        let exit_code = headless::run_headless(project_root, resume, fork, run, on_session).await?;
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
    let backend = tui_backend(&session, run_overrides, &config_dir)?;
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
            backend,
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

/// The session `typed` names for `koda fork <id> --worktree`, by its full
/// id, and the folder it worked in, read from the session database before
/// the checkout is made. A shorter id is matched against `project_root`'s
/// sessions, as `koda fork` matches one; a full id may be any project's.
async fn fork_source(
    config_dir: &std::path::Path,
    typed: &str,
    project_root: &std::path::Path,
) -> anyhow::Result<(String, std::path::PathBuf)> {
    use koda_state::session_store::SessionStore;
    let db = koda_state::db::Database::init(config_dir).await?;
    let typed = typed.trim();
    let id = if koda_tui::session_id::is_full_id(typed) {
        typed.to_string()
    } else {
        let sessions = db.list_sessions(i64::MAX, project_root).await?;
        koda_tui::session_id::pick(typed, sessions.iter().map(|s| s.id.as_str()))?.to_string()
    };
    let stored = db
        .get_session(&id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no session '{typed}'. Run `koda resume` to pick one."))?;
    let root = stored
        .project_root
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| project_root.to_path_buf());
    anyhow::ensure!(
        root.is_dir(),
        "session {id} worked in {}, which is gone; give -C (--cd) to fork it into a \
         worktree of another checkout",
        root.display()
    );
    Ok((id, root))
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

/// Where `koda serve` listens.
#[derive(Debug, PartialEq, Eq)]
enum Listen {
    /// One client on stdin and stdout.
    Stdio,
    /// The shared server, at this socket.
    Unix(PathBuf),
}

/// What `--stdio` and `--listen` ask for; Koda's socket in `home` when
/// neither says.
fn listen_at(args: &ServeArgs, home: &Path) -> Result<Listen, String> {
    if args.stdio {
        return Ok(Listen::Stdio);
    }
    match args.listen.as_deref() {
        None | Some("unix://") => Ok(Listen::Unix(home.join("serve").join("koda.sock"))),
        Some("stdio://") | Some("stdio") => Ok(Listen::Stdio),
        Some(url) => match url.strip_prefix("unix://") {
            Some(path) => {
                let path = PathBuf::from(path);
                Ok(Listen::Unix(if path.is_absolute() {
                    path
                } else {
                    std::env::current_dir().unwrap_or_default().join(path)
                }))
            }
            None => Err(format!(
                "koda serve --listen {url}: expected `unix://`, `unix://PATH` or `stdio://`"
            )),
        },
    }
}

/// Why `koda serve` can't run as asked: an unknown `--listen`, or a flag
/// that names a session's model or prompt, which the client sends in
/// `session/start` and `turn/start`.
fn serve_refusal(args: &ServeArgs, session: &SessionFlags, prompt: bool) -> Option<String> {
    if let Err(error) = listen_at(args, Path::new("")) {
        return Some(error);
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
             Only -C (--cd) and -c apply."
        )
    })
}

/// Where the TUI's host is, as Codex's TUI picks: Koda's shared server by
/// default; one named with `--remote`; or this process with `--no-daemon`,
/// for `--oss` (its model server and key live in this process) and for a
/// run with `-c` values (the shared server's config is its own).
fn tui_backend(
    session: &SessionFlags,
    run_overrides: bool,
    home: &Path,
) -> anyhow::Result<koda_tui::backend::Backend> {
    use koda_tui::backend::Backend;
    if let Some(remote) = &session.remote {
        let Some(path) = remote.strip_prefix("unix://").filter(|p| !p.is_empty()) else {
            anyhow::bail!("--remote {remote}: expected unix://PATH");
        };
        if session.oss || run_overrides {
            anyhow::bail!(
                "--remote uses that server's config; leave out --oss, -c and the flags \
                 that set config"
            );
        }
        return Ok(Backend::Shared {
            socket: PathBuf::from(path),
            start: false,
        });
    }
    if session.no_daemon || session.oss || run_overrides || cfg!(not(unix)) {
        return Ok(Backend::InProcess);
    }
    Ok(Backend::Shared {
        socket: koda_client::socket::default_socket_path(home),
        start: true,
    })
}

/// `koda serve --stop`: ask the server at `socket` to stop, and wait until
/// it has.
async fn stop_server(socket: &Path) -> Result<(), String> {
    let client = koda_client::InProcessClient::connect(
        socket,
        std::env::current_dir().unwrap_or_default(),
        koda_client::protocol::initialize::ClientInfo {
            name: "koda-serve-stop".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            title: None,
        },
    )
    .await
    .map_err(|_| format!("no koda serve is listening at {}", socket.display()))?;
    let answer = client
        .request::<koda_client::protocol::methods::ServeStop>(
            koda_client::protocol::serve::ServeStopParams {
                if_idle: false,
                ..Default::default()
            },
        )
        .await
        .map_err(|e| e.to_string())?;
    client.shutdown().await;
    if !answer.stopped {
        return Err(answer.reason.unwrap_or_else(|| "it didn't stop".into()));
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while socket.exists() {
        if std::time::Instant::now() > deadline {
            return Err(format!(
                "koda serve at {} is still stopping",
                socket.display()
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    eprintln!("Stopped koda serve at {}", socket.display());
    Ok(())
}

/// Ctrl-C or, on Unix, SIGTERM: what stops `koda serve`.
fn stop_signal() -> impl std::future::Future<Output = ()> + Send {
    // The handlers go in now, not when the future is first polled: a signal
    // that comes once the socket is up but before the server waits would
    // otherwise kill the process.
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let term = signal(SignalKind::terminate()).ok();
        let interrupt = signal(SignalKind::interrupt()).ok();
        async move {
            match (term, interrupt) {
                (Some(mut term), Some(mut interrupt)) => {
                    tokio::select! {
                        _ = interrupt.recv() => {}
                        _ = term.recv() => {}
                    }
                }
                _ => {
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        async {
            let _ = tokio::signal::ctrl_c().await;
        }
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
        output_schema: output
            .output_schema
            .as_deref()
            .map(crate::output_schema::load)
            .transpose()?,
        // Set once the flags are read.
        named: headless::Named::default(),
    })
}

/// `koda exec fork` without a prompt: the fork only, as `codex exec fork`
/// makes one. The copy's id goes to stdout, or `session/started` with
/// --json.
fn exec_fork_only(json: bool) -> headless::Run {
    headless::Run {
        prompt: String::new(),
        images: Vec::new(),
        output: if json {
            headless::Output::Events { deltas: false }
        } else {
            headless::Output::Text
        },
        last_message: None,
        output_schema: None,
        named: headless::Named::default(),
    }
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
