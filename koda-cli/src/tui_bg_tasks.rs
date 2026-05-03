//! `/agents` and `/cancel <id>` slash commands — runtime view of
//! **all** background tasks (sub-agents *and* shell processes).
//!
//! ## Overview
//!
//! `/agents` lists every currently-tracked background task — both
//! background sub-agents from [`koda_core::bg_agent::BgAgentRegistry`]
//! (spawned via `InvokeAgent { background: true }`) and background
//! shell processes from [`koda_core::tools::bg_process::BgRegistry`]
//! (spawned via `Bash { background: true }`). They share a single
//! table because from the user's perspective they're the same
//! concept: "stuff Koda kicked off in the background that I'd like
//! to see, wait on, or cancel."
//!
//! `/cancel <id>` accepts the same prefixed forms as the LLM-facing
//! `CancelTask` tool ([`parse_task_id`]):
//!
//! - `agent:N` — fire a bg sub-agent's cancel token
//! - `process:N` — SIGTERM a bg shell process
//! - `N` (bare numeric) — back-compat with the original #1042 UX,
//!   treated as `agent:N`
//!
//! Together they close #996: "the model launched background work
//! and I have no way to see what it's doing or stop it."
//!
//! ## Why one renderer instead of two slash commands
//!
//! We considered `/agents` + `/processes`, but Codex / Gemini / Claude
//! Code all converge on a single "background work" surface. Splitting
//! by spawn mechanism would push the user to remember which command
//! launched which task — exactly the friction the unified view fixes.
//! The `TYPE` column distinguishes the two when it matters (e.g.
//! "did `cargo test` finish?" vs "is the explore agent done?").
//!
//! Foreground sub-agents (the synchronous `/agent <name>` switch in
//! [`crate::tui_wizards::handle_list_agents`]) don't appear here —
//! they block the conversation and are visible inline.
//!
//! ## Display
//!
//! Status icons follow Codex's `multi_agents::status_summary_spans`
//! palette so users coming from that ecosystem find them familiar:
//!
//! | Status      | Glyph | Color      |
//! |-------------|-------|------------|
//! | `Pending`   | `◐`   | cyan       |
//! | `Running`   | `▶`   | cyan bold  |
//! | `Cancelled` | `⊗`   | dim        |
//! | `Completed` | `✓`   | green      |
//! | `Errored`   | `✗`   | red        |
//! | `Killed`    | `⊗`   | dim        |  (process-only)
//! | `Exited(c)` | `✓/✗` | green/red  |  (process-only; ✗ when c≠0)
//!
//! ## Out of scope (deferred)
//!
//! - The "completed lingers 30s" UX polish — deferred to a polish
//!   PR. Drained results still inject into the conversation, so the
//!   user isn't missing info, just visual confirmation.
//! - Status-bar pill — Layer 3 / PR #1044.
//! - Per-iter `iter` updates — Layer 4 / PR #1058 (landed).
//!
//! [`parse_task_id`]: koda_core::tools::bg_task_tools::parse_task_id

use crate::scroll_buffer::ScrollBuffer;
use crate::tui_output;
use koda_core::tools::bg_task_tools::TaskId;

