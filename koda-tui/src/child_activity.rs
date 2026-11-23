//! What the activity overlay shows: the turn's sub-agents and the
//! session's background processes (#1210).
//!
//! Stateful counterpart to the pure-render
//! [`crate::widgets::child_activity_overlay`] widget. Both kinds of row
//! come from what the host sends, not from the engine's registries:
//!
//! - **Sub-agents** from the turn's items: an `InvokeAgent` call starting
//!   adds a row, the calls and notices under it say what it does now, and
//!   its completion removes the row. Sub-agents run inside the turn, so the
//!   turn's end clears any left. Claude Code's panel below the prompt
//!   works the same way.
//! - **Background processes** from `session/backgroundTerminals/list`,
//!   which the TUI asks for about once a second while a turn runs or a
//!   process is listed. They outlive the turn; `/stop` stops them.
//!
//! The hint row says what stops what: Esc interrupts the turn and its
//! sub-agents, `/stop` the background processes. Codex's TUI draws the
//! same split.

use crate::widgets::child_activity_overlay::{ActivityRow, ActivityStatus, MAX_VISIBLE};
use koda_client::protocol::background::{BackgroundAgent, BackgroundTerminal};
use std::time::{Duration, Instant};

/// A sub-agent of the running turn: its `InvokeAgent` call.
#[derive(Debug, Clone)]
struct AgentRow {
    /// The call's item id.
    call: String,
    name: String,
    /// When its call started.
    started: Instant,
    /// Its latest tool call or notice, if any yet.
    activity: Option<String>,
    /// Esc asked the turn to stop; the row goes red until its call ends.
    cancelling: bool,
}

/// The overlay's state. Carry one on `TuiContext`.
#[derive(Debug, Clone, Default)]
pub struct ChildActivityTracker {
    /// The turn's sub-agents, in the order they started.
    agents: Vec<AgentRow>,
    /// The background processes, as the host last listed them, oldest
    /// first.
    processes: Vec<BackgroundTerminal>,
    /// The model's background agents, as the host last listed them, oldest
    /// first.
    background_agents: Vec<BackgroundAgent>,
    /// When the host last listed them.
    listed_at: Option<Instant>,
}

/// How often the TUI asks the host for its background processes.
const LIST_EVERY: Duration = Duration::from_secs(1);

impl ChildActivityTracker {
    /// An `InvokeAgent` call started: its sub-agent gets a row.
    pub fn agent_started(&mut self, call: &str, name: &str) {
        if self.agents.iter().any(|a| a.call == call) {
            return;
        }
        self.agents.push(AgentRow {
            call: call.to_string(),
            name: name.to_string(),
            started: Instant::now(),
            activity: None,
            cancelling: false,
        });
    }

    /// What the sub-agent of `call` does now: a tool call it started, a
    /// notice, or a call of its that failed.
    pub fn agent_activity(&mut self, call: &str, activity: String) {
        if let Some(row) = self.agents.iter_mut().find(|a| a.call == call) {
            row.activity = Some(activity);
        }
    }

    /// The `InvokeAgent` call ended: its row goes.
    pub fn agent_ended(&mut self, call: &str) {
        self.agents.retain(|a| a.call != call);
    }

    /// Esc or Ctrl+C stopped the turn: its sub-agents' rows go red until
    /// their last statuses arrive.
    pub fn mark_agents_cancelling(&mut self) {
        for row in self.agents.iter_mut() {
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

    /// The background processes and background agents the host now lists.
    pub fn set_background(
        &mut self,
        processes: Vec<BackgroundTerminal>,
        agents: Vec<BackgroundAgent>,
    ) {
        self.background_agents = agents;
        self.set_processes(processes);
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
        !self.processes.is_empty() || !self.background_agents.is_empty()
    }

    /// The rows to draw, at most [`MAX_VISIBLE`], and how many there are
    /// in all: sub-agents in spawn order, then processes, oldest first.
    pub fn build_rows(&self) -> (Vec<ActivityRow>, usize) {
        // `startedAt` is in Unix milliseconds.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        let agents = self.agents.iter().map(|a| ActivityRow {
            icon: "\u{1f916}", // 🤖
            label: a.name.clone(),
            age: format_age(a.started.elapsed()),
            activity: a.activity.clone(),
            status: if a.cancelling {
                ActivityStatus::Cancelling
            } else {
                ActivityStatus::Running
            },
        });
        let processes = self.processes.iter().map(|p| ActivityRow {
            icon: "\u{1f41a}", // 🐚
            label: format!("process:{}", p.process_id),
            age: format_age(Duration::from_millis(
                now_ms.saturating_sub(p.started_at).max(0) as u64,
            )),
            // Processes have no activity stream; the command tells two
            // `cargo build`s apart.
            activity: Some(p.command.clone()),
            status: ActivityStatus::Running,
        });
        let background_agents = self.background_agents.iter().map(|a| ActivityRow {
            icon: "\u{1f916}", // 🤖
            label: format!("{} {}", a.agent_name, a.agent_id),
            age: format_age(Duration::from_millis(
                now_ms.saturating_sub(a.started_at).max(0) as u64,
            )),
            // Its task tells two `explore`s apart.
            activity: Some(a.prompt.lines().next().unwrap_or_default().to_string()),
            status: ActivityStatus::Running,
        });
        let mut all: Vec<ActivityRow> = agents.chain(background_agents).chain(processes).collect();
        let total = all.len();
        all.truncate(MAX_VISIBLE);
        (all, total)
    }

