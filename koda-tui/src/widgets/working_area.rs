//! The working area: what Koda is doing now, drawn above the prompt while a
//! turn runs, with the turn's plan under it.
//!
//! The status line names the step and how long it has taken (`Thinking… 12s`,
//! `Reading Cargo.toml… 2s`, `Running a command… 1m 2s`), as Claude Code's
//! does. The plan is the one `turn/plan/updated` sends (Codex's
//! notification), drawn in place as Claude Code draws its task list in the
//! status area: at most [`MAX_PLAN_STEPS`] steps, shown while any is
//! unfinished, hidden and shown again with Ctrl+T.
//!
//! The renderer keeps this state from the turn's items; the viewport asks for
//! [`WorkingArea::lines`] on each frame, so the times tick.

use crate::theme::{BOLD, DIM, STATUS_INFO, STATUS_SUCCESS};
use crate::tool_summary::{ToolCallKind, ToolCallSummary};
use koda_client::protocol::turn::{PlanStep, PlanStepStatus};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use std::time::{Duration, Instant};

/// Plan steps shown at once, as Claude Code's task list shows five.
pub(crate) const MAX_PLAN_STEPS: usize = 5;

/// What Koda is doing between tool calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// Waiting on the model.
    Working,
    /// The model is reasoning.
    Thinking,
    /// The model is writing text.
    Writing,
}

impl Step {
    fn label(self) -> &'static str {
        match self {
            Self::Working => "Working",
            Self::Thinking => "Thinking",
            Self::Writing => "Writing",
        }
    }
}

/// A tool call running now.
struct Running {
    id: String,
    label: String,
    since: Instant,
}

/// The working area's state for one turn.
pub(crate) struct WorkingArea {
    /// What the model is doing when no call runs, and since when.
    step: (Step, Instant),
    /// The session's own calls running, oldest first. The status line names
    /// the latest.
    running: Vec<Running>,
    /// The turn's plan, as the host last sent it.
    plan: Vec<PlanStep>,
}

impl Default for WorkingArea {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkingArea {
    pub(crate) fn new() -> Self {
        Self {
            step: (Step::Working, Instant::now()),
            running: Vec::new(),
            plan: Vec::new(),
        }
    }

    /// The model moved on to `step`. Its clock restarts only on a change.
    pub(crate) fn set(&mut self, step: Step) {
        if self.step.0 != step {
            self.step = (step, Instant::now());
        }
    }

    /// One of the session's calls started.
    pub(crate) fn call_started(&mut self, id: &str, tool: &str, arguments: &serde_json::Value) {
        self.running.push(Running {
            id: id.to_string(),
            label: call_label(tool, arguments),
            since: Instant::now(),
        });
    }

    /// One of the session's calls ended. When none runs, Koda waits on the
    /// model again, from now.
    pub(crate) fn call_ended(&mut self, id: &str) {
        let before = self.running.len();
        self.running.retain(|r| r.id != id);
        if before != self.running.len() && self.running.is_empty() {
            self.step = (Step::Working, Instant::now());
        }
    }

    /// The host sent the plan again.
    pub(crate) fn set_plan(&mut self, plan: Vec<PlanStep>) {
        self.plan = plan;
    }

    /// The plan has a step left to do: the panel has something to show.
    pub(crate) fn has_open_plan(&self) -> bool {
        self.plan
            .iter()
            .any(|s| s.status != PlanStepStatus::Completed)
    }

    /// What the status line says now, without its time.
    pub(crate) fn label(&self) -> &str {
        match self.running.last() {
            Some(running) => &running.label,
            None => self.step.0.label(),
        }
    }

    fn since(&self) -> Instant {
        self.running.last().map_or(self.step.1, |r| r.since)
    }

    /// The working area's lines at `now`: the status line, then the plan
    /// when it is shown and has a step left. `waiting` names what the user
    /// is asked for, which stops Koda until they answer: it takes the status
    /// line's place, without a time.
    pub(crate) fn lines(
        &self,
        waiting: Option<&str>,
        plan_shown: bool,
        now: Instant,
    ) -> Vec<Line<'static>> {
        let elapsed = now.saturating_duration_since(self.since());
        let mut status = vec![Span::styled(
            format!("{} ", spinner(elapsed)),
            STATUS_INFO.add_modifier(Modifier::BOLD),
        )];
        match waiting {
            Some(waiting) => status.push(Span::styled(waiting.to_string(), BOLD)),
            None => {
                status.push(Span::styled(format!("{}\u{2026}", self.label()), BOLD));
                if elapsed >= Duration::from_secs(1) {
                    let ms = elapsed.as_millis() as u64;
                    status.push(Span::styled(
                        format!(" {}", crate::tui_render::worked_for(ms)),
                        DIM,
                    ));
                }
            }
        }
        let open_plan = self.has_open_plan();
        if open_plan {
            let hint = if plan_shown {
                "ctrl+t to hide plan"
            } else {
                "ctrl+t to show plan"
            };
            status.push(Span::styled(format!("  ({hint})"), DIM));
        }
        let mut lines = vec![Line::from(status)];
        if open_plan && plan_shown {
            lines.extend(plan_lines(&self.plan));
        }
        lines
    }
}

