//! Koda 🐻 - A high-performance AI coding agent built in Rust.
//!
//! CLI entry point. The binary is named `koda` for ergonomics.

mod app;
mod clipboard;
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
mod markdown;
mod memory;
mod onboarding;
mod providers;
mod repl;
mod runtime_env;
mod tools;
mod tui;
mod version;

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

/// Koda 🐻 - Your AI coding bear.
#[derive(Parser, Debug)]
#[command(name = "koda", version, about)]
struct Cli {
    /// Agent to use (matches a JSON file in agents/)
    #[arg(short, long, default_value = "default")]
    agent: String,

    /// Session ID to resume (omit to start a new session)
    #[arg(short, long)]
    session: Option<String>,

    /// Project root directory (defaults to current directory)
    #[arg(short, long)]
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Install Ctrl+C handler for graceful interrupts
    interrupt::install_handler();

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

    // Start version check in the background (non-blocking)
    let version_check = version::spawn_version_check();

    // Load and inject stored API keys (env vars take precedence)
    match keystore::KeyStore::load() {
        Ok(store) => store.inject_into_env(),
        Err(e) => tracing::warn!("Failed to load keystore: {e}"),
    }

    // First-run onboarding
    let onboarding_provider = if onboarding::is_first_run() {
        onboarding::run_wizard()
    } else {
        None
    };

    // Load configuration
    let config = config::KodaConfig::load(&project_root, &cli.agent)?;
    let config = config.with_overrides(
        cli.base_url,
        cli.model,
        cli.provider
            .or_else(|| onboarding_provider.map(|p| p.to_string())),
    );

    // Initialize database
    let db = db::Database::init(&project_root).await?;

    // Load or create session
    let session_id = match cli.session {
        Some(id) => id,
        None => db.create_session(&config.agent_name).await?,
    };

    // Run the main event loop (pass version check handle for post-banner hint)
    app::run(project_root, config, db, session_id, version_check).await
}
