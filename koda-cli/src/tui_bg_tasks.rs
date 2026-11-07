//! Cancel-all for background work, used by Esc and Ctrl+C.
//!
//! Background work is the sub-agent running inside the current turn
//! ([`koda_core::child_agent::ChildAgentRegistry`]) and background
//! shell processes ([`koda_core::tools::bg_process::BgRegistry`],
//! spawned via `Bash { background: true }`, which outlive the turn).
//! Both show in the activity overlay above the status bar, and
//! [`cancel_all_bg_work`] stops all of them at once.
//!
//! There is no command to stop one entry. `/cancel <id>` did that
//! until 2026-11-06; it only ran at idle, when no sub-agent exists, so
//! its `agent:N` form could never reach one, and stopping a single
//! background process is a request to the model ("stop the dev
//! server"), which knows the PID it started.

use crate::scroll_buffer::ScrollBuffer;
use crate::tui_output;

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
    bg_agents: &koda_core::child_agent::ChildAgentRegistry,
    bg_processes: &koda_core::tools::bg_process::BgRegistry,
    child_activity: &mut crate::child_activity::ChildActivityTracker,
) -> (usize, usize) {
    use koda_core::child_agent::AgentStatus;

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
                child_activity.mark_cancelling(**id);
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
    bg_agents: &koda_core::child_agent::ChildAgentRegistry,
    bg_processes: &koda_core::tools::bg_process::BgRegistry,
) -> usize {
    use koda_core::child_agent::AgentStatus;
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
    use koda_core::child_agent::{AgentStatus, ChildAgentRegistry, ChildStatusEmitter};
    use koda_core::tools::bg_process::BgRegistry;
    use std::sync::Arc;
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

    /// Register a sub-agent entry the way the dispatch path does and
    /// keep it alive for the test. Returns `(emitter, guard)`: drive
    /// the status through the emitter, and hold the guard so the entry
    /// stays registered.
    fn register_entry(
        reg: &Arc<ChildAgentRegistry>,
        agent_name: &str,
        prompt: &str,
    ) -> (
        ChildStatusEmitter,
        koda_core::child_agent::FgRegistrationGuard,
    ) {
        let parent = CancellationToken::new();
        reg.register_fg_with_emitter(agent_name, prompt, &parent, None)
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

    // ── cancel_all_bg_work / active_bg_count (#1200) ─────────────────

    /// Empty registries: `cancel_all_bg_work` returns (0, 0) and
    /// emits no message. The buffer must stay clean so the idle
    /// Ctrl+C path can fall through to its textarea-clear behaviour
    /// without leaving a phantom "cancelled 0" line in scrollback.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_all_bg_work_empty_is_silent() {
        let mut buf = ScrollBuffer::new(64);
        let reg = Arc::new(ChildAgentRegistry::new());
        let procs = BgRegistry::new();
        let (a, p) = cancel_all_bg_work(
            &mut buf,
            &reg,
            &procs,
            &mut crate::child_activity::ChildActivityTracker::default(),
        );
        assert_eq!((a, p), (0, 0));
        assert!(
            buffer_text(&buf).is_empty(),
            "empty registries must not emit a status line"
        );
    }

    /// Multiple running agents are all cancelled in one call.
    /// This is the in-process equivalent of "user hit Ctrl+C and
    /// every bg agent should die".
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_all_bg_work_cancels_every_running_agent() {
        let mut buf = ScrollBuffer::new(64);
        let reg = Arc::new(ChildAgentRegistry::new());
        let procs = BgRegistry::new();

        let (status1, _g1) = register_entry(&reg, "explore", "prompt 1");
        let (status2, _g2) = register_entry(&reg, "task", "prompt 2");
        // Bring them out of Pending so they show up as Running in
        // the snapshot filter.
        status1.send(AgentStatus::Running { iter: 1 });
        status2.send(AgentStatus::Running { iter: 1 });

        let (a, p) = cancel_all_bg_work(
            &mut buf,
            &reg,
            &procs,
            &mut crate::child_activity::ChildActivityTracker::default(),
        );
        assert_eq!((a, p), (2, 0));
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
        let reg = Arc::new(ChildAgentRegistry::new());
        let procs = BgRegistry::new();

        let (status, _guard) = register_entry(&reg, "explore", "hi");
        status.send(AgentStatus::Running { iter: 1 });
        let _pid = spawn_sleep_in_registry(&procs);

        let (a, p) = cancel_all_bg_work(
            &mut buf,
            &reg,
            &procs,
            &mut crate::child_activity::ChildActivityTracker::default(),
        );
        assert_eq!((a, p), (1, 1));
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
        let reg = Arc::new(ChildAgentRegistry::new());
        let procs = BgRegistry::new();

        let (status, _guard) = register_entry(&reg, "explore", "hi");
        // Drive to Completed terminal state.
        status.send(AgentStatus::Completed {
            summary: "done".to_string(),
        });

        let (a, p) = cancel_all_bg_work(
            &mut buf,
            &reg,
            &procs,
            &mut crate::child_activity::ChildActivityTracker::default(),
        );
        assert_eq!((a, p), (0, 0));
        assert!(buffer_text(&buf).is_empty());
    }

    /// `active_bg_count` matches what the idle Ctrl+C handler will
    /// see when deciding whether to cancel bg work or fall through
    /// to the textarea-clear path. Counts must exclude completed
    /// entries (same filter as `cancel_all_bg_work`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn active_bg_count_excludes_completed() {
        let reg = Arc::new(ChildAgentRegistry::new());
        let procs = BgRegistry::new();
        assert_eq!(active_bg_count(&reg, &procs), 0);

        let (s1, _g1) = register_entry(&reg, "a", "p1");
        s1.send(AgentStatus::Running { iter: 1 });
        let (s2, _g2) = register_entry(&reg, "b", "p2");
        s2.send(AgentStatus::Completed {
            summary: "x".to_string(),
        });

        assert_eq!(
            active_bg_count(&reg, &procs),
            1,
            "only the running agent should count"
        );
    }
}
