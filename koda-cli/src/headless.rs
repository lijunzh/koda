//! Headless mode — run a single prompt and exit.

use crate::input;
use koda_core::agent::KodaAgent;
use koda_core::approval::ApprovalMode;
use koda_core::config::KodaConfig;
use koda_core::db::{Database, Role};
use koda_core::session::KodaSession;

use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;

/// Run a single prompt and exit. Returns process exit code (0 = success).
pub async fn run_headless(
    project_root: PathBuf,
    config: KodaConfig,
    db: Database,
    session_id: String,
    prompt: String,
    output_format: &str,
) -> Result<i32> {
    let agent = Arc::new(KodaAgent::new(&config, project_root.clone()).await?);
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<koda_core::engine::EngineCommand>(32);
    let mut session = KodaSession::new(session_id, agent, db, &config, ApprovalMode::Yolo);

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

    let cli_sink = crate::headless_sink::HeadlessSink::new(cmd_tx);
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