    /// What the hint row says stops the rows shown.
    pub fn hint(&self) -> &'static str {
        match (!self.agents.is_empty(), self.has_processes()) {
            (true, true) => "Esc to interrupt \u{b7} /stop to stop background work",
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

    /// A process started `age_secs` ago, with `startedAt` in Unix
    /// milliseconds, as the host sends it.
    fn process(pid: u32, command: &str, age_secs: i64) -> BackgroundTerminal {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        BackgroundTerminal {
            process_id: pid.to_string(),
            command: command.to_string(),
            cwd: "/tmp".into(),
            os_pid: pid,
            started_at: now_ms - age_secs * 1000,
        }
    }

    #[test]
    fn nothing_running_draws_nothing() {
        let (rows, total) = ChildActivityTracker::default().build_rows();
        assert!(rows.is_empty());
        assert_eq!(total, 0);
    }

    #[test]
    fn a_sub_agent_row_comes_with_its_call_and_goes_with_it() {
        let mut t = ChildActivityTracker::default();
        t.agent_started("call_1", "explore");
        let (rows, _) = t.build_rows();
        assert_eq!(rows[0].label, "explore");
        assert_eq!(rows[0].status, ActivityStatus::Running);
        assert_eq!(rows[0].activity, None);
        t.agent_ended("call_1");
        assert_eq!(t.build_rows().1, 0);
    }

    #[test]
    fn activity_shows_the_latest_call_or_notice() {
        let mut t = ChildActivityTracker::default();
        t.agent_started("call_1", "explore");
        t.agent_activity("call_1", "Read src/auth.rs".into());
        assert_eq!(
            t.build_rows().0[0].activity.as_deref(),
            Some("Read src/auth.rs")
        );
        t.agent_activity("call_1", "Bash \u{2717}".into());
        assert_eq!(
            t.build_rows().0[0].activity.as_deref(),
            Some("Bash \u{2717}")
        );
    }

    #[test]
    fn activity_for_an_unknown_call_adds_no_row() {
        let mut t = ChildActivityTracker::default();
        t.agent_activity("call_9", "Read x".into());
        assert_eq!(t.build_rows().1, 0);
    }

    #[test]
    fn interrupting_turns_the_sub_agents_red_and_the_turns_end_clears_them() {
        let mut t = ChildActivityTracker::default();
        t.agent_started("call_1", "a");
        t.mark_agents_cancelling();
        assert_eq!(t.build_rows().0[0].status, ActivityStatus::Cancelling);
        t.clear_agents();
        assert_eq!(t.build_rows().1, 0);
    }

    #[test]
    fn background_agents_are_listed_after_the_turns_sub_agents() {
        let mut t = ChildActivityTracker::default();
        t.agent_started("call-1", "explore");
        t.set_background(
            vec![process(42, "cargo build", 8)],
            vec![BackgroundAgent {
                agent_id: "a1b2".into(),
                agent_name: "task".into(),
                prompt: "write the tests\nand run them".into(),
                started_at: 0,
            }],
        );
        let (rows, total) = t.build_rows();
        assert_eq!(total, 3);
        assert_eq!(rows[1].label, "task a1b2");
        assert_eq!(rows[1].activity.as_deref(), Some("write the tests"));
        assert!(rows[2].label.starts_with("process:"));
        t.clear_agents();
        assert!(
            t.has_processes(),
            "a background agent keeps the list polling"
        );
        t.set_background(Vec::new(), Vec::new());
        assert!(!t.has_processes());
    }

    #[test]
    fn sub_agents_come_in_spawn_order_before_processes() {
        let mut t = ChildActivityTracker::default();
        t.set_processes(vec![process(42, "cargo build", 8)]);
        t.agent_started("call_1", "explore");
        t.agent_started("call_2", "verify");
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
        t.agent_started("call_1", "a");
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
