//! Headless mode — run a single prompt and exit.

use crate::input;
use koda_core::agent::KodaAgent;
use koda_core::approval::ApprovalMode;
use koda_core::config::KodaConfig;
use koda_core::db::{Database, Role};
use koda_core::engine::{ApprovalDecision, EngineCommand, EngineEvent, EngineSink};
use koda_core::persistence::Persistence;
use koda_core::session::KodaSession;

use anyhow::Result;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

/// Run a single prompt and exit. Returns process exit code (0 = success).
pub async fn run_headless(
    project_root: PathBuf,
    mut config: KodaConfig,
    db: Database,
    session_id: String,
    prompt: String,
    output_format: &str,
) -> Result<i32> {
    // Query actual model capabilities from the provider API before building agent.
    let tmp_provider = koda_core::providers::create_provider(&config);
    config
        .query_and_apply_capabilities(tmp_provider.as_ref())
        .await;

    let agent = Arc::new(KodaAgent::new(&config, project_root.clone()).await?);
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<koda_core::engine::EngineCommand>(32);
    let mut session = KodaSession::new(session_id, agent, db, &config, ApprovalMode::Auto);

    // Process @file references and images
    let processed = input::process_input(&prompt, &project_root);
    let user_message = if let Some(context) = input::format_context_files(&processed.context_files)
    {
        format!("{}\n\n{context}", processed.prompt)
    } else {
        processed.prompt.clone()
    };

    let pending_images = if processed.images.is_empty() {
        None
    } else {
        Some(processed.images)
    };

    session
        .db
        .insert_message(
            &session.id,
            &Role::User,
            Some(&user_message),
            None,
            None,
            None,
        )
        .await?;

    let cli_sink = HeadlessSink::new(cmd_tx);
    let cancel = session.cancel.clone();
    let result = tokio::select! {
        r = session.run_turn(
            &config,
            pending_images,
            &cli_sink,
            &mut cmd_rx,
        ) => r,
        _ = tokio::signal::ctrl_c() => {
            cancel.cancel();
            eprintln!("\n\x1b[33m\u{26a0} Interrupted\x1b[0m");
            Ok(())
        }
    };

    // For JSON output, wrap the last assistant response
    if output_format == "json" {
        let last_response = session
            .db
            .last_assistant_message(&session.id)
            .await
            .unwrap_or_default();
        let json = serde_json::json!({
            "success": result.is_ok(),
            "response": last_response,
            "session_id": session.id,
            "model": config.model,
        });
        println!("{}", serde_json::to_string_pretty(&json)?);
    }

    match result {
        Ok(()) => Ok(0),
        Err(e) => {
            eprintln!("Error: {e}");
            Ok(1)
        }
    }
}

// ---------------------------------------------------------------------------
// HeadlessSink — simple println rendering, auto-approves everything
// ---------------------------------------------------------------------------

struct HeadlessSink {
    cmd_tx: tokio::sync::mpsc::Sender<EngineCommand>,
}

impl HeadlessSink {
    fn new(cmd_tx: tokio::sync::mpsc::Sender<EngineCommand>) -> Self {
        Self { cmd_tx }
    }
}

impl EngineSink for HeadlessSink {
    fn emit(&self, event: EngineEvent) {
        match event {
            // ── Auto-approve (headless = yolo) ──────────────────
            EngineEvent::ApprovalRequest { id, .. } => {
                let _ = self.cmd_tx.blocking_send(EngineCommand::ApprovalResponse {
                    id,
                    decision: ApprovalDecision::Approve,
                });
            }
            EngineEvent::LoopCapReached { .. } => {
                let _ = self.cmd_tx.blocking_send(EngineCommand::LoopDecision {
                    action: koda_core::loop_guard::LoopContinuation::Continue200,
                });
            }

            // ── Streaming text ──────────────────────────────────
            EngineEvent::TextDelta { text } => {
                print!("{text}");
                let _ = std::io::stdout().flush();
            }
            EngineEvent::TextDone => {
                println!();
            }

            // ── Thinking ────────────────────────────────────────
            EngineEvent::ThinkingStart => {
                eprintln!("\x1b[90m  \u{1f4ad} thinking...\x1b[0m");
            }
            EngineEvent::ThinkingDelta { .. } => {}
            EngineEvent::ThinkingDone => {}

            // ── Tool calls ──────────────────────────────────────
            EngineEvent::ToolCallStart { name, .. } => {
                eprintln!("\x1b[36m  \u{26a1} {name}\x1b[0m");
            }
            EngineEvent::ToolCallResult { name, output, .. } => {
                use koda_core::truncate::{Truncated, truncate_for_display};
                eprintln!("\x1b[32m  \u{2713} {name}\x1b[0m");
                match truncate_for_display(&output) {
                    Truncated::Full(_) => {
                        for line in output.lines() {
                            eprintln!("  \u{2502} {line}");
                        }
                    }
                    Truncated::Split {
                        head,
                        tail,
                        hidden,
                        total,
                    } => {
                        for line in &head {
                            eprintln!("  \u{2502} {line}");
                        }
                        eprintln!(
                            "\x1b[2m{}\x1b[0m",
                            koda_core::truncate::separator(hidden, total)
                        );
                        for line in &tail {
                            eprintln!("  \u{2502} {line}");
                        }
                    }
                }
            }

            // ── Sub-agents ──────────────────────────────────────
            EngineEvent::SubAgentStart { agent_name } => {
                eprintln!("\x1b[35m  \u{1f916} {agent_name}\x1b[0m");
            }

            // ── Blocked actions ──────────────────────────────────
            EngineEvent::ActionBlocked {
                detail, preview, ..
            } => {
                eprintln!("\x1b[33m  \u{1f50d} Would execute: {detail}\x1b[0m");
                if let Some(ref p) = preview {
                    let rendered = crate::diff_render::render(p);
                    for line in rendered.lines() {
                        eprintln!("  {line}");
                    }
                }
            }

            // ── Info/Warn/Error ──────────────────────────────────
            EngineEvent::Info { message } => eprintln!("\x1b[36m  {message}\x1b[0m"),
            EngineEvent::Warn { message } => eprintln!("\x1b[33m  \u{26a0} {message}\x1b[0m"),
            EngineEvent::Error { message } => eprintln!("\x1b[31m  \u{2717} {message}\x1b[0m"),

            // ── Ignored in headless ─────────────────────────────
            EngineEvent::ResponseStart => {}
            EngineEvent::SpinnerStart { .. } => {}
            EngineEvent::SpinnerStop => {}
            EngineEvent::StatusUpdate { .. } => {}
            EngineEvent::TurnStart { .. } => {}
            EngineEvent::TurnEnd { .. } => {}
            EngineEvent::Footer {
                completion_tokens,
                total_chars,
                elapsed_ms,
                rate,
                ..
            } => {
                let tokens = if completion_tokens > 0 {
                    completion_tokens
                } else {
                    (total_chars / 4) as i64
                };
                let secs = elapsed_ms as f64 / 1000.0;
                eprintln!(
                    "\x1b[90m  {tokens} tokens \u{00b7} {secs:.1}s \u{00b7} {rate:.0} t/s\x1b[0m"
                );
            }
        }
    }
}
