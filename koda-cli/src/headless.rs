//! Headless mode — run a single prompt and exit.
//!
//! Invoked via `koda -p "fix the bug"`. Runs one inference turn with
//! auto-approval (no interactive prompts), prints the output, and exits
//! with code 0 (success) or 1 (error).
//!
//! ## Output formats
//!
//! - `text` (default) — the streamed reply on stdout, progress
//!   decorations (tool calls, footer) on stderr. For humans and pipes.
//! - `json` — stdout carries exactly one pretty-printed run summary
//!   object after the turn ends (see [`run_summary`]); the streamed reply
//!   and progress go to stderr.
//! - `stream-json` — stdout carries newline-delimited JSON only: one line
//!   per [`EngineEvent`] as it happens (`{"type":"text_delta",...}`),
//!   then a final `{"type":"run_summary",...}` line. Machine consumers
//!   (the eval harness) parse every stdout line; stderr stays quiet.

use crate::input;
use koda_core::agent::KodaAgent;
use koda_core::config::KodaConfig;
use koda_core::db::{Database, Role};
use koda_core::engine::{ApprovalDecision, EngineCommand, EngineEvent, EngineSink, TurnEndReason};
use koda_core::persistence::{Persistence, SessionUsage};
use koda_core::session::KodaSession;

use anyhow::Result;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

/// Headless output format (`--output-format`). See the module docs for
/// what each format writes to stdout vs stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    /// Streamed reply on stdout, decorations on stderr.
    Text,
    /// One run-summary JSON object on stdout after the turn.
    Json,
    /// NDJSON on stdout: every engine event, then the run summary.
    StreamJson,
}

/// Run a single prompt and exit. Returns process exit code (0 = success).
pub async fn run_headless(
    project_root: PathBuf,
    mut config: KodaConfig,
    db: Database,
    session_id: String,
    prompt: String,
    output_format: OutputFormat,
) -> Result<i32> {
    // Query actual model capabilities from the provider API before building agent.
    let tmp_provider = koda_core::providers::create_provider(&config);
    let source = config
        .query_and_apply_capabilities(tmp_provider.as_ref())
        .await;
    // stderr, like every other notice: stdout carries only the answer or JSON.
    if let Some(notice) = config.assumed_context_notice(source) {
        eprintln!("\x1b[33m  \u{26a0} {notice}\x1b[0m");
    }

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

    let cli_sink = HeadlessSink::new(cmd_tx, output_format);
    let cancel = session.cancel_token();
    let started = Instant::now();
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

    let elapsed_ms = started.elapsed().as_millis() as u64;

    if output_format != OutputFormat::Text {
        // Usage comes from the DB rather than the `Footer` event: the
        // footer only fires on a clean finish, while every persisted
        // assistant message (sub-agents included) carries its tokens,
        // so the totals stay right for errored and cancelled turns too.
        let response = session
            .db
            .last_assistant_message(&session.id)
            .await
            .unwrap_or_default();
        let usage = session
            .db
            .session_token_usage(&session.id)
            .await
            .unwrap_or_default();
        let usage_by_agent = session
            .db
            .session_usage_by_agent(&session.id)
            .await
            .unwrap_or_default();
        let summary = run_summary(RunOutcome {
            result: &result,
            session_id: &session.id,
            provider: config.provider_type.meta().name,
            model: &config.model,
            response: &response,
            elapsed_ms,
            usage: &usage,
            usage_by_agent: &usage_by_agent,
            stats: &cli_sink.stats(),
        });
        match output_format {
            OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&summary)?),
            OutputFormat::StreamJson => println!("{}", serde_json::to_string(&summary)?),
            OutputFormat::Text => {}
        }
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
// Run summary — the `json` object / final `stream-json` line
// ---------------------------------------------------------------------------

/// Per-run counters the sink accumulates from the event stream.
#[derive(Debug, Clone, Default, PartialEq)]
struct RunStats {
    /// Top-level tool calls by tool name.
    tool_calls: BTreeMap<String, u64>,
    /// Tool calls made by sub-agents, by tool name.
    sub_agent_tool_calls: BTreeMap<String, u64>,
    /// Reason from the last `TurnEnd` seen (the top-level turn ends last).
    turn_end: Option<TurnEndReason>,
}

