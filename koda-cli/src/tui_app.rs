//! TUI main event loop.
//!
//! Thin entry point that creates a `TuiContext` and runs the event loop.
//! All handler logic lives in `tui_context.rs` and its sub-modules.
//!
//! Supporting modules:
//! - `tui_context` — shared state struct + handler methods
//! - `tui_types` — enums, type aliases, constants
//! - `tui_viewport` — viewport drawing and terminal lifecycle
//! - `tui_history` — command history persistence
//! - `tui_commands` — slash command dispatch
//! - `tui_render` — streaming inference output rendering
//! - `tui_output` — low-level terminal output helpers
//!
//! See #209 for the refactoring plan.

use crate::tui_context::{ProviderOverrides, TuiContext};

use anyhow::Result;
use koda_core::config::KodaConfig;
use koda_core::db::Database;
use koda_core::engine::EngineCommand;
use ratatui::{
    style::{Color, Style},
    text::Line,
};
use std::path::PathBuf;
use tokio::sync::mpsc;

/// Run the main interactive event loop with persistent TUI.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    project_root: PathBuf,
    config: KodaConfig,
    overrides: ProviderOverrides,
    db: Database,
    resume: Option<String>,
    prompt: Option<String>,
    version_check: tokio::task::JoinHandle<Option<String>>,
    first_run: bool,
) -> Result<()> {
    let mut ctx = TuiContext::new(
        project_root,
        config,
        overrides,
        db,
        resume,
        version_check,
        first_run,
    )
    .await?;

    // First-run welcome message (after terminal is initialized)
    if first_run {
        ctx.emit(Line::styled(
            "  \u{1f43b} Welcome to Koda! Let's pick your LLM provider.",
            Style::default().fg(Color::Cyan),
        ));
    }

    // The engine's events, from the host; the dialogs' answers and steering
    // text, from the key handlers to the turn loop.
    let mut ui_rx = ctx
        .client
        .take_raw_events()
        .ok_or_else(|| anyhow::anyhow!("the host's engine events were taken"))?;
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<EngineCommand>(32);

    // `koda "prompt"` sends it, as `codex "prompt"` does. On a first run it
    // waits in the input box until a provider is chosen.
    if let Some(prompt) = prompt {
        if first_run {
            ctx.textarea.insert_str(&prompt);
        } else {
            ctx.pending_command = Some(prompt);
        }
    }

    // Initial viewport draw
    ctx.draw()?;

    // ── Main event loop ────────────────────────────────────────
    let ran = ctx.run_event_loop(&mut ui_rx, &cmd_tx, &mut cmd_rx).await;

    // ── Cleanup ────────────────────────────────────────────
    ctx.cleanup().await;
    ctx.client.shutdown().await;
    ran?;

    Ok(())
}
