//! Headless mode — run a single prompt and exit.
//!
//! Invoked via `koda -p "fix the bug"`. Runs one inference turn with
//! auto-approval (no interactive prompts), prints the output, and exits
//! with code 0 (success) or 1 (error).
//!
//! ## Output formats
//!
//! - `text` (default) — plain text, suitable for piping
//! - `json` — structured JSON with tool calls and results
//! - `stream-json` — newline-delimited JSON events

use crate::input;
use koda_core::agent::KodaAgent;
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

    let mut agent = KodaAgent::new(&config, project_root.clone(), &[]).await?;
    crate::builtin_skills::inject_builtin_skills(&mut agent);
    agent.rebuild_system_prompt(&config, &[]);
    let agent = Arc::new(agent);
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<koda_core::engine::EngineCommand>(32);
    let mut session = KodaSession::new(session_id, agent, db, &config, config.trust).await;

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
    let cancel = session.cancel_token();
    let result = tokio::select! {
        r = session.run_turn(
            &config,
            pending_images,
            &cli_sink,
            &mut cmd_rx,
            None,
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
            // ── Approve non-destructive, reject destructive ────
            EngineEvent::ApprovalRequest {
                id,
                effect,
                tool_name,
                detail,
                ..
            } => {
                if effect == koda_core::tools::ToolEffect::Destructive {
                    eprintln!(
                        "\x1b[31m  ✗ Rejected destructive action: {tool_name} — {detail}\x1b[0m"
                    );
                    // #1022 B15: use `RejectAuto` so the model can
                    // distinguish a structural headless-policy block
                    // from a real human "no". Pre-fix it saw `"User
                    // rejected this action."` and would loop asking
                    // the (nonexistent) user for clarification.
                    let reason = format!(
                        "destructive action '{tool_name}' auto-rejected (headless mode \
                         refuses destructive ops by policy; no human is available to \
                         approve). Adapt your plan: avoid this kind of action for the \
                         rest of this session, or ask the operator to re-run \
                         interactively."
                    );
                    let _ = self.cmd_tx.try_send(EngineCommand::ApprovalResponse {
                        id,
                        decision: ApprovalDecision::RejectAuto { reason },
                    });
                } else {
                    let _ = self.cmd_tx.try_send(EngineCommand::ApprovalResponse {
                        id,
                        decision: ApprovalDecision::Approve,
                    });
                }
            }
            EngineEvent::AskUserRequest { id, question, .. } => {
                // Headless: no user present, print the question and skip.
                eprintln!("[koda] AskUser (no interactive session): {question}");
                let _ = self.cmd_tx.try_send(EngineCommand::AskUserResponse {
                    id,
                    answer: String::new(),
                });
            }
            EngineEvent::LoopCapReached { .. } => {
                let _ = self.cmd_tx.try_send(EngineCommand::LoopDecision {
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
            EngineEvent::ToolOutputLine {
                line, is_stderr, ..
            } => {
                if is_stderr {
                    eprintln!("  \u{2502}e {line}");
                } else {
                    eprintln!("  \u{2502} {line}");
                }
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
                    let diff_lines = crate::diff_render::render_lines(p);
                    for line in &diff_lines {
                        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                        eprintln!("  {text}");
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
            EngineEvent::ContextUsage { .. } => {}
            EngineEvent::TurnStart { .. } => {}
            EngineEvent::TurnEnd { .. } => {}
            // (#1076) Bg-task lifecycle in headless: render as a short
            // status line so a script tailing stdout sees "task 7
            // running (iter 3)" instead of nothing. The full result
            // payload is still injected at completion via the
            // `drain_completed` path — this is just live progress.
            EngineEvent::ChildTaskUpdate {
                task_id, status, ..
            } => {
                let summary = match status {
                    koda_core::child_agent::AgentStatus::Pending => "pending".to_string(),
                    koda_core::child_agent::AgentStatus::Running { iter } => {
                        if iter == 0 {
                            "running (starting)".to_string()
                        } else {
                            format!("running (iter {iter})")
                        }
                    }
                    koda_core::child_agent::AgentStatus::Cancelled => "cancelled".to_string(),
                    koda_core::child_agent::AgentStatus::Completed { .. } => {
                        "completed".to_string()
                    }
                    koda_core::child_agent::AgentStatus::Errored { error } => {
                        let snippet: String = error.chars().take(80).collect();
                        format!("errored: {snippet}")
                    }
                    // Forward-compat: future statuses render as a
                    // generic label so script tailing stdout sees
                    // *something* (#1224).
                    _ => "(unknown status)".to_string(),
                };
                eprintln!("\x1b[2m  [bg task {task_id}] {summary}\x1b[0m");
            }
            // (#1201 B) Live activity from inside a bg agent. Render
            // dimly indented under the parent feed so a tailing script
            // sees "  [bg task 7]   \u{1f527} Read src/auth.rs" without
            // having to wait for the post-completion drain.
            EngineEvent::ChildAgentActivity { task_id, kind, .. } => {
                let line = match kind {
                    koda_core::engine::event::ChildAgentActivityKind::ToolStart {
                        summary, ..
                    } => format!("\u{1f527} {summary}"),
                    koda_core::engine::event::ChildAgentActivityKind::ToolEnd {
                        tool_name,
                        success,
                    } => {
                        let icon = if success { "\u{2713}" } else { "\u{2717}" };
                        format!("{icon} {tool_name}")
                    }
                    koda_core::engine::event::ChildAgentActivityKind::Info { message } => message,
                    // Forward-compat: future activity kinds render as
                    // a generic line (#1224).
                    _ => "(activity)".to_string(),
                };
                eprintln!("\x1b[2m  [bg task {task_id}] {line}\x1b[0m");
            }
            // (#1077 Phase A) TodoWrite lifecycle in headless: render a
            // dim one-liner with diff counts so a script tailing stdout
            // sees "todos: +2 ~1 -0 (5 total)" on every accepted change.
            // The full per-item list still rides on the next tool result
            // (which the headless `print_tool_result` path renders) —
            // this is just the structured transition for scripts that
            // want to grep "added" without parsing the formatted list.
            EngineEvent::TodoUpdate { items, diff } => {
                eprintln!(
                    "\x1b[2m  [todos] +{} ~{} -{} ({} total)\x1b[0m",
                    diff.added.len(),
                    diff.changed.len(),
                    diff.removed.len(),
                    items.len(),
                );
            }
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
            // Forward-compat: future EngineEvent variants are silently
            // dropped from the headless feed until we wire a render
            // for them. Same shape as the existing ContextUsage /
            // TurnStart no-op arms (#1224).
            _ => {}
        }
    }
}
