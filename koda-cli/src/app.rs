//! CLI argument parsing and application entry point.
//!
//! Consumed by [`crate::run()`] which is the single `pub` surface
//! the binary calls into.

use crate::*;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use koda_core::persistence::Persistence;
use std::path::{Path, PathBuf};

const LONG_ABOUT: &str = "Koda runs in two modes:

  INTERACTIVE   Run `koda` (no arguments) to open the full TUI.
                Type your question and press Enter.
                Type /help inside for keybindings and all commands.

  HEADLESS      Pass a prompt to get a single answer and exit.
                Great for scripts, pipes, and CI pipelines.
                  koda \"explain this codebase\"
                  git diff | koda
                  koda -p - < prompt.txt

Configuration precedence (highest wins):
  1. CLI flags     --model, --provider, --base-url
  2. Env vars      KODA_MODEL, KODA_PROVIDER, KODA_BASE_URL
  3. Saved config  set interactively with /model, /provider, /key
  4. Built-in defaults

API keys (ANTHROPIC_API_KEY, OPENAI_API_KEY, GEMINI_API_KEY, …)
follow the same order. Keys saved with /key are loaded from the
local keystore at startup and injected as env vars — shell env
vars always win over stored keys.";

const AFTER_HELP: &str = "Examples:
  koda                                # interactive TUI (type /help inside)
  koda \"explain this codebase\"       # one-shot question, then exit
  koda -p \"fix the failing tests\"    # same, explicit flag form
  koda -p -                           # read prompt from stdin
  git diff | koda                     # pipe diff as the prompt
  koda \"refactor\" --model o3         # one-shot with a specific model
  KODA_MODEL=gemini-flash koda \"...\" # env-var model override
  koda server --stdio                 # ACP stdio server for editor plugins
  koda -s abc123 \"continue\"          # resume a saved session";

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

    /// Run a single prompt and exit (headless mode).
    /// Use "-" to read from stdin.
    #[arg(short, long, value_name = "PROMPT")]
    prompt: Option<String>,

    /// Positional prompt (alternative to -p).
    /// `koda "fix the bug"` is equivalent to `koda -p "fix the bug"`.
    #[arg(value_name = "PROMPT", conflicts_with = "prompt")]
    positional_prompt: Option<String>,

    /// Output format for headless mode.
    #[arg(long, default_value = "text", value_parser = ["text", "json"])]
    output_format: String,

    /// Agent to use (matches a JSON file in agents/)
    #[arg(short, long, default_value = "default")]
    agent: String,

    /// Session ID to resume (omit to start a new session)
    #[arg(short, long = "resume", alias = "session")]
    session: Option<String>,

    /// Project root directory (defaults to current directory)
    #[arg(long)]
    project_root: Option<PathBuf>,

    /// LLM provider base URL override
    #[arg(long, env = "KODA_BASE_URL")]
    base_url: Option<String>,

    /// Model name override
    #[arg(long, env = "KODA_MODEL")]
    model: Option<String>,

    /// LLM provider (openai, anthropic, lmstudio, gemini, groq, grok, ollama)
    #[arg(long, env = "KODA_PROVIDER")]
    provider: Option<String>,

    /// Maximum output tokens
    #[arg(long)]
    max_tokens: Option<u32>,

    /// Sampling temperature (0.0 - 2.0)
    #[arg(long)]
    temperature: Option<f64>,

    /// Anthropic extended thinking budget (tokens)
    #[arg(long)]
    thinking_budget: Option<u32>,

    /// OpenAI reasoning effort (low, medium, high)
    #[arg(long)]
    reasoning_effort: Option<String>,

    /// Trust mode: safe (default) or auto.
    /// "safe" confirms every side effect before executing.
    /// "auto" auto-approves all actions within the project sandbox.
    /// Sandbox with credential protection is always active.
    #[arg(long, env = "KODA_MODE", default_value = "safe",
          value_parser = ["safe", "auto"])]
    mode: String,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start an ACP (Agent Client Protocol) server for editor/client integrations
    Server {
        /// WebSocket port (not yet implemented — use --stdio instead)
        #[arg(long, default_value = "9999")]
        port: u16,
        /// Use stdin/stdout JSON-RPC transport (required for VS Code, Zed, etc.)
        ///
        /// Reads newline-delimited JSON-RPC 2.0 from stdin and writes to stdout.
        /// This is the standard ACP transport for local agent integrations.
        #[arg(long)]
        stdio: bool,
    },
    /// Connect to a running Koda server (not yet implemented)
    Connect { url: String },
}

