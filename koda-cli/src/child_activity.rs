//! What the activity overlay shows: the turn's sub-agents and the
//! session's background processes (#1210).
//!
//! Stateful counterpart to the pure-render
//! [`crate::widgets::child_activity_overlay`] widget. Both kinds of row
//! come from what the host sends, not from the engine's registries:
//!
//! - **Sub-agents** from the `ChildTaskUpdate` and `ChildAgentActivity`
//!   events of the running turn. A row appears with the first status
//!   and goes with the last (`Completed`, `Errored`, `Cancelled`). Sub-
//!   agents run inside the turn, so the turn's end clears any left.
//! - **Background processes** from `session/backgroundTerminals/list`,
//!   which the TUI asks for about once a second while a turn runs or a
//!   process is listed. They outlive the turn; `/stop` stops them.
//!
//! The hint row says what stops what: Esc interrupts the turn and its
//! sub-agents, `/stop` the background processes. Codex's TUI draws the
//! same split.

use crate::widgets::child_activity_overlay::{ActivityRow, ActivityStatus, MAX_VISIBLE};
use koda_client::protocol::background::BackgroundTerminal;
use koda_core::child_agent::AgentStatus;
use koda_core::engine::event::ChildAgentActivityKind;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// A sub-agent of the running turn, as its events describe it.
#[derive(Debug, Clone)]
struct AgentRow {
    name: String,
    /// When its first status arrived.
    started: Instant,
    /// Its latest tool call or info line, if any yet.
    activity: Option<String>,
    /// Still queued (`Pending`).
    pending: bool,
    /// Esc asked the turn to stop; the row goes red until its last
    /// status arrives.
    cancelling: bool,
}

/// The overlay's state. Carry one on `TuiContext`.
#[derive(Debug, Clone, Default)]
pub struct ChildActivityTracker {
    /// The turn's sub-agents, by task id, so they keep spawn order.
    agents: BTreeMap<u32, AgentRow>,
    /// The background processes, as the host last listed them, oldest
    /// first.
    processes: Vec<BackgroundTerminal>,
    /// When the host last listed them.
    listed_at: Option<Instant>,
}

/// How often the TUI asks the host for its background processes.
const LIST_EVERY: Duration = Duration::from_secs(1);

impl ChildActivityTracker {
    /// Absorb a `ChildTaskUpdate`: a first status adds the row, a last
    /// one drops it.
    pub fn record_status(&mut self, task_id: u32, agent_name: &str, status: &AgentStatus) {
        let pending = match status {
            AgentStatus::Pending => true,
            AgentStatus::Running { .. } => false,
            // Terminal, or a status this build doesn't know (#1224):
            // either way the row goes rather than stick.
            _ => {
                self.agents.remove(&task_id);
                return;
            }
        };
        let row = self.agents.entry(task_id).or_insert_with(|| AgentRow {
            name: agent_name.to_string(),
            started: Instant::now(),
            activity: None,
            pending,
            cancelling: false,
        });
        row.pending = pending;
        if row.name.is_empty() {
            row.name = agent_name.to_string();
        }
    }

    /// Absorb a `ChildAgentActivity` for a sub-agent already shown.
    pub fn record_activity(&mut self, task_id: u32, kind: &ChildAgentActivityKind) {
        let description = match kind {
            ChildAgentActivityKind::ToolStart { summary, .. } => summary.clone(),
            // A success keeps showing the call that started; a failure
            // says so, so the user sees it without waiting for the end.
            ChildAgentActivityKind::ToolEnd { success: true, .. } => return,
            ChildAgentActivityKind::ToolEnd { tool_name, .. } => format!("{tool_name} \u{2717}"),
            ChildAgentActivityKind::Info { message } => message.clone(),
            // A kind this build can't render (#1224).
            _ => return,
        };
        if let Some(row) = self.agents.get_mut(&task_id) {
            row.activity = Some(description);
        }
    }

    /// Esc or Ctrl+C stopped the turn: its sub-agents' rows go red until
    /// their last statuses arrive.
    pub fn mark_agents_cancelling(&mut self) {
        for row in self.agents.values_mut() {
            row.cancelling = true;
        }
    }

    /// The turn ended, and its sub-agents with it.
    pub fn clear_agents(&mut self) {
        self.agents.clear();
    }