/// A turning mark, a quarter each second, so a long step still looks alive.
fn spinner(elapsed: Duration) -> char {
    const FRAMES: [char; 4] = ['\u{25d0}', '\u{25d3}', '\u{25d1}', '\u{25d2}'];
    FRAMES[(elapsed.as_secs() % 4) as usize]
}

/// The plan's rows: each step with its mark, the one in progress bold.
/// More than [`MAX_PLAN_STEPS`]: the window starts a step before the first
/// unfinished one, and a last row counts the rest.
fn plan_lines(plan: &[PlanStep]) -> Vec<Line<'static>> {
    let first_open = plan
        .iter()
        .position(|s| s.status != PlanStepStatus::Completed)
        .unwrap_or(0);
    let start = first_open
        .saturating_sub(1)
        .min(plan.len().saturating_sub(MAX_PLAN_STEPS));
    let shown = &plan[start..plan.len().min(start + MAX_PLAN_STEPS)];
    let mut lines: Vec<Line<'static>> = shown
        .iter()
        .enumerate()
        .map(|(i, step)| {
            let lead = if i == 0 { "  \u{23bf}  " } else { "     " };
            let (mark, mark_style, text_style) = match step.status {
                PlanStepStatus::Completed => (
                    "\u{2714}",
                    STATUS_SUCCESS,
                    DIM.add_modifier(Modifier::CROSSED_OUT),
                ),
                PlanStepStatus::InProgress => ("\u{25fc}", BOLD, BOLD),
                PlanStepStatus::Pending => ("\u{25fb}", DIM, ratatui::style::Style::default()),
            };
            Line::from(vec![
                Span::styled(lead, DIM),
                Span::styled(format!("{mark} "), mark_style),
                Span::styled(step.step.clone(), text_style),
            ])
        })
        .collect();
    let hidden = plan.len() - shown.len();
    if hidden > 0 {
        lines.push(Line::from(Span::styled(
            format!("     \u{2026} +{hidden} more"),
            DIM,
        )));
    }
    lines
}

/// What the status line says while a call runs, in the words Claude's apps
/// use: `Reading Cargo.toml`, `Searching for "GUI"`, `Running a command`.
pub(crate) fn call_label(tool: &str, arguments: &serde_json::Value) -> String {
    let summary = ToolCallSummary::from_call(tool, arguments);
    let label = match (tool, &summary.kind) {
        ("Read", ToolCallKind::Path { path }) => format!("Reading {}", file_name(path)),
        ("Write", ToolCallKind::Path { path }) => format!("Writing {}", file_name(path)),
        ("Edit", ToolCallKind::Path { path }) => format!("Editing {}", file_name(path)),
        ("Grep", ToolCallKind::Grep { pattern, .. }) => format!("Searching for \"{pattern}\""),
        ("Glob", ToolCallKind::Glob { pattern, .. }) => format!("Finding {pattern}"),
        ("List", ToolCallKind::List { dir }) => format!("Listing {dir}"),
        ("WebFetch", ToolCallKind::WebFetch { url }) => format!("Fetching {}", host(url)),
        ("Bash" | "Cargo", _) => "Running a command".to_string(),
        ("WebSearch", _) => "Searching the web".to_string(),
        ("TodoWrite", _) => "Updating the plan".to_string(),
        ("AskUser", _) => "Waiting for your answer".to_string(),
        ("InvokeAgent", _) => format!(
            "Waiting on {}",
            arguments["agent_name"].as_str().unwrap_or("an agent")
        ),
        _ => format!("Running {tool}"),
    };
    truncate_label(label)
}

/// A path's last part: the status line has no room for the rest.
fn file_name(path: &str) -> &str {
    path.rsplit('/').find(|p| !p.is_empty()).unwrap_or(path)
}

/// A URL's host.
fn host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.split('/').next().unwrap_or(rest)
}

/// Labels longer than this are cut with `…`.
const MAX_LABEL_CHARS: usize = 60;

