//! Live tracker for background-work activity (#1210).
//!
//! Stateful counterpart to the pure-render
//! [`crate::widgets::child_activity_overlay`] widget. Absorbs
//! `ChildAgentActivity` and `ChildTaskUpdate` engine events into a
//! per-task last-activity map, then projects (alongside the engine's
//! `ChildAgentRegistry` + `BgRegistry` snapshots) into a flat list of
//! [`crate::widgets::child_activity_overlay::ActivityRow`]s the widget
//! can render.
//!
//! ## Why a separate module
//!
//! Same separation as `queue_lanes.rs` (state) vs
//! `widgets::queue_preview` (render): the widget is a pure renderer
//! with zero engine dependencies, the tracker holds frame-spanning
//! state and bridges the engine event stream to the render layer.
//! Keeps the widget testable without faking up engine snapshots, and
//! keeps the tracker testable without spinning up ratatui buffers.
//!
//! ## Lifecycle
//!
//! 1. Construct empty (`ChildActivityTracker::default()`) at TUI start.
//! 2. On every `EngineEvent::ChildAgentActivity`, call
//!    [`ChildActivityTracker::record_activity`].
//! 3. On every `EngineEvent::ChildTaskUpdate`, call
//!    [`ChildActivityTracker::record_status`].
//! 4. Each frame, call [`ChildActivityTracker::build_rows`] with the
//!    current registry snapshots to get visible rows + total count.
//!
//! Pruning is implicit: rows are derived from the registry snapshot,
//! so terminal tasks fall off the overlay as soon as the registry
//! drops them. The tracker's internal map is bounded by registry
//! membership at projection time (see `build_rows`).

use crate::widgets::child_activity_overlay::{ActivityRow, ActivityStatus, MAX_VISIBLE};
use koda_core::child_agent::{AgentStatus, ChildTaskSnapshot};
use koda_core::engine::event::ChildAgentActivityKind;
use koda_core::tools::bg_process::{BgProcessSnapshot, BgProcessStatus};
use std::collections::HashMap;
use std::time::Duration;

/// Per-task last-known activity description. Populated from
/// `ChildAgentActivity` events. Bounded by registry membership: rows
/// for task ids no longer in the agent registry are dropped at
/// projection time.
#[derive(Debug, Clone)]
struct LastActivity {
    /// Pre-formatted single-line activity preview (e.g. "Read foo.rs",
    /// "Bash cargo test", info-line message). Truncated for display
    /// downstream — stored verbatim here.
    description: String,
}

/// Stateful tracker. Cheap to construct; carry one as a field on
/// `TuiContext`.
#[derive(Debug, Clone, Default)]
pub struct ChildActivityTracker {
    /// Latest activity per agent task_id. Process tasks have no
    /// activity stream (they're shells, not LLM agents) — their
    /// "activity" is the static command string from
    /// [`BgProcessSnapshot::command`], computed at projection time.
    per_agent: HashMap<u32, LastActivity>,
    /// Tasks the user (or the engine) has fired the cancel token on
    /// but the future hasn't observed yet. Distinguishes "you asked
    /// to cancel" from "still running" so the overlay icon flips
    /// red the moment the cancel is requested, not when it lands.
    cancelling: std::collections::HashSet<u32>,
}