    /// The background processes the host now lists.
    pub fn set_processes(&mut self, processes: Vec<BackgroundTerminal>) {
        self.processes = processes;
        self.listed_at = Some(Instant::now());
    }

    /// Whether to ask the host for the background processes again: about
    /// once a second while a turn runs, which may start one, or while one
    /// is listed, which may exit.
    pub fn list_due(&self, turn_running: bool) -> bool {
        (turn_running || self.has_processes())
            && self.listed_at.is_none_or(|at| at.elapsed() >= LIST_EVERY)
    }

    /// Whether any background process is listed.
    pub fn has_processes(&self) -> bool {
        !self.processes.is_empty()
    }

    /// The rows to draw, at most [`MAX_VISIBLE`], and how many there are
    /// in all: sub-agents in spawn order, then processes, oldest first.
    pub fn build_rows(&self) -> (Vec<ActivityRow>, usize) {
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let agents = self.agents.values().map(|a| ActivityRow {
            icon: "\u{1f916}", // 🤖
            label: a.name.clone(),
            age: format_age(a.started.elapsed()),
            activity: a.activity.clone(),
            status: if a.cancelling {
                ActivityStatus::Cancelling
            } else if a.pending {
                ActivityStatus::Pending
            } else {
                ActivityStatus::Running
            },
        });
        let processes = self.processes.iter().map(|p| ActivityRow {
            icon: "\u{1f41a}", // 🐚
            label: format!("process:{}", p.process_id),
            age: format_age(Duration::from_secs((now_unix - p.started_at).max(0) as u64)),
            // Processes have no activity stream; the command tells two
            // `cargo build`s apart.
            activity: Some(p.command.clone()),
            status: ActivityStatus::Running,
        });
        let mut all: Vec<ActivityRow> = agents.chain(processes).collect();
        let total = all.len();
        all.truncate(MAX_VISIBLE);
        (all, total)
    }

    /// What the hint row says stops the rows shown.
    pub fn hint(&self) -> &'static str {
        match (!self.agents.is_empty(), self.has_processes()) {
            (true, true) => "Esc to interrupt \u{b7} /stop to stop background processes",
            (true, false) => "Esc to interrupt",
            _ => "/stop to stop",
        }
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

    fn process(pid: u32, command: &str, age_secs: i64) -> BackgroundTerminal {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        BackgroundTerminal {
            process_id: pid.to_string(),
            command: command.to_string(),
            cwd: "/tmp".into(),
            os_pid: pid,
            started_at: now - age_secs,
        }
    }

    fn tool_start(summary: &str) -> ChildAgentActivityKind {
        ChildAgentActivityKind::ToolStart {
            tool_name: "Read".into(),
            summary: summary.into(),
        }
    }

    #[test]
    fn nothing_running_draws_nothing() {
        let (rows, total) = ChildActivityTracker::default().build_rows();
        assert!(rows.is_empty());
        assert_eq!(total, 0);
    }

    #[test]
    fn a_sub_agent_row_comes_with_its_first_status_and_goes_with_its_last() {
        let mut t = ChildActivityTracker::default();
        t.record_status(1, "explore", &AgentStatus::Pending);
        let (rows, _) = t.build_rows();
        assert_eq!(rows[0].label, "explore");
        assert_eq!(rows[0].status, ActivityStatus::Pending);
        assert_eq!(rows[0].activity, None);

        t.record_status(1, "explore", &AgentStatus::Running { iter: 0 });
        assert_eq!(t.build_rows().0[0].status, ActivityStatus::Running);

        t.record_status(
            1,
            "explore",
            &AgentStatus::Completed {
                summary: "done".into(),
            },
        );
        assert_eq!(t.build_rows().1, 0);
    }

    #[test]
    fn an_errored_or_cancelled_sub_agent_goes_too() {
        let mut t = ChildActivityTracker::default();
        t.record_status(1, "a", &AgentStatus::Running { iter: 1 });
        t.record_status(2, "b", &AgentStatus::Running { iter: 1 });
        t.record_status(1, "a", &AgentStatus::Errored { error: "x".into() });
        t.record_status(2, "b", &AgentStatus::Cancelled);
        assert_eq!(t.build_rows().1, 0);
    }