/// Render `/cancel <id>`. Routes to the right registry based on the
/// parsed [`TaskId`]:
///
/// - [`TaskId::Agent`] → [`BgAgentRegistry::cancel`]
/// - [`TaskId::Process`] → [`BgRegistry::kill`] (sends SIGTERM)
///
/// Both registries' cancel paths are idempotent (PR #1041's
/// `cancel_is_idempotent_while_pending` for agents; SIGTERM-on-already-
/// dead is a no-op for processes), so re-issuing on a still-running
/// cancelled task is harmless.
///
/// `task_id == None` means the user typed `/cancel` with no arg or
/// an arg that didn't parse — we report the usage error here rather
/// than at the parser layer (see `ReplAction::CancelBackgroundTask`'s
/// docstring for the rationale).
///
/// [`BgAgentRegistry::cancel`]: koda_core::bg_agent::BgAgentRegistry::cancel
/// [`BgRegistry::kill`]: koda_core::tools::bg_process::BgRegistry::kill
pub(crate) fn handle_cancel_background_task(
    buffer: &mut ScrollBuffer,
    bg_agents: &koda_core::bg_agent::BgAgentRegistry,
    bg_processes: &koda_core::tools::bg_process::BgRegistry,
    bg_activity: &mut crate::bg_activity::BgActivityTracker,
    task_id: Option<TaskId>,
) {
    let Some(id) = task_id else {
        tui_output::warn_msg(
            buffer,
            "Usage: /cancel <agent:id|process:id>  (ids are visible in the live bg-activity overlay)".into(),
        );
        return;
    };

    match id {
        TaskId::Agent(n) => {
            if bg_agents.cancel(n) {
                // Flip the overlay icon to red immediately so the user
                // sees their cancel landed (#1210). The registry status
                // doesn't transition to Cancelled until the inference
                // loop next checks the token — could be a few hundred
                // ms — and that lag without feedback feels broken.
                bg_activity.mark_cancelling(n);
                tui_output::ok_msg(
                    buffer,
                    format!("Cancellation requested for agent:{n}. Result will inject shortly."),
                );
            } else {
                tui_output::warn_msg(
                    buffer,
                    format!(
                        "No background sub-agent with id agent:{n}. Check the bg-activity overlay for active task ids."
                    ),
                );
            }
        }
        TaskId::Process(n) => {
            if bg_processes.kill(n) {
                tui_output::ok_msg(
                    buffer,
                    format!("SIGTERM sent to process:{n}. It should exit shortly."),
                );
            } else {
                tui_output::warn_msg(
                    buffer,
                    format!(
                        "No background process with id process:{n}. Check the bg-activity overlay for active task ids."
                    ),
                );
            }
        }
    }
}

/// Cancel every active background agent and SIGTERM every running
/// background process. Used by the global Ctrl+C path so users have
/// a single keystroke to stop *all* background work — see issue #1200.
///
/// Counts only entries that are still running/pending; already-finished
/// snapshots are skipped so we don't spam "Cancelled 0" messages when
/// the registries are full of completed (not-yet-reaped) entries.
///
/// Returns `(agents_cancelled, processes_killed)` so callers can decide
/// whether to emit a "nothing to do" message or fall through to
/// alternate Ctrl+C behaviour (e.g. textarea clear at idle).
pub(crate) fn cancel_all_bg_work(
    buffer: &mut ScrollBuffer,
    bg_agents: &koda_core::bg_agent::BgAgentRegistry,
    bg_processes: &koda_core::tools::bg_process::BgRegistry,
    bg_activity: &mut crate::bg_activity::BgActivityTracker,
) -> (usize, usize) {
    use koda_core::bg_agent::AgentStatus;

    // Collect ids first so we don't hold the registry lock across
    // `.cancel()` (which itself takes the lock). Snapshot is cheap
    // — copies are bounded by the small number of bg tasks.
    let agent_ids: Vec<u32> = bg_agents
        .snapshot()
        .into_iter()
        .filter(|s| matches!(s.status, AgentStatus::Pending | AgentStatus::Running { .. }))
        .map(|s| s.task_id)
        .collect();
    let proc_ids: Vec<u32> = bg_processes
        .snapshot()
        .into_iter()
        .filter(|s| {
            matches!(
                s.status,
                koda_core::tools::bg_process::BgProcessStatus::Running
            )
        })
        .map(|s| s.pid)
        .collect();

    let agents_cancelled = agent_ids
        .iter()
        .filter(|id| {
            let cancelled = bg_agents.cancel(**id);
            if cancelled {
                // Flip the overlay icon red right away so the user gets
                // immediate feedback (#1210); the registry status
                // transition to Cancelled lags by an inference
                // iteration.
                bg_activity.mark_cancelling(**id);
            }
            cancelled
        })
        .count();
    let processes_killed = proc_ids
        .iter()
        .filter(|pid| bg_processes.kill(**pid))
        .count();

    if agents_cancelled + processes_killed > 0 {
        let parts: Vec<String> = [
            (agents_cancelled > 0).then(|| format!("{agents_cancelled} agent(s)")),
            (processes_killed > 0).then(|| format!("{processes_killed} process(es)")),
        ]
        .into_iter()
        .flatten()
        .collect();
        tui_output::warn_msg(
            buffer,
            format!("\u{26d4} Cancelled {} background work", parts.join(" + ")),
        );
    }

    (agents_cancelled, processes_killed)
}