impl ChildActivityTracker {
    /// Absorb a `ChildAgentActivity` event. Idempotent — re-recording
    /// the same activity overwrites the previous entry.
    pub fn record_activity(&mut self, task_id: u32, kind: &ChildAgentActivityKind) {
        let description = match kind {
            ChildAgentActivityKind::ToolStart { summary, .. } => summary.clone(),
            ChildAgentActivityKind::ToolEnd { tool_name, success } => {
                // ToolEnd would normally be a no-op (we want to keep
                // showing the most recent ToolStart, not "Read foo
                // ✓"), but if it's a *failure* we surface that so the
                // user sees something went wrong without reaching for
                // the post-completion bullets. Successes leave the
                // existing ToolStart in place.
                if *success {
                    tracing::debug!(
                        target: "koda_cli::diag::child_activity",
                        stage = "record_activity",
                        task_id = task_id,
                        kind = "ToolEnd(success)",
                        "no-op (preserving prior ToolStart)"
                    );
                    return;
                }
                format!("{tool_name} \u{2717}")
            }
            ChildAgentActivityKind::Info { message } => message.clone(),
            // Forward-compat: future activity kinds are dropped from
            // the live overlay until we know how to render them. Same
            // shape as the success ToolEnd case above (#1224).
            _ => {
                tracing::debug!(
                    target: "koda_cli::diag::child_activity",
                    stage = "record_activity",
                    task_id = task_id,
                    "unknown ChildAgentActivityKind variant \u{2014} dropping"
                );
                return;
            }
        };
        tracing::debug!(
            target: "koda_cli::diag::child_activity",
            stage = "record_activity",
            task_id = task_id,
            description = %description,
            "per_agent updated"
        );
        self.per_agent.insert(task_id, LastActivity { description });
    }

    /// Mark a task as cancelling. Called when the user fires
    /// `/cancel agent:N` or Esc-cancel-all (#1200). The icon flips
    /// red until the registry snapshot transitions the task to a
    /// terminal status (at which point it falls off the overlay
    /// entirely).
    pub fn mark_cancelling(&mut self, task_id: u32) {
        self.cancelling.insert(task_id);
    }

    /// Forget per-task state for a terminal task. Called by
    /// `build_rows` for any tracker entry not present in the agent
    /// registry (i.e. the registry has dropped it after completion
    /// or cancellation cleanup).
    fn forget(&mut self, task_id: u32) {
        self.per_agent.remove(&task_id);
        self.cancelling.remove(&task_id);
    }

    /// Project current state + registry snapshots into renderable
    /// rows. Returns `(visible_rows, total_count)` matching the
    /// widget's `ChildActivityOverlay::new` signature.
    ///
    /// Sort order: agents first (sorted by task_id ascending, so the
    /// fan-out spawn order is preserved), then processes (sorted by
    /// pid ascending). Agents come first because they're the more
    /// common subject of the user's attention during a multi-agent
    /// wait — fan-out workflows are the headline use case for this
    /// overlay (#1201).
    ///
    /// Side effect: prunes per-agent activity entries whose task_id
    /// is no longer in the agent registry. Bounds the internal map
    /// so it can't grow unbounded across a long session of
    /// fan-out → drain cycles.
    pub fn build_rows(
        &mut self,
        agents: &[ChildTaskSnapshot],
        processes: &[BgProcessSnapshot],
    ) -> (Vec<ActivityRow>, usize) {
        // ── Prune stale tracker entries (registry has dropped these tasks) ──
        let live_ids: std::collections::HashSet<u32> = agents.iter().map(|a| a.task_id).collect();
        let stale: Vec<u32> = self
            .per_agent
            .keys()
            .copied()
            .filter(|id| !live_ids.contains(id))
            .collect();
        for id in stale {
            self.forget(id);
        }

        // ── Build agent rows (skip terminal statuses) ──
        let mut agent_rows: Vec<(u32, ActivityRow)> = agents
            .iter()
            .filter_map(|s| {
                let status = match s.status {
                    AgentStatus::Pending => ActivityStatus::Pending,
                    AgentStatus::Running { .. } => {
                        if self.cancelling.contains(&s.task_id) {
                            ActivityStatus::Cancelling
                        } else {
                            ActivityStatus::Running
                        }
                    }
                    // Terminal statuses are excluded from the live
                    // overlay — they're a "done" surface, not a
                    // "what's happening" surface. The registry will
                    // drop them shortly anyway.
                    AgentStatus::Cancelled
                    | AgentStatus::Completed { .. }
                    | AgentStatus::Errored { .. } => return None,
                    // Forward-compat: an unknown future status is
                    // treated as terminal so the overlay never
                    // surfaces a row we can't classify (#1224). Safer
                    // than guessing Running and getting a stuck row.
                    _ => return None,
                };
                let activity = self
                    .per_agent
                    .get(&s.task_id)
                    .map(|a| a.description.clone());
                Some((
                    s.task_id,
                    ActivityRow {
                        icon: "\u{1f916}", // 🤖
                        label: s.agent_name.clone(),
                        age: format_age(s.age),
                        activity,
                        status,
                    },
                ))
            })
            .collect();
        agent_rows.sort_by_key(|(id, _)| *id);

        // ── Build process rows (skip terminal statuses) ──
        let mut process_rows: Vec<(u32, ActivityRow)> = processes
            .iter()
            .filter_map(|p| {
                let status = match p.status {
                    BgProcessStatus::Running => ActivityStatus::Running,
                    // Killed/Exited are terminal — excluded for the
                    // same reason as terminal agent statuses.
                    BgProcessStatus::Killed | BgProcessStatus::Exited { .. } => return None,
                };
                Some((
                    p.pid,
                    ActivityRow {
                        icon: "\u{1f41a}", // 🐚
                        label: format!("process:{}", p.pid),
                        age: format_age(p.age),
                        // Processes have no activity event stream;
                        // surface the spawn command verbatim so the
                        // user knows which `cargo build` is the slow
                        // one when there are several.
                        activity: Some(p.command.clone()),
                        status,
                    },
                ))
            })
            .collect();
        process_rows.sort_by_key(|(pid, _)| *pid);

        let mut all: Vec<ActivityRow> = agent_rows.into_iter().map(|(_, r)| r).collect();
        all.extend(process_rows.into_iter().map(|(_, r)| r));

        let total = all.len();
        all.truncate(MAX_VISIBLE);
        tracing::debug!(
            target: "koda_cli::diag::child_activity",
            stage = "build_rows",
            agent_snapshot_len = agents.len(),
            tracker_state_len = self.per_agent.len(),
            total = total,
            visible = all.len(),
            first_activity = all.first().and_then(|r| r.activity.as_deref()).unwrap_or("<none>"),
            "build_rows result"
        );
        (all, total)
    }
}