fn truncate_label(label: String) -> String {
    if label.chars().count() <= MAX_LABEL_CHARS {
        return label;
    }
    let mut cut: String = label.chars().take(MAX_LABEL_CHARS - 1).collect();
    cut.push('\u{2026}');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn step(step: &str, status: PlanStepStatus) -> PlanStep {
        PlanStep {
            step: step.into(),
            status,
        }
    }

    #[test]
    fn a_call_is_named_as_claudes_apps_name_it() {
        let label = |tool: &str, args| call_label(tool, &args);
        assert_eq!(
            label("Read", json!({"file_path": "/repo/koda/Cargo.toml"})),
            "Reading Cargo.toml"
        );
        assert_eq!(
            label("Grep", json!({"pattern": "GUI"})),
            "Searching for \"GUI\""
        );
        assert_eq!(
            label("Bash", json!({"command": "rg GUI"})),
            "Running a command"
        );
        assert_eq!(
            label("Edit", json!({"file_path": "src/item.rs"})),
            "Editing item.rs"
        );
        assert_eq!(
            label("WebFetch", json!({"url": "https://docs.rs/ratatui/latest"})),
            "Fetching docs.rs"
        );
        assert_eq!(
            label("InvokeAgent", json!({"agent_name": "explore"})),
            "Waiting on explore"
        );
        let long = label("Grep", json!({"pattern": "x".repeat(200)}));
        assert_eq!(long.chars().count(), MAX_LABEL_CHARS);
        assert!(long.ends_with('\u{2026}'));
    }

    /// The status line names the latest running call, then what the model
    /// does once none runs.
    #[test]
    fn the_status_line_follows_the_turn() {
        let mut area = WorkingArea::new();
        assert_eq!(area.label(), "Working");
        area.set(Step::Thinking);
        assert_eq!(area.label(), "Thinking");
        area.call_started("r1", "Read", &json!({"file_path": "a.rs"}));
        area.call_started("b1", "Bash", &json!({"command": "ls"}));
        assert_eq!(area.label(), "Running a command");
        area.call_ended("b1");
        assert_eq!(area.label(), "Reading a.rs");
        area.call_ended("r1");
        assert_eq!(area.label(), "Working");

        let now = Instant::now() + Duration::from_secs(62);
        let line = text(&area.lines(None, true, now)[0]);
        assert!(line.ends_with("Working\u{2026} 1m 2s"), "{line}");
        // A question for the user takes the line's place.
        let line = text(&area.lines(Some("Waiting for your approval"), true, now)[0]);
        assert!(line.ends_with("Waiting for your approval"), "{line}");
    }

    /// The plan shows under the status line while a step is left, and Ctrl+T
    /// hides it.
    #[test]
    fn the_plan_shows_while_a_step_is_left() {
        let mut area = WorkingArea::new();
        let now = Instant::now();
        area.set_plan(vec![
            step("Read the note", PlanStepStatus::Completed),
            step("Write the code", PlanStepStatus::InProgress),
            step("Run the tests", PlanStepStatus::Pending),
        ]);
        let lines: Vec<String> = area.lines(None, true, now).iter().map(text).collect();
        assert_eq!(lines.len(), 4, "{lines:?}");
        assert!(lines[0].ends_with("(ctrl+t to hide plan)"), "{lines:?}");
        assert_eq!(lines[1], "  \u{23bf}  \u{2714} Read the note");
        assert_eq!(lines[2], "     \u{25fc} Write the code");
        assert_eq!(lines[3], "     \u{25fb} Run the tests");

        let hidden: Vec<String> = area.lines(None, false, now).iter().map(text).collect();
        assert_eq!(hidden.len(), 1);
        assert!(hidden[0].ends_with("(ctrl+t to show plan)"), "{hidden:?}");

        // Every step done: nothing left to show.
        area.set_plan(vec![step("Read the note", PlanStepStatus::Completed)]);
        let done: Vec<String> = area.lines(None, true, now).iter().map(text).collect();
        assert_eq!(done.len(), 1);
        assert!(!done[0].contains("ctrl+t"), "{done:?}");
    }

    /// A long plan shows five steps from just before the first unfinished
    /// one, and counts the rest.
    #[test]
    fn a_long_plan_shows_five_steps_around_the_current_one() {
        let plan: Vec<PlanStep> = (1..=9)
            .map(|i| {
                let status = match i {
                    1..=4 => PlanStepStatus::Completed,
                    5 => PlanStepStatus::InProgress,
                    _ => PlanStepStatus::Pending,
                };
                step(&format!("step {i}"), status)
            })
            .collect();
        let lines: Vec<String> = plan_lines(&plan).iter().map(text).collect();
        assert_eq!(lines.len(), 6, "{lines:?}");
        assert!(lines[0].ends_with("step 4"), "{lines:?}");
        assert!(lines[1].ends_with("step 5"), "{lines:?}");
        assert!(lines[4].ends_with("step 8"), "{lines:?}");
        assert_eq!(lines[5], "     \u{2026} +4 more");
    }
}
