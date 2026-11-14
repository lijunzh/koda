//! One-shot runs: one prompt, one turn, then exit: `koda exec`.
//!
//! The run goes through the client API, as every Koda front end will: it
//! starts Koda's host in process, opens a session whose requests Koda
//! answers itself (`approvalPolicy: never`), starts one turn and reads what
//! comes back. Koda approves what the mode allows, refuses destructive
//! actions and anything outside the sandbox, answers a question with nothing
//! and keeps going at the loop cap. Ctrl+C sends `turn/interrupt`. Exit code
//! 0 is success, 1 failure.
//!
//! ## Output
//!
//! - [`Output::Text`] (`koda exec`): the streamed reply on stdout,
//!   progress (tool calls, footer) on stderr. Rendered from the engine's
//!   own events, which the client hands out in process until the front ends
//!   render the protocol's items.
//! - [`Output::Events`] (`koda exec --json`): stdout carries only JSON-RPC
//!   notifications from the client protocol (`koda-protocol`), one per
//!   line, as the host sends them: `session/started`, the turn and its
//!   items, each server request followed by `serverRequest/resolved` with
//!   the answer Koda took, and `exec/summary` last. Deltas only with
//!   `--json-deltas`.

use crate::input;
use koda_client::events::{Outgoing, usage_of};
use koda_client::protocol::initialize::ClientInfo;
use koda_client::protocol::server::ExecSummary;
use koda_client::protocol::session::{ApprovalPolicy, SessionStartParams};
use koda_client::protocol::turn::{
    ImageInput, Turn, TurnInterruptParams, TurnStartParams, TurnStatus,
};
use koda_client::protocol::{ServerNotification, methods};
use koda_client::{InProcessClient, RawEvent, StartArgs};
use koda_core::config::KodaConfig;
use koda_core::db::Database;
use koda_core::engine::{EngineEvent, TurnEndReason, UsageTotals};
use koda_core::persistence::{Persistence, SessionUsage};

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

/// What a one-shot run writes to stdout. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// The reply as text.
    Text,
    /// The protocol's notifications, deltas included or not.
    Events {
        /// Whether to print `item/*/delta` notifications too.
        deltas: bool,
    },
}

/// One one-shot run.
#[derive(Debug, Clone)]
pub struct Run {
    /// The prompt.
    pub prompt: String,
    /// What goes to stdout.
    pub output: Output,
    /// Where to write the last reply (`koda exec -o`).
    pub last_message: Option<PathBuf>,
}

