//! Koda 🐻 - A high-performance AI coding agent built in Rust.
//!
//! CLI entry point. The binary is named `koda` for ergonomics.

mod app;
mod approval;
mod config;
mod confirm;
mod context;
mod db;
mod display;
mod highlight;
mod inference;
mod input;
mod interrupt;
mod keystore;
mod loop_guard;
mod markdown;
mod memory;
mod onboarding;
mod preview;
mod providers;
mod repl;
mod runtime_env;
mod tools;
mod tui;
mod version;
mod mcp;

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;

/// Koda 🐻 - Your AI coding bear.
#[derive(Parser, Debug)]
#[command(name = "koda", version, about)]
struct Cli {
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
    #[arg(short, long)]
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

    /// LLM provider (openai, anthropic, lmstudio, gemini, groq, grok)
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Install Ctrl+C handler for graceful interrupts
    interrupt::install_handler();

    // Resolve headless prompt: -p flag, positional arg, or stdin
    let headless_prompt = resolve_headless_prompt(&cli)?;

    // Resolve project root
    let project_root = cli
        .project_root
        .unwrap_or_else(|| std::env::current_dir().expect("Failed to get current directory"));
    let project_root = std::fs::canonicalize(&project_root)?;

    // Initialize logging to file (invisible to user)
    let log_dir = project_root.join(".koda_logs");
    std::fs::create_dir_all(&log_dir)?;
    let file_appender = tracing_appender::rolling::daily(&log_dir, "koda.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_env_filter("koda_agent=debug")
        .init();

    tracing::info!("Koda starting. Project root: {:?}", project_root);

    // Load and inject stored API keys (env vars take precedence)
    match keystore::KeyStore::load() {
        Ok(store) => store.inject_into_env(),
        Err(e) => tracing::warn!("Failed to load keystore: {e}"),
    }

    // Headless mode: skip onboarding, banner, version check
    if let Some(prompt) = headless_prompt {
        let config = config::KodaConfig::load(&project_root, &cli.agent)?;
        let config = config
            .with_overrides(cli.base_url, cli.model, cli.provider)
            .with_model_overrides(
                cli.max_tokens,
                cli.temperature,
                cli.thinking_budget,
                cli.reasoning_effort,
            );
        let db = db::Database::init(&project_root).await?;
        let session_id = match cli.session {
            Some(id) => id,
            None => db.create_session(&config.agent_name, &project_root).await?,
        };
        let exit_code = app::run_headless(
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
    let version_check = version::spawn_version_check();

    // First-run onboarding
    let onboarding_provider = if onboarding::is_first_run() {
        onboarding::run_wizard()
    } else {
        None
    };

    // Load configuration
    let config = config::KodaConfig::load(&project_root, &cli.agent)?;
    let config = config
        .with_overrides(
            cli.base_url,
            cli.model,
            cli.provider
                .or_else(|| onboarding_provider.map(|p| p.to_string())),
        )
        .with_model_overrides(
            cli.max_tokens,
            cli.temperature,
            cli.thinking_budget,
            cli.reasoning_effort,
        );

    // Initialize database
    let db = db::Database::init(&project_root).await?;

    // Load or create session
    let session_id = match cli.session {
        Some(id) => id,
        None => db.create_session(&config.agent_name, &project_root).await?,
    };

    // Run the main event loop (pass version check handle for post-banner hint)
    app::run(project_root, config, db, session_id, version_check).await
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

/// Check if stdin is a terminal (not piped).
fn atty_is_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}
