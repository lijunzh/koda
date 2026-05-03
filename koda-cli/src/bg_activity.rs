//! Live tracker for background-work activity (#1210).
//!
//! Stateful counterpart to the pure-render
//! [`crate::widgets::bg_activity_overlay`] widget. Absorbs
//! `BgChildActivity` and `BgTaskUpdate` engine events into a
//! per-task last-activity map, then projects (alongside the engine's
//! `BgAgentRegistry` + `BgRegistry` snapshots) into a flat list of
//! [`crate::widgets::bg_activity_overlay::ActivityRow`]s the widget
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
//! 1. Construct empty (`BgActivityTracker::default()`) at TUI start.
//! 2. On every `EngineEvent::BgChildActivity`, call
//!    [`BgActivityTracker::record_activity`].
//! 3. On every `EngineEvent::BgTaskUpdate`, call
//!    [`BgActivityTracker::record_status`].
//! 4. Each frame, call [`BgActivityTracker::build_rows`] with the
//!    current registry snapshots to get visible rows + total count.
//!
//! Pruning is implicit: rows are derived from the registry snapshot,
//! so terminal tasks fall off the overlay as soon as the registry
//! drops them. The tracker's internal map is bounded by registry
//! membership at projection time (see `build_rows`).

use crate::widgets::bg_activity_overlay::{ActivityRow, ActivityStatus, MAX_VISIBLE};
use koda_core::bg_agent::{AgentStatus, BgTaskSnapshot};
use koda_core::engine::event::BgChildActivityKind;
use koda_core::tools::bg_process::{BgProcessSnapshot, BgProcessStatus};
use std::collections::HashMap;
use std::time::Duration;

/// Per-task last-known activity description. Populated from
/// `BgChildActivity` events. Bounded by registry membership: rows
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
pub struct BgActivityTracker {
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

impl BgActivityTracker {
    /// Absorb a `BgChildActivity` event. Idempotent — re-recording
    /// the same activity overwrites the previous entry.
    pub fn record_activity(&mut self, task_id: u32, kind: &BgChildActivityKind) {
        let description = match kind {
            BgChildActivityKind::ToolStart { summary, .. } => summary.clone(),
            BgChildActivityKind::ToolEnd { tool_name, success } => {
                // ToolEnd would normally be a no-op (we want to keep
                // showing the most recent ToolStart, not "Read foo
                // ✓"), but if it's a *failure* we surface that so the
                // user sees something went wrong without reaching for
                // the post-completion bullets. Successes leave the
                // existing ToolStart in place.
                if *success {
                    return;
                }
                format!("{tool_name} \u{2717}")
            }
            BgChildActivityKind::Info { message } => message.clone(),
            // Forward-compat: future activity kinds are dropped from
            // the live overlay until we know how to render them. Same
            // shape as the success ToolEnd case above (#1224).
            _ => return,
        };
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
    /// widget's `BgActivityOverlay::new` signature.
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
        agents: &[BgTaskSnapshot],
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
        (all, total)
    }
}

/// Format an age into the compact 3-cell-wide string the overlay
/// expects (`"<1s"`, `" 4s"`, `"42s"`, `" 2m"`, `"15m"`, `" 1h"`).
///
/// Caller is responsible for any further padding alignment; this
/// helper just keeps the textual width to <=3 graphemes so the
/// `(AGE)` parens align across rows of mixed magnitudes.
fn format_age(d: Duration) -> String {
    let secs = d.as_secs();
    if secs == 0 {
        "<1s".to_string()
    } else if secs < 60 {
        format!("{secs:>2}s")
    } else if secs < 3600 {
        let m = secs / 60;
        format!("{m:>2}m")
    } else {
        let h = secs / 3600;
        format!("{h:>2}h")
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use koda_core::engine::event::BgChildActivityKind;

    fn agent(task_id: u32, name: &str, age_secs: u64, status: AgentStatus) -> BgTaskSnapshot {
        BgTaskSnapshot::for_testing(
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
        let mut t = BgActivityTracker::default();
        let (rows, total) = t.build_rows(&[], &[]);
        assert!(rows.is_empty());
        assert_eq!(total, 0);
    }

    #[test]
    fn running_agent_without_recorded_activity_renders_with_no_activity() {
        let mut t = BgActivityTracker::default();
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
        let mut t = BgActivityTracker::default();
        t.record_activity(
            1,
            &BgChildActivityKind::ToolStart {
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
        let mut t = BgActivityTracker::default();
        t.record_activity(
            1,
            &BgChildActivityKind::ToolStart {
                tool_name: "Read".into(),
                summary: "Read foo.rs".into(),
            },
        );
        t.record_activity(
            1,
            &BgChildActivityKind::ToolEnd {
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
        let mut t = BgActivityTracker::default();
        t.record_activity(
            1,
            &BgChildActivityKind::ToolStart {
                tool_name: "Bash".into(),
                summary: "Bash cargo test".into(),
            },
        );
        t.record_activity(
            1,
            &BgChildActivityKind::ToolEnd {
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
        let mut t = BgActivityTracker::default();
        t.record_activity(
            1,
            &BgChildActivityKind::ToolStart {
                tool_name: "Read".into(),
                summary: "Read foo.rs".into(),
            },
        );
        t.record_activity(
            1,
            &BgChildActivityKind::Info {
                message: "Cache hit, skipping".into(),
            },
        );
        let snap = vec![agent(1, "explore", 5, AgentStatus::Running { iter: 2 })];
        let (rows, _) = t.build_rows(&snap, &[]);
        assert_eq!(rows[0].activity.as_deref(), Some("Cache hit, skipping"));
    }

    #[test]
    fn terminal_agents_are_excluded_from_overlay() {
        let mut t = BgActivityTracker::default();
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
        let mut t = BgActivityTracker::default();
        t.mark_cancelling(7);
        let snap = vec![agent(7, "explore", 5, AgentStatus::Running { iter: 2 })];
        let (rows, _) = t.build_rows(&snap, &[]);
        assert_eq!(rows[0].status, ActivityStatus::Cancelling);
    }

    #[test]
    fn stale_tracker_entries_pruned_when_registry_drops_task() {
        let mut t = BgActivityTracker::default();
        t.record_activity(
            42,
            &BgChildActivityKind::ToolStart {
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
        let mut t = BgActivityTracker::default();
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
        let mut t = BgActivityTracker::default();
        let agents: Vec<_> = (0..(MAX_VISIBLE as u32 + 4))
            .map(|i| agent(i, &format!("ag{i}"), 1, AgentStatus::Running { iter: 1 }))
            .collect();
        let (rows, total) = t.build_rows(&agents, &[]);
        assert_eq!(rows.len(), MAX_VISIBLE);
        assert_eq!(total, MAX_VISIBLE + 4);
    }

    #[test]
    fn process_row_uses_command_as_activity() {
        let mut t = BgActivityTracker::default();
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

    #[test]
    fn format_age_minutes_padded() {
        assert_eq!(format_age(Duration::from_secs(60)), " 1m");
        assert_eq!(format_age(Duration::from_secs(15 * 60)), "15m");
    }

    #[test]
    fn format_age_hours_padded() {
        assert_eq!(format_age(Duration::from_secs(3600)), " 1h");
    }
}