pub(crate) async fn run() -> Result<()> {
    let cli = Cli::parse();

    // Handle subcommands first
    if let Some(cmd) = &cli.command {
        match cmd {
            Command::Server { port, stdio } => {
                if *stdio {
                    let project_root = cli.project_root.clone().unwrap_or_else(|| {
                        std::env::current_dir().expect("Failed to get current directory")
                    });
                    let project_root = std::fs::canonicalize(&project_root)?;

                    // Init DB and inject keys (#693)
                    let db = koda_core::db::Database::init(&koda_core::db::config_dir()?).await?;
                    if let Err(e) = koda_core::keystore::inject_into_env(&db).await {
                        tracing::warn!("Failed to load keystore: {e}");
                    }

                    let config = koda_core::config::KodaConfig::load(&project_root, &cli.agent)?;
                    let config = config
                        .with_overrides(
                            cli.base_url.clone(),
                            cli.model.clone(),
                            cli.provider.clone(),
                        )
                        .with_model_overrides(
                            cli.max_tokens,
                            cli.temperature,
                            cli.thinking_budget,
                            cli.reasoning_effort.clone(),
                        )
                        .with_trust(
                            koda_core::trust::TrustMode::parse(&cli.mode).unwrap_or_default(),
                        );
                    server::run_stdio_server(project_root, config).await?;
                } else {
                    eprintln!("WebSocket server (--port {port}) not yet implemented. Use --stdio.");
                    std::process::exit(1);
                }
                return Ok(());
            }
            Command::Connect { url } => {
                println!("Not implemented: Connect to {}", url);
                std::process::exit(0);
            }
        }
    }

    // Resolve headless prompt: -p flag, positional arg, or stdin
    let headless_prompt = resolve_headless_prompt(&cli)?;

    // Resolve project root
    let project_root = cli
        .project_root
        .unwrap_or_else(|| std::env::current_dir().expect("Failed to get current directory"));
    let project_root = std::fs::canonicalize(&project_root)?;

    // Initialize logging to a per-process file (invisible to user).
    //
    // Each koda invocation gets its own log file named koda-<PID>.log.
    // A 'latest' symlink is updated to point to it so users can always
    // tail the current session without knowing the filename:
    //   tail -f ~/.config/koda/logs/latest
    let log_dir = koda_core::db::config_dir()?.join("logs");
    std::fs::create_dir_all(&log_dir)?;
    prune_old_logs(&log_dir, 50);
    let log_filename = format!("koda-{}.log", std::process::id());
    let file_appender = tracing_appender::rolling::never(&log_dir, &log_filename);
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    // Best-effort symlink — silently skip on non-unix or permission errors.
    #[cfg(unix)]
    {
        let latest = log_dir.join("latest");
        let _ = std::fs::remove_file(&latest);
        let _ = std::os::unix::fs::symlink(log_dir.join(&log_filename), &latest);
    }
    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("koda_core=info,koda_cli=info")
            }),
        )
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .init();

    tracing::info!("Koda starting. Project root: {:?}", project_root);

    // Initialize database early — keys and last-provider recall live here (#693)
    let db = koda_core::db::Database::init(&koda_core::db::config_dir()?).await?;

    // Load and inject stored API keys (env vars take precedence)
    if let Err(e) = koda_core::keystore::inject_into_env(&db).await {
        tracing::warn!("Failed to load keystore: {e}");
    }

    // Headless mode: skip onboarding, banner, version check
    if let Some(prompt) = headless_prompt {
        let config = koda_core::config::KodaConfig::load(&project_root, &cli.agent)?;
        let config = config
            .with_overrides(cli.base_url, cli.model, cli.provider)
            .with_model_overrides(
                cli.max_tokens,
                cli.temperature,
                cli.thinking_budget,
                cli.reasoning_effort,
            )
            .with_trust(koda_core::trust::TrustMode::parse(&cli.mode).unwrap_or_default());
        let session_id = match cli.session {
            Some(id) => id,
            None => db.create_session(&config.agent_name, &project_root).await?,
        };
        let exit_code = headless::run_headless(
            project_root,
            config,
            db,
            session_id,
            prompt,
            &cli.output_format,
        )
        .await?;
        std::process::exit(exit_code);
    }

    // Interactive mode: full REPL experience
    let version_check = koda_core::version::spawn_version_check();

    // Detect first run (onboarding happens inside the TUI now)
    let first_run = onboarding::is_first_run();

    // Load configuration
    let config = koda_core::config::KodaConfig::load(&project_root, &cli.agent)?;
    let config = config
        .with_overrides(cli.base_url, cli.model, cli.provider)
        .with_model_overrides(
            cli.max_tokens,
            cli.temperature,
            cli.thinking_budget,
            cli.reasoning_effort,
        )
        .with_trust(koda_core::trust::TrustMode::parse(&cli.mode).unwrap_or_default());

    // Initialize database is already done above

    // Load or create session
    let session_id = match cli.session {
        Some(id) => id,
        None => db.create_session(&config.agent_name, &project_root).await?,
    };

    // Run the main event loop
    tui_app::run(
        project_root,
        config,
        db,
        session_id,
        version_check,
        first_run,
    )
    .await
}

