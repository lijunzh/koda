//! TUI main event loop.
//!
//! Thin entry point that creates a [`TuiContext`] and runs the event loop.
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

use crate::sink::UiEvent;
use crate::tui_context::TuiContext;

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
pub async fn run(
    project_root: PathBuf,
    config: KodaConfig,
    db: Database,
    session_id: String,
    version_check: tokio::task::JoinHandle<Option<String>>,
    first_run: bool,
    skip_probe: bool,
) -> Result<()> {
    let mut ctx = TuiContext::new(
        project_root,
        config,
        db,
        session_id,
        version_check,
        first_run,
        skip_probe,
    )
    .await?;

    // First-run welcome message (after terminal is initialized)
    if first_run {
        ctx.emit(Line::styled(
            "  \u{1f43b} Welcome to Koda! Let's pick your LLM provider.",
            Style::default().fg(Color::Cyan),
        ));
    }

    // Channels stay in run() — consumed by tokio::select! in different ways
    let (ui_tx, mut ui_rx) = mpsc::channel::<UiEvent>(256);
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<EngineCommand>(32);

    // Initial viewport draw
    ctx.draw()?;

    // ── Main event loop ────────────────────────────────────────
    ctx.run_event_loop(&ui_tx, &mut ui_rx, &cmd_tx, &mut cmd_rx)
        .await?;

    // ── Cleanup ────────────────────────────────────────────
    ctx.cleanup().await;

    Ok(())
}