    #[test]
    fn activity_shows_the_latest_call_and_keeps_it_through_a_success() {
        let mut t = ChildActivityTracker::default();
        t.record_status(1, "explore", &AgentStatus::Running { iter: 1 });
        t.record_activity(1, &tool_start("Read src/auth.rs"));
        t.record_activity(
            1,
            &ChildAgentActivityKind::ToolEnd {
                tool_name: "Read".into(),
                success: true,
            },
        );
        assert_eq!(
            t.build_rows().0[0].activity.as_deref(),
            Some("Read src/auth.rs")
        );
        t.record_activity(
            1,
            &ChildAgentActivityKind::ToolEnd {
                tool_name: "Bash".into(),
                success: false,
            },
        );
        assert_eq!(
            t.build_rows().0[0].activity.as_deref(),
            Some("Bash \u{2717}")
        );
        t.record_activity(
            1,
            &ChildAgentActivityKind::Info {
                message: "thinking".into(),
            },
        );
        assert_eq!(t.build_rows().0[0].activity.as_deref(), Some("thinking"));
    }

    #[test]
    fn activity_for_an_unknown_task_adds_no_row() {
        let mut t = ChildActivityTracker::default();
        t.record_activity(9, &tool_start("Read x"));
        assert_eq!(t.build_rows().1, 0);
    }

    #[test]
    fn interrupting_turns_the_sub_agents_red_and_the_turns_end_clears_them() {
        let mut t = ChildActivityTracker::default();
        t.record_status(1, "a", &AgentStatus::Running { iter: 1 });
        t.mark_agents_cancelling();
        assert_eq!(t.build_rows().0[0].status, ActivityStatus::Cancelling);
        t.clear_agents();
        assert_eq!(t.build_rows().1, 0);
    }

    #[test]
    fn sub_agents_come_in_spawn_order_before_processes() {
        let mut t = ChildActivityTracker::default();
        t.set_processes(vec![process(42, "cargo build", 8)]);
        t.record_status(2, "verify", &AgentStatus::Running { iter: 1 });
        t.record_status(1, "explore", &AgentStatus::Running { iter: 1 });
        let labels: Vec<String> = t.build_rows().0.into_iter().map(|r| r.label).collect();
        assert_eq!(labels, ["explore", "verify", "process:42"]);
    }

    #[test]
    fn a_process_row_shows_its_command_and_age() {
        let mut t = ChildActivityTracker::default();
        t.set_processes(vec![process(42, "npm run dev", 12)]);
        let (rows, _) = t.build_rows();
        assert_eq!(rows[0].activity.as_deref(), Some("npm run dev"));
        assert!(
            rows[0].age == "12s" || rows[0].age == "13s",
            "{}",
            rows[0].age
        );
    }

    #[test]
    fn rows_past_the_cap_count_toward_the_total() {
        let mut t = ChildActivityTracker::default();
        t.set_processes((1..=7).map(|pid| process(pid, "sleep 30", 1)).collect());
        let (rows, total) = t.build_rows();
        assert_eq!(rows.len(), MAX_VISIBLE);
        assert_eq!(total, 7);
    }

    #[test]
    fn processes_are_listed_again_each_second_while_a_turn_runs_or_one_is_listed() {
        let mut t = ChildActivityTracker::default();
        assert!(!t.list_due(false));
        assert!(t.list_due(true));
        t.set_processes(vec![process(42, "sleep 30", 1)]);
        assert!(!t.list_due(false) && !t.list_due(true));
        t.listed_at = Some(Instant::now() - LIST_EVERY);
        assert!(t.list_due(false));
        t.set_processes(Vec::new());
        t.listed_at = Some(Instant::now() - LIST_EVERY);
        assert!(!t.list_due(false));
    }

    #[test]
    fn the_hint_names_what_stops_the_rows_shown() {
        let mut t = ChildActivityTracker::default();
        t.set_processes(vec![process(42, "sleep 30", 1)]);
        assert_eq!(t.hint(), "/stop to stop");
        t.record_status(1, "a", &AgentStatus::Running { iter: 1 });
        assert!(t.hint().starts_with("Esc to interrupt \u{b7} /stop"));
        t.set_processes(Vec::new());
        assert_eq!(t.hint(), "Esc to interrupt");
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
}