/// Format an age into a compact string suitable for the overlay's
/// age column. Always includes sub-unit precision so the value
/// changes at least once per second while the sub-agent is alive
/// (#1360 \u2014 was `"1m"` for a full 60s interval, leaving the user
/// unable to tell if the pill was frozen).
///
/// | Duration       | Format    | Example   |
/// |----------------|-----------|-----------|
/// | `< 1s`         | `<1s`     | `<1s`     |
/// | `< 60s`        | `XXs`     | `42s`     |
/// | `< 10m`        | `XmXXs`   | `1m43s`   |
/// | `< 60m`        | `XXmXXs`  | `15m23s`  |
/// | `< 10h`        | `XhXXm`   | `1h23m`   |
/// | `\u{2265} 10h`        | `XXhXXm`  | `25h59m`  |
///
/// Width varies from 3 to 6 graphemes; the overlay's column is
/// padded to 6 cells so the `(AGE)` parens align across rows.
fn format_age(d: Duration) -> String {
    let secs = d.as_secs();
    if secs == 0 {
        "<1s".to_string()
    } else if secs < 60 {
        // Right-pad to 2 digits so single-digit seconds align with
        // double-digit ones (`\" 4s\"` vs `\"42s\"`).
        format!("{secs:>2}s")
    } else if secs < 3600 {
        // Always include the seconds component so the value ticks
        // every second (the whole point of #1360). Width is 5 chars
        // for single-digit minutes (`1m43s`), 6 for double-digit
        // (`15m23s`).
        let m = secs / 60;
        let s = secs % 60;
        format!("{m}m{s:02}s")
    } else {
        // At hour scale, second precision is irrelevant for the
        // "is it alive?" feedback loop \u2014 minutes still tick visibly
        // every 60s, and longer-running agents are usually waiting
        // on a slow tool (e.g. cargo build) where the user already
        // knows the situation. Drop seconds; keep minutes.
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!("{h}h{m:02}m")
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use koda_core::engine::event::ChildAgentActivityKind;

    fn agent(task_id: u32, name: &str, age_secs: u64, status: AgentStatus) -> ChildTaskSnapshot {
        ChildTaskSnapshot::for_testing(
            task_id,
            name.to_string(),
            "x".into(),
            Duration::from_secs(age_secs),
            status,
            None,
        )
    }

    fn process(pid: u32, command: &str, age_secs: u64) -> BgProcessSnapshot {
        BgProcessSnapshot {
            pid,
            command: command.to_string(),
            age: Duration::from_secs(age_secs),
            status: BgProcessStatus::Running,
            spawner: None,
        }
    }

    #[test]
    fn empty_tracker_with_empty_registry_yields_no_rows() {
        let mut t = ChildActivityTracker::default();
        let (rows, total) = t.build_rows(&[], &[]);
        assert!(rows.is_empty());
        assert_eq!(total, 0);
    }

    #[test]
    fn running_agent_without_recorded_activity_renders_with_no_activity() {
        let mut t = ChildActivityTracker::default();
        let snap = vec![agent(1, "explore", 5, AgentStatus::Running { iter: 1 })];
        let (rows, total) = t.build_rows(&snap, &[]);
        assert_eq!(total, 1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "explore");
        assert!(rows[0].activity.is_none());
        assert_eq!(rows[0].status, ActivityStatus::Running);
    }

    #[test]
    fn tool_start_event_populates_activity() {
        let mut t = ChildActivityTracker::default();
        t.record_activity(
            1,
            &ChildAgentActivityKind::ToolStart {
                tool_name: "Read".into(),
                summary: "Read src/auth.rs".into(),
            },
        );
        let snap = vec![agent(1, "explore", 5, AgentStatus::Running { iter: 2 })];
        let (rows, _) = t.build_rows(&snap, &[]);
        assert_eq!(rows[0].activity.as_deref(), Some("Read src/auth.rs"));
    }

    #[test]
    fn tool_end_success_does_not_overwrite_last_tool_start() {
        let mut t = ChildActivityTracker::default();
        t.record_activity(
            1,
            &ChildAgentActivityKind::ToolStart {
                tool_name: "Read".into(),
                summary: "Read foo.rs".into(),
            },
        );
        t.record_activity(
            1,
            &ChildAgentActivityKind::ToolEnd {
                tool_name: "Read".into(),
                success: true,
            },
        );
        let snap = vec![agent(1, "explore", 5, AgentStatus::Running { iter: 2 })];
        let (rows, _) = t.build_rows(&snap, &[]);
        // Successful end → preserve the in-progress description.
        assert_eq!(rows[0].activity.as_deref(), Some("Read foo.rs"));
    }

    #[test]
    fn tool_end_failure_surfaces_failure_marker() {
        let mut t = ChildActivityTracker::default();
        t.record_activity(
            1,
            &ChildAgentActivityKind::ToolStart {
                tool_name: "Bash".into(),
                summary: "Bash cargo test".into(),
            },
        );
        t.record_activity(
            1,
            &ChildAgentActivityKind::ToolEnd {
                tool_name: "Bash".into(),
                success: false,
            },
        );
        let snap = vec![agent(1, "verify", 12, AgentStatus::Running { iter: 3 })];
        let (rows, _) = t.build_rows(&snap, &[]);
        let activity = rows[0].activity.as_deref().unwrap();
        assert!(
            activity.contains("Bash") && activity.contains('\u{2717}'),
            "expected failure marker in activity: {activity:?}"
        );
    }

    #[test]
    fn info_event_overrides_previous_activity() {
        let mut t = ChildActivityTracker::default();
        t.record_activity(
            1,
            &ChildAgentActivityKind::ToolStart {
                tool_name: "Read".into(),
                summary: "Read foo.rs".into(),
            },
        );
        t.record_activity(
            1,
            &ChildAgentActivityKind::Info {
                message: "Cache hit, skipping".into(),
            },
        );
        let snap = vec![agent(1, "explore", 5, AgentStatus::Running { iter: 2 })];
        let (rows, _) = t.build_rows(&snap, &[]);
        assert_eq!(rows[0].activity.as_deref(), Some("Cache hit, skipping"));
    }

    /// #1354: tracker must accept the user's observed Info\u2192ToolStart
    /// sequence (worktree-isolation `Info` event followed by sub-agent
    /// `Glob`/`Read` ToolStart events). Bug 1 of #1349 reported the
    /// pill freezing on the `Info` text \u2014 if this test passes, the
    /// tracker logic is correct and the bug is downstream of
    /// `record_activity` (event delivery, not state mutation).
    #[test]
    fn tool_start_event_overrides_previous_info() {
        let mut t = ChildActivityTracker::default();
        t.record_activity(
            1,
            &ChildAgentActivityKind::Info {
                message: "explore: isolated in worktree".into(),
            },
        );
        t.record_activity(
            1,
            &ChildAgentActivityKind::ToolStart {
                tool_name: "Glob".into(),
                summary: "Glob src/**/*.rs".into(),
            },
        );
        let snap = vec![agent(1, "explore", 5, AgentStatus::Running { iter: 1 })];
        let (rows, _) = t.build_rows(&snap, &[]);
        assert_eq!(
            rows[0].activity.as_deref(),
            Some("Glob src/**/*.rs"),
            "#1354: ToolStart after Info must override the Info text in the pill"
        );
    }

    #[test]
    fn terminal_agents_are_excluded_from_overlay() {
        let mut t = ChildActivityTracker::default();
        let snap = vec![
            agent(1, "explore", 5, AgentStatus::Running { iter: 2 }),
            agent(
                2,
                "verify",
                7,
                AgentStatus::Completed {
                    summary: "done".into(),
                },
            ),
            agent(
                3,
                "lint",
                3,
                AgentStatus::Errored {
                    error: "boom".into(),
                },
            ),
            agent(4, "scan", 9, AgentStatus::Cancelled),
        ];
        let (rows, total) = t.build_rows(&snap, &[]);
        assert_eq!(total, 1, "only the running agent should count: {rows:?}");
        assert_eq!(rows[0].label, "explore");
    }

    #[test]
    fn cancelling_marker_flips_status_color() {
        let mut t = ChildActivityTracker::default();
        t.mark_cancelling(7);
        let snap = vec![agent(7, "explore", 5, AgentStatus::Running { iter: 2 })];
        let (rows, _) = t.build_rows(&snap, &[]);
        assert_eq!(rows[0].status, ActivityStatus::Cancelling);
    }

    #[test]
    fn stale_tracker_entries_pruned_when_registry_drops_task() {
        let mut t = ChildActivityTracker::default();
        t.record_activity(
            42,
            &ChildAgentActivityKind::ToolStart {
                tool_name: "Read".into(),
                summary: "stale".into(),
            },
        );
        t.mark_cancelling(42);
        // Registry no longer reports task 42 → tracker should prune.
        let _ = t.build_rows(&[], &[]);
        assert!(!t.per_agent.contains_key(&42));
        assert!(!t.cancelling.contains(&42));
    }

    #[test]
    fn agents_sorted_before_processes_in_render_order() {
        let mut t = ChildActivityTracker::default();
        let agents = vec![
            agent(2, "verify", 2, AgentStatus::Running { iter: 1 }),
            agent(1, "explore", 4, AgentStatus::Running { iter: 1 }),
        ];
        let procs = vec![process(99, "cargo build", 8)];
        let (rows, _) = t.build_rows(&agents, &procs);
        assert_eq!(rows[0].label, "explore", "task_id=1 should sort first");
        assert_eq!(rows[1].label, "verify", "task_id=2 should sort second");
        assert!(
            rows[2].label.starts_with("process:"),
            "processes should render after agents: {:?}",
            rows[2].label
        );
    }

    #[test]
    fn overflow_total_exceeds_visible_when_more_than_max_visible() {
        let mut t = ChildActivityTracker::default();
        let agents: Vec<_> = (0..(MAX_VISIBLE as u32 + 4))
            .map(|i| agent(i, &format!("ag{i}"), 1, AgentStatus::Running { iter: 1 }))
            .collect();
        let (rows, total) = t.build_rows(&agents, &[]);
        assert_eq!(rows.len(), MAX_VISIBLE);
        assert_eq!(total, MAX_VISIBLE + 4);
    }

    #[test]
    fn process_row_uses_command_as_activity() {
        let mut t = ChildActivityTracker::default();
        let procs = vec![process(123, "cargo test --release", 4)];
        let (rows, _) = t.build_rows(&[], &procs);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "process:123");
        assert_eq!(rows[0].activity.as_deref(), Some("cargo test --release"));
    }

    #[test]
    fn format_age_under_one_second() {
        assert_eq!(format_age(Duration::from_millis(500)), "<1s");
    }

    #[test]
    fn format_age_seconds_padded_to_two_digits() {
        assert_eq!(format_age(Duration::from_secs(4)), " 4s");
        assert_eq!(format_age(Duration::from_secs(42)), "42s");
    }

    /// #1360 regression: minute-scale ages MUST include the seconds
    /// component so the value ticks every second. Pre-fix, this
    /// returned `" 1m"` and `"15m"` \u2014 frozen for a full 60s interval
    /// while the user wondered if the sub-agent was alive.
    #[test]
    fn format_age_minutes_include_seconds() {
        assert_eq!(format_age(Duration::from_secs(60)), "1m00s");
        assert_eq!(format_age(Duration::from_secs(60 + 43)), "1m43s");
        assert_eq!(format_age(Duration::from_secs(9 * 60 + 59)), "9m59s");
        assert_eq!(format_age(Duration::from_secs(15 * 60 + 23)), "15m23s");
        assert_eq!(format_age(Duration::from_secs(59 * 60 + 59)), "59m59s");
    }

    /// #1360: at hour scale we drop seconds (irrelevant to the
    /// "is it alive?" feedback loop) but keep minutes so the value
    /// still ticks visibly every 60 seconds.
    #[test]
    fn format_age_hours_include_minutes_drop_seconds() {
        assert_eq!(format_age(Duration::from_secs(3600)), "1h00m");
        assert_eq!(format_age(Duration::from_secs(3600 + 23 * 60)), "1h23m");
        assert_eq!(format_age(Duration::from_secs(9 * 3600 + 59 * 60)), "9h59m");
        assert_eq!(
            format_age(Duration::from_secs(25 * 3600 + 59 * 60)),
            "25h59m"
        );
    }

    /// #1360: width contract the overlay column relies on. Widest
    /// possible output must fit the 6-cell padding in
    /// `child_activity_overlay`'s `format!(\" ({:>6}) \", row.age)`.
    #[test]
    fn format_age_width_never_exceeds_six_chars() {
        // Sample boundary values across all branches.
        for d in [
            Duration::from_millis(500),
            Duration::from_secs(1),
            Duration::from_secs(59),
            Duration::from_secs(60),
            Duration::from_secs(9 * 60 + 59),
            Duration::from_secs(59 * 60 + 59),
            Duration::from_secs(3600),
            Duration::from_secs(99 * 3600 + 59 * 60),
        ] {
            let s = format_age(d);
            assert!(
                s.chars().count() <= 6,
                "format_age({d:?}) = {s:?} exceeds 6-cell column width"
            );
        }
    }

    // ── PR-A of #1232 §1: foreground sub-agent overlay rows ─────────────

    /// A foreground sub-agent (`is_background == false`) registered
    /// in the registry must render an overlay row with the same
    /// shape as a background entry.
    ///
    /// **Why this is the headline test for PR-A**: the umbrella bug
    /// (#1232 §1) was "fg sub-agents have ZERO live progress in the
    /// master TUI." The overlay's `build_rows` is what the user
    /// actually sees; if a fg-tagged snapshot doesn't produce a row
    /// here, the engine-side wiring (`register_fg_with_emitter` +
    /// `FgForwardingSink`) is invisible end-to-end and the bug isn't
    /// fixed regardless of how clean the lower layers look.
    #[test]
    fn foreground_agent_renders_overlay_row_with_activity_snippet() {
        let mut t = ChildActivityTracker::default();
        let snap = vec![ChildTaskSnapshot::for_testing_fg(
            7,
            "explore".into(),
            "audit storage".into(),
            Duration::from_secs(12),
            AgentStatus::Running { iter: 3 },
            None,
        )];
        // Drive activity in via the same `record_activity` entrypoint
        // the inference loop uses for both fg and bg — the tracker
        // intentionally treats every `ChildAgentActivity` event the
        // same regardless of `is_background`, so this end-to-end
        // shape proves there's no fg-specific filter blocking the row.
        t.record_activity(
            7,
            &ChildAgentActivityKind::ToolStart {
                tool_name: "Read".into(),
                summary: "Read storage/mod.rs".into(),
            },
        );
        let (rows, total) = t.build_rows(&snap, &[]);
        assert_eq!(total, 1, "fg snapshot must produce exactly one overlay row");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.label, "explore", "label uses agent_name");
        assert_eq!(
            row.activity.as_deref(),
            Some("Read storage/mod.rs"),
            "fg row must show the same `last activity` snippet as bg \
             — the headline UX for #1232 §1"
        );
        assert_eq!(row.status, ActivityStatus::Running);
    }

    /// Symmetric removal: when a fg sub-agent's registry entry
    /// disappears (the `FgRegistrationGuard` dropped on the engine
    /// side), the overlay must drop the row on the next `build_rows`
    /// call. Issue acceptance criterion: "Removing the row when the
    /// sub-agent returns is symmetric with bg path."
    #[test]
    fn foreground_row_disappears_when_registry_entry_removed() {
        let mut t = ChildActivityTracker::default();
        let snap_running = vec![ChildTaskSnapshot::for_testing_fg(
            7,
            "explore".into(),
            "x".into(),
            Duration::from_secs(1),
            AgentStatus::Running { iter: 0 },
            None,
        )];
        t.record_activity(
            7,
            &ChildAgentActivityKind::ToolStart {
                tool_name: "Read".into(),
                summary: "Read foo".into(),
            },
        );
        let (rows, _) = t.build_rows(&snap_running, &[]);
        assert_eq!(rows.len(), 1, "row present while registered");

        // Simulate guard drop: registry no longer reports this task.
        let (rows_after, total_after) = t.build_rows(&[], &[]);
        assert_eq!(
            total_after, 0,
            "row must vanish when fg entry leaves the registry; \
             without this the overlay would show phantom rows for \
             every completed fg sub-agent"
        );
        assert!(rows_after.is_empty());
    }

    /// Mixed fg + bg snapshot: both render, both count toward `total`,
    /// neither shadows the other. Catches a regression where a future
    /// `is_background` filter accidentally hides one or the other.
    #[test]
    fn mixed_foreground_and_background_agents_both_render() {
        let mut t = ChildActivityTracker::default();
        let snap = vec![
            // bg first (id 1)
            agent(1, "task", 30, AgentStatus::Running { iter: 5 }),
            // fg next (id 2)
            ChildTaskSnapshot::for_testing_fg(
                2,
                "plan".into(),
                "y".into(),
                Duration::from_secs(10),
                AgentStatus::Running { iter: 1 },
                None,
            ),
        ];
        let (rows, total) = t.build_rows(&snap, &[]);
        assert_eq!(total, 2, "both fg and bg agents must be counted");
        assert_eq!(rows.len(), 2);
        // Sort by label for stability — build_rows sorts by task_id,
        // so order is bg(1) then fg(2), but we shouldn't bake that in.
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert!(labels.contains(&"task"), "bg agent row missing");
        assert!(labels.contains(&"plan"), "fg agent row missing");
    }
}