/// Everything [`run_summary`] needs once the turn is over.
struct RunOutcome<'a> {
    result: &'a Result<()>,
    session_id: &'a str,
    provider: &'a str,
    model: &'a str,
    response: &'a str,
    elapsed_ms: u64,
    usage: &'a SessionUsage,
    usage_by_agent: &'a [(String, SessionUsage)],
    stats: &'a RunStats,
}

fn usage_json(usage: &SessionUsage) -> serde_json::Value {
    serde_json::json!({
        "prompt_tokens": usage.prompt_tokens,
        "completion_tokens": usage.completion_tokens,
        "cache_read_tokens": usage.cache_read_tokens,
        "cache_creation_tokens": usage.cache_creation_tokens,
        "thinking_tokens": usage.thinking_tokens,
        "api_calls": usage.api_calls,
    })
}

/// Build the run summary. `success`, `response`, `session_id` and
/// `model` keep the shape the `json` format always had; the rest is
/// additive. `"type": "run_summary"` doesn't collide with any
/// `EngineEvent` tag, so `stream-json` consumers can tell it apart.
fn run_summary(outcome: RunOutcome<'_>) -> serde_json::Value {
    let usage_by_agent: serde_json::Map<String, serde_json::Value> = outcome
        .usage_by_agent
        .iter()
        .map(|(agent, usage)| (agent.clone(), usage_json(usage)))
        .collect();
    serde_json::json!({
        "type": "run_summary",
        "success": outcome.result.is_ok(),
        "error": outcome.result.as_ref().err().map(|e| format!("{e:#}")),
        "session_id": outcome.session_id,
        "provider": outcome.provider,
        "model": outcome.model,
        "response": outcome.response,
        "turn_end": outcome.stats.turn_end,
        "elapsed_ms": outcome.elapsed_ms,
        "usage": usage_json(outcome.usage),
        "usage_by_agent": usage_by_agent,
        "tool_calls": outcome.stats.tool_calls,
        "sub_agent_tool_calls": outcome.stats.sub_agent_tool_calls,
    })
}

// ---------------------------------------------------------------------------
// HeadlessSink — answers engine requests, renders or streams events
// ---------------------------------------------------------------------------

struct HeadlessSink {
    cmd_tx: tokio::sync::mpsc::Sender<EngineCommand>,
    format: OutputFormat,
    stats: Mutex<RunStats>,
}

impl HeadlessSink {
    fn new(cmd_tx: tokio::sync::mpsc::Sender<EngineCommand>, format: OutputFormat) -> Self {
        Self {
            cmd_tx,
            format,
            stats: Mutex::new(RunStats::default()),
        }
    }