/// Snapshot count of *active* bg work (running agents + tracked
/// processes). Used by the idle Ctrl+C path to decide whether to
/// cancel bg work or fall back to the textarea-clear behaviour.
pub(crate) fn active_bg_count(
    bg_agents: &koda_core::bg_agent::BgAgentRegistry,
    bg_processes: &koda_core::tools::bg_process::BgRegistry,
) -> usize {
    use koda_core::bg_agent::AgentStatus;
    use koda_core::tools::bg_process::BgProcessStatus;
    let active_agents = bg_agents
        .snapshot()
        .into_iter()
        .filter(|s| matches!(s.status, AgentStatus::Pending | AgentStatus::Running { .. }))
        .count();
    let active_procs = bg_processes
        .snapshot()
        .into_iter()
        .filter(|s| matches!(s.status, BgProcessStatus::Running))
        .count();
    active_agents + active_procs
}

#[cfg(test)]
mod tests {
    use super::*;
    use koda_core::bg_agent::{AgentStatus, BgAgentRegistry, BgPayload};
    use koda_core::tools::bg_process::BgRegistry;
    use tokio::sync::{oneshot, watch};
    use tokio_util::sync::CancellationToken;

    /// Concatenate every line in the buffer into one searchable
    /// string. Style/color is stripped — we only assert content here.
    fn buffer_text(buffer: &ScrollBuffer) -> String {
        buffer
            .all_lines()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Build a registered bg-agent entry using only `BgAgentRegistry`'s
    /// **public** API. We can't use the in-crate `register_test*`
    /// helpers because they're `#[cfg(test)]`-gated to `koda-core`
    /// itself — those gates make them invisible to other crates'
    /// test builds.
    ///
    /// Returns `(task_id, result_sender, status_sender, cancel_observer)`
    /// so tests can drive the entry through any state. The
    /// `JoinHandle` we attach is a noop spawn — enough to satisfy
    /// `AbortOnDropHandle` without burning a tokio worker on real work.
    fn register_entry(
        reg: &BgAgentRegistry,
        agent_name: &str,
        prompt: &str,
    ) -> (
        u32,
        oneshot::Sender<Result<BgPayload, BgPayload>>,
        watch::Sender<AgentStatus>,
        CancellationToken,
    ) {
        let parent = CancellationToken::new();
        // Phase A1 of #996 added a `spawner: Option<u32>` to both
        // reserve() and attach(). The TUI test harness only ever
        // exercises the top-level path, so `None` is the right value.
        let r = reg.reserve(&parent, None);
        let task_id = r.task_id;
        let tx = r.tx;
        let status_tx = r.status_tx;
        let observer = r.cancel.clone();
        let noop = tokio::spawn(async {});
        reg.attach(
            task_id,
            agent_name,
            prompt,
            r.rx,
            r.cancel,
            r.status_rx,
            None,
            None,
            noop,
        );
        (task_id, tx, status_tx, observer)
    }

    /// Spawn a real child process for tests that need a live PID in
    /// the [`BgRegistry`]. We use `sleep` because it's universally
    /// available, exits cleanly on SIGTERM, and lets us observe
    /// kill / reap state without timing flakiness.
    ///
    /// `BgRegistry::insert` takes a `tokio::process::Child` (not the
    /// std variant) because the LLM-tool path needs async waits.
    /// Returns the OS pid — also the id we register under, since
    /// `BgRegistry::insert` doesn't allocate its own ids.
    fn spawn_sleep_in_registry(reg: &BgRegistry) -> u32 {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("60");
        let child = cmd.spawn().expect("spawn sleep");
        let pid = child.id().expect("sleep should have a pid before exit");
        reg.insert(pid, "sleep 60".to_string(), child, None);
        pid
    }

    // ── handle_cancel_background_task ──────────────────────────────────────────────

    /// Happy path for an agent: known `agent:N` → cancel fires,
    /// success message surfaces, and the underlying token observed
    /// the cancel.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_known_agent_id_reports_success_and_fires_token() {
        let mut buf = ScrollBuffer::new(64);
        let reg = BgAgentRegistry::new();
        let procs = BgRegistry::new();
        let (task_id, _tx, _status_tx, observer) = register_entry(&reg, "explore", "x");

        handle_cancel_background_task(&mut buf, &reg, &procs, &mut crate::bg_activity::BgActivityTracker::default(), Some(TaskId::Agent(task_id)));

        let text = buffer_text(&buf);
        assert!(
            text.contains(&format!("agent:{task_id}")),
            "success message should mention the prefixed id, got: {text}"
        );
        assert!(
            observer.is_cancelled(),
            "the cancel token should have been fired"
        );
    }