/// Run a single prompt and exit. `resume` is an earlier session's full id;
/// without one a new session starts. Returns the process exit code (0 =
/// success).
pub async fn run_headless(
    project_root: PathBuf,
    config: KodaConfig,
    db: Database,
    resume: Option<String>,
    run: Run,
) -> Result<i32> {
    // Process @file references and images
    let processed = input::process_input(&run.prompt, &project_root);
    let user_message = if let Some(context) = input::format_context_files(&processed.context_files)
    {
        format!("{}\n\n{context}", processed.prompt)
    } else {
        processed.prompt.clone()
    };
    let images = processed
        .images
        .into_iter()
        .map(|image| ImageInput {
            media_type: image.media_type,
            data: image.base64,
        })
        .collect();

    let provider = config.provider_type.meta().name;
    let mut client = InProcessClient::start(StartArgs {
        config,
        project_root,
        db: db.clone(),
        raw_events: true,
        client_info: ClientInfo {
            name: "koda-exec".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
    })
    .await?;
    let mut raw = client
        .take_raw_events()
        .context("the client gave no engine events")?;
    let mut view = View::new(run.output);

    let opened = client
        .request::<methods::SessionStart>(SessionStartParams {
            session_id: resume,
            approval_policy: Some(ApprovalPolicy::Never),
        })
        .await?;
    let session_id = opened.session.id;
    let model = opened.session.model;
    // What starting the session said: `session/started`, and the context
    // window Koda assumed when the server didn't report one.
    while let Some(message) = client.try_next_event() {
        view.before_turn(&message);
    }

    // From here a Ctrl+C asks the turn to stop instead of killing Koda.
    let mut ctrl_c = CtrlC::listen()?;
    let started = Instant::now();
    let turn = client
        .request::<methods::TurnStart>(TurnStartParams {
            session_id: session_id.clone(),
            input: user_message,
            images,
            client_message_id: None,
            mode: None,
        })
        .await?
        .turn;
    let requests = client.request_handle();
    let mut interrupt = None;
    let completed: Turn = loop {
        tokio::select! {
            // Engine events first, so text keeps up with the protocol.
            biased;
            Some(event) = raw.recv() => view.raw(event),
            message = client.next_event() => match message {
                Some(message) => {
                    if let Some(turn) = view.protocol(message) {
                        break turn;
                    }
                }
                None => anyhow::bail!("Koda's host stopped during the turn"),
            },
            () = ctrl_c.recv(), if interrupt.is_none() => {
                eprintln!("\n\x1b[33m\u{26a0} Interrupted\x1b[0m");
                // Answered once the turn has stopped. The turn bounds its
                // own wind-down, so this can't hang the run.
                let requests = requests.clone();
                let params = TurnInterruptParams {
                    session_id: session_id.clone(),
                    turn_id: turn.id.clone(),
                };
                interrupt = Some(tokio::spawn(async move {
                    requests.request::<methods::TurnInterrupt>(params).await
                }));
            }
        }
    };
    // The engine's events for the turn were all sent before its end.
    while let Ok(event) = raw.try_recv() {
        view.raw(event);
    }
    let elapsed_ms = started.elapsed().as_millis() as u64;

    let response = db
        .last_assistant_message(&session_id)
        .await
        .unwrap_or_default();
    if let Some(path) = &run.last_message
        && let Err(e) = std::fs::write(path, &response)
    {
        eprintln!(
            "Error: couldn't write the last message to {}: {e}",
            path.display()
        );
    }

    // A turn that returned an error, such as one dropped where it stood
    // after a cancel, says so on `turn/completed`.
    let result: Result<()> = match &completed.error {
        Some(error) if completed.status != TurnStatus::Failed => {
            Err(anyhow::anyhow!(error.message.clone()))
        }
        _ => Ok(()),
    };
    if let Output::Events { .. } = run.output {
        // Usage comes from the DB rather than the `Footer` event: the
        // footer only fires on a clean finish, while every persisted
        // assistant message (sub-agents included) carries its tokens,
        // so the totals stay right for errored and cancelled turns too.
        let usage = db
            .session_token_usage(&session_id)
            .await
            .unwrap_or_default();
        let usage_by_agent = db
            .session_usage_by_agent(&session_id)
            .await
            .unwrap_or_default();
        let outcome = RunOutcome {
            result: &result,
            session_id: &session_id,
            provider,
            model: &model,
            response: &response,
            elapsed_ms,
            usage: &usage,
            usage_by_agent: &usage_by_agent,
            stats: &view.stats,
        };
        view.print(&Outgoing::Notification(ServerNotification::ExecSummary(
            exec_summary(outcome),
        )));
    }
    client.shutdown().await;

    match result {
        // An early stop already printed its warning; just fail the exit code.
        Ok(()) => Ok(i32::from(run_failure(&Ok(()), &view.stats).is_some())),
        Err(e) => {
            eprintln!("Error: {e}");
            Ok(1)
        }
    }
}

/// Ctrl+C, listened for from the moment it is made, so a signal that comes
/// before the first wait is kept instead of killing the process.
struct CtrlC {
    #[cfg(unix)]
    signal: tokio::signal::unix::Signal,
}

impl CtrlC {
    fn listen() -> Result<Self> {
        Ok(Self {
            #[cfg(unix)]
            signal: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .context("couldn't listen for Ctrl+C")?,
        })
    }

    async fn recv(&mut self) {
        #[cfg(unix)]
        {
            self.signal.recv().await;
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

// ---------------------------------------------------------------------------
// Run summary: `exec/summary`
// ---------------------------------------------------------------------------

/// Per-run counters the sink accumulates from the event stream.
#[derive(Debug, Clone, Default, PartialEq)]
struct RunStats {
    /// Top-level tool calls by tool name.
    tool_calls: BTreeMap<String, u64>,
    /// Tool calls made by sub-agents, by tool name.
    sub_agent_tool_calls: BTreeMap<String, u64>,
    /// Reason from the top-level `TurnEnd`.
    turn_end: Option<TurnEndReason>,
}

/// Everything a summary needs once the turn is over.
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

/// Why the run failed, if it did. A turn the engine cut short on a warning
/// (provider server error, image rejection, dropped connection) returns
/// `Ok` but ends with `TurnEnd::Error` and no reply, so it counts too.
fn run_failure(result: &Result<()>, stats: &RunStats) -> Option<String> {
    match (result, &stats.turn_end) {
        (Err(e), _) => Some(format!("{e:#}")),
        (Ok(()), Some(TurnEndReason::Error { message })) => Some(message.clone()),
        (Ok(()), _) => None,
    }
}

/// `koda exec --json`'s last line.
fn exec_summary(outcome: RunOutcome<'_>) -> ExecSummary {
    let failure = run_failure(outcome.result, outcome.stats);
    let usage = |u: &SessionUsage| usage_of(&UsageTotals::from(u));
    ExecSummary {
        session_id: outcome.session_id.to_string(),
        success: failure.is_none(),
        error: failure,
        provider: outcome.provider.to_string(),
        model: outcome.model.to_string(),
        response: outcome.response.to_string(),
        turn_status: outcome.stats.turn_end.as_ref().map(|reason| match reason {
            TurnEndReason::Complete => TurnStatus::Completed,
            TurnEndReason::Cancelled => TurnStatus::Interrupted,
            _ => TurnStatus::Failed,
        }),
        duration_ms: outcome.elapsed_ms,
        usage: usage(outcome.usage),
        usage_by_agent: outcome
            .usage_by_agent
            .iter()
            .map(|(agent, u)| (agent.clone(), usage(u)))
            .collect(),
        tool_calls: outcome.stats.tool_calls.clone(),
        sub_agent_tool_calls: outcome.stats.sub_agent_tool_calls.clone(),
    }
}

// ---------------------------------------------------------------------------
// View — what the run prints, and the counts its summary reports
// ---------------------------------------------------------------------------

struct View {
    output: Output,
    stats: RunStats,
}

impl View {
    fn new(output: Output) -> Self {
        Self {
            output,
            stats: RunStats::default(),
        }
    }

    /// Write one protocol message to stdout. `println!` goes through Rust's
    /// line-buffered stdout, so it reaches a piped reader as soon as it ends.
    fn print(&self, message: &Outgoing) {
        if let Output::Events { deltas: false } = self.output
            && message.is_delta()
        {
            return;
        }
        match message.to_json() {
            Ok(text) => println!("{text}"),
            Err(e) => tracing::warn!("exec --json: failed to serialize a message: {e}"),
        }
    }

    /// What starting the session said. Text shows its warnings on stderr,
    /// like every other notice: stdout carries only the answer.
    fn before_turn(&self, message: &Outgoing) {
        match (self.output, message) {
            (Output::Events { .. }, _) => self.print(message),
            (Output::Text, Outgoing::Notification(ServerNotification::Warning(w))) => {
                eprintln!("\x1b[33m  \u{26a0} {}\x1b[0m", w.message);
            }
            (Output::Text, _) => {}
        }
    }

    /// One protocol message during the turn: printed for `--json`. Returns
    /// the turn once it has completed.
    fn protocol(&self, message: Outgoing) -> Option<Turn> {
        if let Output::Events { .. } = self.output {
            self.print(&message);
        }
        match message {
            Outgoing::Notification(ServerNotification::TurnCompleted(done)) => Some(done.turn),
            _ => None,
        }
    }

    /// One engine event: counted for the summary, and rendered for text.
    fn raw(&mut self, raw: RawEvent) {
        let RawEvent { parent, event, .. } = raw;
        self.record(parent.as_deref(), &event);
        match self.output {
            Output::Events { .. } => {}
            // A sub-agent's results aren't shown: its answer is.
            Output::Text
                if parent.is_some() && matches!(event, EngineEvent::ToolCallResult { .. }) => {}
            Output::Text => self.render(event),
        }
    }

    fn record(&mut self, parent_id: Option<&str>, event: &EngineEvent) {
        let stats = &mut self.stats;
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
            EngineEvent::TurnEnd { reason, .. } if parent_id.is_none() => {
                stats.turn_end = Some(reason.clone());
            }
            _ => {}
        }
    }

    /// Human-readable rendering for text and summary output. The streamed
    /// reply goes to stdout only for text; with a summary, stdout is
    /// reserved for it.
    fn render(&self, event: EngineEvent) {
        let reply_on_stdout = self.output == Output::Text;
        match event.notice().unwrap_or(event) {
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
            // A call that never ran was shown as its rejection or block.
            EngineEvent::ToolCallResult { outcome, .. } if !outcome.ran() => {}
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_start(name: &str, is_sub_agent: bool) -> EngineEvent {
        EngineEvent::ToolCallStart {
            id: format!("call-{name}"),
            name: name.into(),
            args: serde_json::json!({}),
            is_sub_agent,
        }
    }

    #[test]
    fn stats_split_top_level_and_sub_agent_tool_calls() {
        let mut view = View::new(Output::Text);
        let mut emit = |parent: Option<&str>, event| {
            view.record(parent, &event);
        };
        emit(None, tool_start("Read", false));
        emit(None, tool_start("Read", false));
        emit(None, tool_start("Edit", false));
        emit(Some("call-task"), tool_start("Grep", true));
        emit(
            None,
            EngineEvent::TurnEnd {
                turn_id: "t1".into(),
                reason: TurnEndReason::Complete,
            },
        );

        let stats = view.stats;
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
    fn exec_summary_carries_the_run_and_its_metrics() {
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
        let summary = exec_summary(RunOutcome {
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

        assert!(summary.success);
        assert_eq!(summary.error, None);
        assert_eq!(summary.response, "done");
        assert_eq!(summary.session_id, "s1");
        assert_eq!(summary.model, "mock-model");
        assert_eq!(summary.duration_ms, 42);
        assert_eq!(summary.turn_status, Some(TurnStatus::Completed));
        assert_eq!(summary.usage.input_tokens, 120);
        assert_eq!(summary.usage.model_calls, 2);
        assert_eq!(summary.usage_by_agent["main"].output_tokens, 30);
        assert_eq!(summary.tool_calls["Read"], 2);
    }

    #[test]
    fn exec_summary_reports_errors() {
        let result: Result<()> = Err(anyhow::anyhow!("provider exploded"));
        let summary = exec_summary(RunOutcome {
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
        assert!(!summary.success);
        assert_eq!(summary.error.as_deref(), Some("provider exploded"));
        assert_eq!(summary.turn_status, None);
    }

    /// A turn the engine ended early on a warning returns `Ok` but carries
    /// `TurnEnd::Error`; the summary must report failure with that message
    /// (server error, image rejection, dropped connection all take this path).
    #[test]
    fn exec_summary_reports_early_stop_as_failure() {
        for message in [
            "Provider returned a server error: 500",
            "This model rejected the image attachment",
            "Stream failed mid-reply \u{2014} turn discarded (reset)",
        ] {
            let stats = RunStats {
                turn_end: Some(TurnEndReason::Error {
                    message: message.to_string(),
                }),
                ..Default::default()
            };
            let result: Result<()> = Ok(());
            let summary = exec_summary(RunOutcome {
                result: &result,
                session_id: "s1",
                provider: "mock",
                model: "mock-model",
                response: "",
                elapsed_ms: 0,
                usage: &SessionUsage::default(),
                usage_by_agent: &[],
                stats: &stats,
            });
            assert!(!summary.success, "{message}");
            assert_eq!(summary.error.as_deref(), Some(message));
            assert_eq!(summary.turn_status, Some(TurnStatus::Failed));
            assert_eq!(run_failure(&result, &stats).as_deref(), Some(message));
        }
    }

    #[test]
    fn cancelled_turn_is_not_a_failure() {
        let stats = RunStats {
            turn_end: Some(TurnEndReason::Cancelled),
            ..Default::default()
        };
        assert_eq!(run_failure(&Ok(()), &stats), None);
    }
}