    fn stats(&self) -> RunStats {
        self.stats
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn record(&self, event: &EngineEvent) {
        let mut stats = self.stats.lock().unwrap_or_else(PoisonError::into_inner);
        match event {
            EngineEvent::ToolCallStart {
                name, is_sub_agent, ..
            } => {
                let counts = if *is_sub_agent {
                    &mut stats.sub_agent_tool_calls
                } else {
                    &mut stats.tool_calls
                };
                *counts.entry(name.clone()).or_default() += 1;
            }
            EngineEvent::TurnEnd { reason, .. } => stats.turn_end = Some(reason.clone()),
            _ => {}
        }
    }

    /// Answer the engine's blocking requests. Runs in every output
    /// format: approve non-destructive actions, auto-reject destructive
    /// ones, skip AskUser, and keep going at the loop cap.
    fn respond(&self, event: &EngineEvent) {
        let command = match event {
            EngineEvent::ApprovalRequest {
                id,
                effect,
                tool_name,
                ..
            } => {
                let decision = if *effect == koda_core::tools::ToolEffect::Destructive {
                    // #1022 B15: use `RejectAuto` so the model can
                    // distinguish a structural headless-policy block
                    // from a real human "no". Pre-fix it saw `"User
                    // rejected this action."` and would loop asking
                    // the (nonexistent) user for clarification.
                    ApprovalDecision::RejectAuto {
                        reason: format!(
                            "destructive action '{tool_name}' auto-rejected (headless mode \
                             refuses destructive ops by policy; no human is available to \
                             approve). Adapt your plan: avoid this kind of action for the \
                             rest of this session, or ask the operator to re-run \
                             interactively."
                        ),
                    }
                } else {
                    ApprovalDecision::Approve
                };
                EngineCommand::ApprovalResponse {
                    id: id.clone(),
                    decision,
                }
            }
            // Headless: no user present, answer empty.
            EngineEvent::AskUserRequest { id, .. } => EngineCommand::AskUserResponse {
                id: id.clone(),
                answer: String::new(),
            },
            EngineEvent::LoopCapReached { .. } => EngineCommand::LoopDecision {
                action: koda_core::loop_guard::LoopContinuation::Continue200,
            },
            _ => return,
        };
        let _ = self.cmd_tx.try_send(command);
    }

    /// Human-readable rendering for `text` and `json`. The streamed reply
    /// goes to stdout only in `text`; in `json`, stdout is reserved for
    /// the summary object.
    fn render(&self, event: EngineEvent) {
        let reply_on_stdout = self.format == OutputFormat::Text;
        match event {
            EngineEvent::ApprovalRequest {
                effect,
                tool_name,
                detail,
                ..
            } => {
                if effect == koda_core::tools::ToolEffect::Destructive {
                    eprintln!(
                        "\x1b[31m  ✗ Rejected destructive action: {tool_name} — {detail}\x1b[0m"
                    );
                }
            }
            EngineEvent::AskUserRequest { question, .. } => {
                eprintln!("[koda] AskUser (no interactive session): {question}");
            }
            EngineEvent::LoopCapReached { .. } => {}

            // ── Streaming text ──────────────────────────────────
            EngineEvent::TextDelta { text } => {
                if reply_on_stdout {
                    print!("{text}");
                    let _ = std::io::stdout().flush();
                } else {
                    eprint!("{text}");
                }
            }
            EngineEvent::TextDone => {
                if reply_on_stdout {
                    println!();
                } else {
                    eprintln!();
                }
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
            // One run; the engine already uses the learned window.
            EngineEvent::ContextWindowChanged { .. } => {}
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

impl EngineSink for HeadlessSink {
    fn emit(&self, event: EngineEvent) {
        self.record(&event);
        self.respond(&event);
        match self.format {
            // `println!` goes through Rust's line-buffered stdout, so each
            // event reaches a piped consumer as soon as its line ends.
            OutputFormat::StreamJson => match serde_json::to_string(&event) {
                Ok(line) => println!("{line}"),
                Err(e) => tracing::warn!("stream-json: failed to serialize event: {e}"),
            },
            OutputFormat::Text | OutputFormat::Json => self.render(event),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use koda_core::tools::ToolEffect;

    fn sink(format: OutputFormat) -> (HeadlessSink, tokio::sync::mpsc::Receiver<EngineCommand>) {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        (HeadlessSink::new(tx, format), rx)
    }

    fn approval(id: &str, effect: ToolEffect) -> EngineEvent {
        EngineEvent::ApprovalRequest {
            id: id.into(),
            tool_name: "Bash".into(),
            detail: "rm -rf build".into(),
            preview: None,
            effect,
        }
    }

    fn tool_start(name: &str, is_sub_agent: bool) -> EngineEvent {
        EngineEvent::ToolCallStart {
            id: format!("call-{name}"),
            name: name.into(),
            args: serde_json::json!({}),
            is_sub_agent,
        }
    }

    #[test]
    fn requests_are_answered_the_same_in_every_format() {
        for format in [
            OutputFormat::Text,
            OutputFormat::Json,
            OutputFormat::StreamJson,
        ] {
            let (sink, mut rx) = sink(format);
            sink.emit(approval("a1", ToolEffect::LocalMutation));
            sink.emit(approval("a2", ToolEffect::Destructive));
            sink.emit(EngineEvent::AskUserRequest {
                id: "q1".into(),
                question: "which file?".into(),
                options: vec![],
            });
            sink.emit(EngineEvent::LoopCapReached {
                cap: 200,
                recent_tools: vec![],
            });

            match rx.try_recv() {
                Ok(EngineCommand::ApprovalResponse { id, decision }) => {
                    assert_eq!(id, "a1");
                    assert_eq!(decision, ApprovalDecision::Approve, "{format:?}");
                }
                other => panic!("{format:?}: expected approval, got {other:?}"),
            }
            match rx.try_recv() {
                Ok(EngineCommand::ApprovalResponse { id, decision }) => {
                    assert_eq!(id, "a2");
                    assert!(
                        matches!(decision, ApprovalDecision::RejectAuto { .. }),
                        "{format:?}: destructive must be auto-rejected, got {decision:?}"
                    );
                }
                other => panic!("{format:?}: expected rejection, got {other:?}"),
            }
            assert!(
                matches!(
                    rx.try_recv(),
                    Ok(EngineCommand::AskUserResponse { ref id, ref answer })
                        if id == "q1" && answer.is_empty()
                ),
                "{format:?}: AskUser must get an empty answer"
            );
            assert!(
                matches!(rx.try_recv(), Ok(EngineCommand::LoopDecision { .. })),
                "{format:?}: loop cap must get a continue decision"
            );
            assert!(rx.try_recv().is_err(), "{format:?}: no extra commands");
        }
    }

    #[test]
    fn stats_split_top_level_and_sub_agent_tool_calls() {
        let (sink, _rx) = sink(OutputFormat::Json);
        sink.emit(tool_start("Read", false));
        sink.emit(tool_start("Read", false));
        sink.emit(tool_start("Edit", false));
        sink.emit(tool_start("Grep", true));
        sink.emit(EngineEvent::TurnEnd {
            turn_id: "t1".into(),
            reason: TurnEndReason::Complete,
        });

        let stats = sink.stats();
        assert_eq!(
            stats.tool_calls,
            BTreeMap::from([("Edit".to_string(), 1), ("Read".to_string(), 2)])
        );
        assert_eq!(
            stats.sub_agent_tool_calls,
            BTreeMap::from([("Grep".to_string(), 1)])
        );
        assert_eq!(stats.turn_end, Some(TurnEndReason::Complete));
    }

    #[test]
    fn run_summary_keeps_legacy_keys_and_adds_metrics() {
        let stats = RunStats {
            tool_calls: BTreeMap::from([("Read".to_string(), 2)]),
            sub_agent_tool_calls: BTreeMap::new(),
            turn_end: Some(TurnEndReason::Complete),
        };
        let usage = SessionUsage {
            prompt_tokens: 120,
            completion_tokens: 30,
            api_calls: 2,
            ..Default::default()
        };
        let by_agent = vec![("main".to_string(), usage.clone())];
        let result: Result<()> = Ok(());
        let summary = run_summary(RunOutcome {
            result: &result,
            session_id: "s1",
            provider: "mock",
            model: "mock-model",
            response: "done",
            elapsed_ms: 42,
            usage: &usage,
            usage_by_agent: &by_agent,
            stats: &stats,
        });

        assert_eq!(summary["type"], "run_summary");
        // Keys the `json` format has always had.
        assert_eq!(summary["success"], true);
        assert_eq!(summary["response"], "done");
        assert_eq!(summary["session_id"], "s1");
        assert_eq!(summary["model"], "mock-model");
        // Additive metrics.
        assert_eq!(summary["error"], serde_json::Value::Null);
        assert_eq!(summary["turn_end"]["kind"], "complete");
        assert_eq!(summary["usage"]["prompt_tokens"], 120);
        assert_eq!(summary["usage"]["api_calls"], 2);
        assert_eq!(summary["usage_by_agent"]["main"]["completion_tokens"], 30);
        assert_eq!(summary["tool_calls"]["Read"], 2);
    }

    #[test]
    fn run_summary_reports_errors() {
        let result: Result<()> = Err(anyhow::anyhow!("provider exploded"));
        let summary = run_summary(RunOutcome {
            result: &result,
            session_id: "s1",
            provider: "mock",
            model: "mock-model",
            response: "",
            elapsed_ms: 0,
            usage: &SessionUsage::default(),
            usage_by_agent: &[],
            stats: &RunStats::default(),
        });
        assert_eq!(summary["success"], false);
        assert_eq!(summary["error"], "provider exploded");
        assert_eq!(summary["turn_end"], serde_json::Value::Null);
    }
}