    /// Happy path for a process: known `process:N` → SIGTERM fires
    /// and the registry transitions the entry into `Killed`. The
    /// child eventually exits but we don't wait on it here — the
    /// reaper handles that.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_known_process_id_kills_and_reports_success() {
        let mut buf = ScrollBuffer::new(64);
        let reg = BgAgentRegistry::new();
        let procs = BgRegistry::new();
        let pid = spawn_sleep_in_registry(&procs);

        handle_cancel_background_task(&mut buf, &reg, &procs, &mut crate::bg_activity::BgActivityTracker::default(), Some(TaskId::Process(pid)));

        let text = buffer_text(&buf);
        assert!(
            text.contains(&format!("process:{pid}")),
            "success message should mention the prefixed id, got: {text}"
        );
        assert!(
            text.to_lowercase().contains("sigterm") || text.to_lowercase().contains("exit"),
            "expected SIGTERM acknowledgement, got: {text}"
        );
    }

    /// Unknown agent id → warn, don't crash. The user should learn
    /// the correct id (or that the task already finished) without us
    /// throwing or silently no-oping.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_unknown_agent_id_reports_helpful_error() {
        let mut buf = ScrollBuffer::new(64);
        let reg = BgAgentRegistry::new();
        let procs = BgRegistry::new();
        handle_cancel_background_task(&mut buf, &reg, &procs, &mut crate::bg_activity::BgActivityTracker::default(), Some(TaskId::Agent(999)));
        let text = buffer_text(&buf);
        assert!(
            text.contains("agent:999") && text.contains("bg-activity overlay"),
            "warn should name the missing prefixed id and point to the bg-activity overlay, got: {text}"
        );
    }

    /// Unknown process id → warn, don't crash. Same shape as the
    /// agent equivalent so users get a consistent error story.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_unknown_process_id_reports_helpful_error() {
        let mut buf = ScrollBuffer::new(64);
        let reg = BgAgentRegistry::new();
        let procs = BgRegistry::new();
        handle_cancel_background_task(&mut buf, &reg, &procs, &mut crate::bg_activity::BgActivityTracker::default(), Some(TaskId::Process(999_999)));
        let text = buffer_text(&buf);
        assert!(
            text.contains("process:999999") && text.contains("bg-activity overlay"),
            "warn should name the missing prefixed id and point to the bg-activity overlay, got: {text}"
        );
    }

    /// `None` id (user typed `/cancel` with no arg or unparseable
    /// arg) renders Usage — not a panic, and not a misleading "task
    /// 0 not found." The Usage line must mention both prefix forms
    /// so the user learns the new syntax.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_none_id_renders_usage_with_both_prefixes() {
        let mut buf = ScrollBuffer::new(64);
        let reg = BgAgentRegistry::new();
        let procs = BgRegistry::new();
        handle_cancel_background_task(&mut buf, &reg, &procs, &mut crate::bg_activity::BgActivityTracker::default(), None);
        let text = buffer_text(&buf);
        assert!(
            text.contains("Usage:") && text.contains("agent:") && text.contains("process:"),
            "None id should render a Usage: line with both prefixes, got: {text}"
        );
    }

    // ── cancel_all_bg_work / active_bg_count (#1200) ─────────────────

    /// Empty registries: `cancel_all_bg_work` returns (0, 0) and
    /// emits no message. The buffer must stay clean so the idle
    /// Ctrl+C path can fall through to its textarea-clear behaviour
    /// without leaving a phantom "cancelled 0" line in scrollback.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_all_bg_work_empty_is_silent() {
        let mut buf = ScrollBuffer::new(64);
        let reg = BgAgentRegistry::new();
        let procs = BgRegistry::new();
        let (a, p) = cancel_all_bg_work(&mut buf, &reg, &procs, &mut crate::bg_activity::BgActivityTracker::default());
        assert_eq!((a, p), (0, 0));
        assert!(
            buffer_text(&buf).is_empty(),
            "empty registries must not emit a status line"
        );
    }

    /// Multiple running agents are all cancelled in one call and
    /// each `BgAgentReservation::cancel` token observes the cascade.
    /// This is the in-process equivalent of "user hit Ctrl+C and
    /// every bg agent should die".
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_all_bg_work_cancels_every_running_agent() {
        let mut buf = ScrollBuffer::new(64);
        let reg = BgAgentRegistry::new();
        let procs = BgRegistry::new();

        let (_id1, _tx1, status1, observer1) = register_entry(&reg, "explore", "prompt 1");
        let (_id2, _tx2, status2, observer2) = register_entry(&reg, "task", "prompt 2");
        // Bring them out of Pending so they show up as Running in
        // the snapshot filter.
        status1.send(AgentStatus::Running { iter: 1 }).unwrap();
        status2.send(AgentStatus::Running { iter: 1 }).unwrap();

        let (a, p) = cancel_all_bg_work(&mut buf, &reg, &procs, &mut crate::bg_activity::BgActivityTracker::default());
        assert_eq!((a, p), (2, 0));
        assert!(observer1.is_cancelled(), "agent 1 should observe cascade");
        assert!(observer2.is_cancelled(), "agent 2 should observe cascade");
        let text = buffer_text(&buf);
        assert!(
            text.contains("Cancelled") && text.contains("2 agent(s)"),
            "summary line should report agent count, got: {text}"
        );
    }

    /// Mixed registries: agents + processes get cancelled and the
    /// summary line names both kinds. We use a real `sleep 60`
    /// child so `BgRegistry::kill` exercises its real SIGTERM path,
    /// not a stubbed one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_all_bg_work_handles_agents_and_processes() {
        let mut buf = ScrollBuffer::new(64);
        let reg = BgAgentRegistry::new();
        let procs = BgRegistry::new();

        let (_id, _tx, status, observer) = register_entry(&reg, "explore", "hi");
        status.send(AgentStatus::Running { iter: 1 }).unwrap();
        let _pid = spawn_sleep_in_registry(&procs);

        let (a, p) = cancel_all_bg_work(&mut buf, &reg, &procs, &mut crate::bg_activity::BgActivityTracker::default());
        assert_eq!((a, p), (1, 1));
        assert!(observer.is_cancelled());
        let text = buffer_text(&buf);
        assert!(
            text.contains("1 agent(s)") && text.contains("1 process(es)"),
            "summary should mention both kinds, got: {text}"
        );
    }

    /// Already-completed agents are skipped — we don't want Ctrl+C
    /// to spam "cancelled" for entries that finished naturally and
    /// just haven't been reaped yet.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_all_bg_work_skips_completed_entries() {
        let mut buf = ScrollBuffer::new(64);
        let reg = BgAgentRegistry::new();
        let procs = BgRegistry::new();

        let (_id, tx, status, observer) = register_entry(&reg, "explore", "hi");
        // Drive to Completed terminal state.
        status
            .send(AgentStatus::Completed {
                summary: "done".to_string(),
            })
            .unwrap();
        // Send a result so the entry looks fully resolved.
        let _ = tx.send(Ok(("done".to_string(), vec![])));

        let (a, p) = cancel_all_bg_work(&mut buf, &reg, &procs, &mut crate::bg_activity::BgActivityTracker::default());
        assert_eq!((a, p), (0, 0));
        assert!(
            !observer.is_cancelled(),
            "completed entries must not have their cancel token fired"
        );
        assert!(buffer_text(&buf).is_empty());
    }

    /// `active_bg_count` matches what the idle Ctrl+C handler will
    /// see when deciding whether to cancel bg work or fall through
    /// to the textarea-clear path. Counts must exclude completed
    /// entries (same filter as `cancel_all_bg_work`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn active_bg_count_excludes_completed() {
        let reg = BgAgentRegistry::new();
        let procs = BgRegistry::new();
        assert_eq!(active_bg_count(&reg, &procs), 0);

        let (_id1, _tx1, s1, _obs1) = register_entry(&reg, "a", "p1");
        s1.send(AgentStatus::Running { iter: 1 }).unwrap();
        let (_id2, tx2, s2, _obs2) = register_entry(&reg, "b", "p2");
        s2.send(AgentStatus::Completed {
            summary: "x".to_string(),
        })
        .unwrap();
        let _ = tx2.send(Ok(("x".to_string(), vec![])));

        assert_eq!(
            active_bg_count(&reg, &procs),
            1,
            "only the running agent should count"
        );
    }
}
