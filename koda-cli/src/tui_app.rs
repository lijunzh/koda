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

use crate::tui_context::TuiContext;
use crate::tui_context::settings::Named;

use anyhow::Result;
use ratatui::{
    style::{Color, Style},
    text::Line,
};
use std::path::PathBuf;
use tokio::sync::mpsc;

/// Run the main interactive event loop with persistent TUI.
/// How the TUI starts: which session, and what it does first.
pub struct Start {
    /// An earlier session to open (`--resume`, `koda resume`).
    pub resume: Option<crate::session_id::Resume>,
    /// A prompt to send once the session opens (`koda "prompt"`).
    pub prompt: Option<String>,
    /// Images to send with it (`--image`), read already.
    pub images: Vec<crate::input::ImageData>,
    /// Open the session list first (`koda resume` with no session named).
    pub pick_session: bool,
}

pub async fn run(
    project_root: PathBuf,
    named: Named,
    start: Start,
    version_check: tokio::task::JoinHandle<Option<String>>,
    first_run: bool,
) -> Result<()> {
    let Start {
        resume,
        prompt,
        images,
        pick_session,
    } = start;
    let mut ctx = TuiContext::new(project_root, named, resume, version_check, first_run).await?;
    let first_run = ctx.first_run;

    // First-run welcome message (after terminal is initialized)
    if first_run {
        ctx.emit(Line::styled(
            "  \u{1f43b} Welcome to Koda! Let's pick your LLM provider.",
            Style::default().fg(Color::Cyan),
        ));
    }

    // The dialogs' answers and steering text, from the key handlers to the
    // turn loop.
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<crate::inference::TurnCommand>(32);

    // `koda "prompt"` sends it, as `codex "prompt"` does, with any
    // `--image` as `[Image #N]` markers ahead of it. On a first run, or
    // when `koda resume` opens the session list first, it waits in the
    // input box instead.
    let send_now = !first_run && !pick_session;
    match prompt {
        Some(prompt) if send_now => {
            let mut text = String::new();
            for image in images {
                ctx.image_count += 1;
                let marker = format!("[Image #{}]", ctx.image_count);
                text.push_str(&marker);
                text.push(' ');
                ctx.pasted_images.push((marker, image));
            }
            text.push_str(&prompt);
            ctx.pending_command = Some(text);
        }
        prompt => {
            for image in images {
                ctx.attach_image(image);
            }
            if let Some(prompt) = prompt {
                if !ctx.textarea.text().is_empty() {
                    ctx.textarea.insert_str(" ");
                }
                ctx.textarea.insert_str(&prompt);
            }
        }
    }
    // `koda resume` with no session named: the list, as `/resume` opens it.
    if pick_session {
        ctx.pending_command = Some("/resume".to_string());
    }

    // Initial viewport draw
    ctx.draw()?;

    // ── Main event loop ────────────────────────────────────────
    let ran = ctx.run_event_loop(&cmd_tx, &mut cmd_rx).await;

    // ── Cleanup ────────────────────────────────────────────
    ctx.cleanup().await;
    ctx.client.shutdown().await;
    ran?;

    Ok(())
}