/// Resolve the headless prompt from -p flag, positional arg, or stdin pipe.
fn resolve_headless_prompt(cli: &Cli) -> Result<Option<String>> {
    // Explicit -p flag
    if let Some(ref p) = cli.prompt {
        if p == "-" {
            // Read from stdin
            use std::io::Read;
            let mut input = String::new();
            std::io::stdin()
                .read_to_string(&mut input)
                .context("Failed to read from stdin")?;
            return Ok(Some(input.trim().to_string()));
        }
        return Ok(Some(p.clone()));
    }

    // Positional prompt
    if let Some(ref p) = cli.positional_prompt {
        return Ok(Some(p.clone()));
    }

    // Check if stdin is piped (not a TTY) — auto-headless
    if !atty_is_terminal() {
        use std::io::Read;
        let mut input = String::new();
        std::io::stdin()
            .read_to_string(&mut input)
            .context("Failed to read from stdin")?;
        let trimmed = input.trim().to_string();
        if !trimmed.is_empty() {
            return Ok(Some(trimmed));
        }
    }

    Ok(None)
}

/// Prune old `koda-<PID>.log` files, keeping only the `keep` most recent by mtime.
///
/// Called on startup before the new log file is created. Prevents unbounded
/// file-count accumulation (e.g. 18 000+ files after a year of heavy use).
/// All errors are silently discarded — a prune failure is never fatal.
fn prune_old_logs(log_dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(log_dir) else {
        return;
    };
    let mut files: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let s = name.to_string_lossy();
            s.starts_with("koda-") && s.ends_with(".log")
        })
        .filter_map(|e| {
            e.metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .map(|t| (t, e.path()))
        })
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0)); // newest first
    for (_, path) in files.into_iter().skip(keep) {
        let _ = std::fs::remove_file(path);
    }
}

/// Check if stdin is a terminal (not piped).
fn atty_is_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}
