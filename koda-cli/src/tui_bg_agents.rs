//! `/agents` and `/cancel <id>` slash commands — runtime view of
//! background sub-agents.
//!
//! ## Overview
//!
//! `/agents` lists currently-running background sub-agents tracked
//! in [`koda_core::bg_agent::BgAgentRegistry`]. `/cancel <id>` fires
//! the per-task cancel token from PR #1041. Together they close the
//! original #996 P0: "the model launched a bg agent and I have no
//! way to see what it's doing or stop it."
//!
//! Foreground sub-agents (the synchronous `/agent <name>` switch in
//! [`crate::tui_wizards::handle_list_agents`]) don't appear here —
//! they block the conversation and are visible inline. This file is
//! exclusively about the *background* (`InvokeAgent { background: true }`)
//! flow.
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
//!
//! ## Out of scope (deferred)
//!
//! - Background **shells** (`Bash { background: true }`) — needs a
//!   `BgRegistry::kill(pid)` API first; deferred to a follow-up PR.
//! - The "completed lingers 30s" UX polish — deferred to a polish
//!   PR. Drained results still inject into the conversation, so the
//!   user isn't missing info, just visual confirmation.
//! - LLM tools (`ListBackgroundTasks`, `CancelAgent`, `AgentStatus`)
//!   — Layer 2 / PR #1043.
//! - Status-bar pill — Layer 3 / PR #1044.
//! - Per-iter `iter` updates — Layer 4 / PR #1045.

use crate::scroll_buffer::ScrollBuffer;
use crate::tui_output;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use tui_output::{BOLD, CYAN, DIM};

/// Render `/agents` — a compact table of currently-pending
/// background sub-agents. Reads [`koda_core::bg_agent::BgAgentRegistry::snapshot`]
/// (sync, no async, sorted by ascending `task_id`). Empty registry
/// renders an explicit "No background sub-agents." line so the user
/// knows the command worked rather than wondering if `/agents`
/// silently failed.
pub(crate) fn handle_list_background_tasks(
    buffer: &mut ScrollBuffer,
    bg_agents: &koda_core::bg_agent::BgAgentRegistry,
) {
    let snapshots = bg_agents.snapshot();

    tui_output::blank(buffer);
    tui_output::emit_line(
        buffer,
        Line::styled("  \u{1f43e} Background sub-agents", BOLD),
    );
    tui_output::blank(buffer);

    if snapshots.is_empty() {
        tui_output::dim_msg(buffer, "No background sub-agents.".into());
        tui_output::blank(buffer);
        tui_output::dim_msg(
            buffer,
            "Ask Koda to launch one with `background: true` in InvokeAgent.".into(),
        );
        return;
    }

    // Column widths: pad the AGENT column to the longest name (with
    // a sane minimum of 8). ID gets 4 cols (10k tasks per session
    // is plenty); AGE gets 6 cols ("999d" is 4, allow slack).
    let name_col = snapshots
        .iter()
        .map(|s| s.agent_name.len())
        .max()
        .unwrap_or(8)
        .max(8);

    // Header.
    tui_output::emit_line(
        buffer,
        Line::from(vec![Span::styled(
            format!(
                "  {:<4}  {:<name_col$}  {:<6}  STATUS",
                "ID", "AGENT", "AGE"
            ),
            DIM,
        )]),
    );

    for snap in &snapshots {
        let mut spans = vec![Span::raw(format!(
            "  {:<4}  {:<name_col$}  {:<6}  ",
            snap.task_id,
            snap.agent_name,
            format_age(snap.age),
        ))];
        spans.extend(status_spans(&snap.status));
        tui_output::emit_line(buffer, Line::from(spans));
    }

    tui_output::blank(buffer);
    tui_output::dim_msg(
        buffer,
        "Use `/cancel <id>` to stop one. Results inject automatically when complete.".into(),
    );
}

/// Render `/cancel <id>`. Looks up the task, fires its cancel
/// token, reports success/failure. Idempotent at the registry layer
/// (PR #1041's `cancel_is_idempotent_while_pending`), so re-issuing
/// on a still-running cancelled task is harmless.
///
/// `task_id == None` means the user typed `/cancel` with no arg or
/// a non-numeric arg — we report the usage error here rather than
/// at the parser layer (see `ReplAction::CancelBackgroundTask`'s
/// docstring for the rationale).
pub(crate) fn handle_cancel_background_task(
    buffer: &mut ScrollBuffer,
    bg_agents: &koda_core::bg_agent::BgAgentRegistry,
    task_id: Option<u32>,
) {
    let Some(id) = task_id else {
        tui_output::warn_msg(
            buffer,
            "Usage: /cancel <id>  (run /agents to see ids)".into(),
        );
        return;
    };

    if bg_agents.cancel(id) {
        tui_output::ok_msg(
            buffer,
            format!("Cancellation requested for task {id}. Result will inject shortly."),
        );
    } else {
        tui_output::warn_msg(
            buffer,
            format!("No background sub-agent with id {id}. Run /agents to see active tasks."),
        );
    }
}

/// Format a `Duration` as a compact age string for the `/agents` table.
///
/// - `< 60s`  → `"Ns"` (e.g. `"5s"`)
/// - `< 60m`  → `"Nm"` (e.g. `"7m"`)
/// - `< 24h`  → `"Nh"` (e.g. `"3h"`)
/// - `>= 24h` → `"Nd"` (e.g. `"2d"`)
///
/// Bias: round **down**. A task running 119 s reads `"1m"`, not
/// `"2m"`. Matches user intent ("how many full Xs has it been
/// running?") better than rounding to nearest, and avoids the
/// confusing `"60s"` -> `"1m"` jitter at the boundary.
fn format_age(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3_600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// Color-coded status icon + label, matching Codex's
/// `multi_agents::status_summary_spans` palette: cyan running, green
/// completed, red errored, dim cancelled / pending.
fn status_spans(status: &koda_core::bg_agent::AgentStatus) -> Vec<Span<'static>> {
    use koda_core::bg_agent::AgentStatus;
    match status {
        AgentStatus::Pending => vec![Span::styled("\u{25d0} Pending", CYAN)],
        AgentStatus::Running { iter } => {
            let label = if *iter == 0 {
                // PR #1041's `Running { iter: 0 }` is a Layer-0
                // placeholder for "started but no per-iter info
                // wired yet." Layer 4 will populate it. Don't
                // render "iter 0/20" — it misleads the user into
                // thinking nothing has happened.
                "\u{25b6} Running".to_string()
            } else {
                format!("\u{25b6} Running (iter {iter}/20)")
            };
            vec![Span::styled(label, CYAN.add_modifier(Modifier::BOLD))]
        }
        AgentStatus::Cancelled => vec![Span::styled("\u{2297} Cancelled", DIM)],
        AgentStatus::Completed { summary } => {
            let mut spans = vec![Span::styled("\u{2713} Completed", tui_output::GREEN)];
            let preview = summary_preview(summary);
            if !preview.is_empty() {
                spans.push(Span::styled(format!(" \u{2014} {preview}"), DIM));
            }
            spans
        }
        AgentStatus::Errored { error } => {
            let mut spans = vec![Span::styled("\u{2717} Errored", tui_output::RED)];
            let preview = summary_preview(error);
            if !preview.is_empty() {
                spans.push(Span::styled(format!(" \u{2014} {preview}"), DIM));
            }
            spans
        }
    }
}

/// Truncate a status summary/error preview to fit on one row of the
/// `/agents` table. Codex uses 80 graphemes
/// (`COLLAB_AGENT_RESPONSE_PREVIEW_GRAPHEMES`); we use 60 to keep
/// rows readable on a 100-col terminal after the ID/AGENT/AGE prefix.
const PREVIEW_CHARS: usize = 60;

fn summary_preview(s: &str) -> String {
    // Collapse all whitespace runs (including newlines and tabs) to
    // a single space — mirrors Codex's `split_whitespace().join(" ")`.
    // Without this, embedded newlines wreck the table layout.
    let collapsed: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= PREVIEW_CHARS {
        collapsed
    } else {
        let truncated: String = collapsed.chars().take(PREVIEW_CHARS).collect();
        format!("{truncated}\u{2026}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use koda_core::bg_agent::{AgentStatus, BgAgentRegistry, BgPayload};
    use std::time::Duration;
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
            noop,
        );
        (task_id, tx, status_tx, observer)
    }

    // ── format_age ─────────────────────────────────────────────────────────────────

    /// Sub-minute durations render in seconds. The 0 boundary is a
    /// real case for very-recently-started tasks.
    #[test]
    fn format_age_seconds_under_one_minute() {
        assert_eq!(format_age(Duration::from_secs(0)), "0s");
        assert_eq!(format_age(Duration::from_secs(1)), "1s");
        assert_eq!(format_age(Duration::from_secs(59)), "59s");
    }

    /// Sub-hour durations render in minutes, **rounded down** (a
    /// 119 s task reads `"1m"`, not `"2m"`). Pinning the round-down
    /// behavior protects against an accidental switch to nearest
    /// rounding, which would jitter at the boundary.
    #[test]
    fn format_age_minutes_round_down() {
        assert_eq!(format_age(Duration::from_secs(60)), "1m");
        assert_eq!(format_age(Duration::from_secs(119)), "1m");
        assert_eq!(format_age(Duration::from_secs(3_599)), "59m");
    }

    /// Sub-day durations render in hours, also rounded down.
    #[test]
    fn format_age_hours_round_down() {
        assert_eq!(format_age(Duration::from_secs(3_600)), "1h");
        assert_eq!(format_age(Duration::from_secs(86_399)), "23h");
    }

    /// 24 h+ durations render in days. A truly long-running bg
    /// agent is unusual but we shouldn't render `"24h"` indefinitely
    /// once it crosses the day boundary.
    #[test]
    fn format_age_days_round_down() {
        assert_eq!(format_age(Duration::from_secs(86_400)), "1d");
        assert_eq!(format_age(Duration::from_secs(172_799)), "1d");
        assert_eq!(format_age(Duration::from_secs(259_200)), "3d");
    }

    // ── summary_preview ────────────────────────────────────────────────────────────

    /// Newlines and tabs in a `Completed.summary` would break our
    /// single-row table layout. Mirror Codex's whitespace-collapse.
    #[test]
    fn summary_preview_collapses_whitespace() {
        assert_eq!(
            summary_preview("line one\nline two\tline three"),
            "line one line two line three"
        );
    }

    /// Long previews are truncated with an ellipsis. Threshold is
    /// `PREVIEW_CHARS` graphemes so multi-byte text is safe.
    #[test]
    fn summary_preview_truncates_with_ellipsis() {
        let long = "a".repeat(PREVIEW_CHARS + 10);
        let preview = summary_preview(&long);
        // chars().count() because the trailing ellipsis is
        // multi-byte and `.len()` would over-count.
        assert_eq!(preview.chars().count(), PREVIEW_CHARS + 1);
        assert!(preview.ends_with('\u{2026}'));
    }

    /// Short previews pass through unchanged — no spurious ellipsis.
    #[test]
    fn summary_preview_short_passes_through() {
        assert_eq!(summary_preview("all good"), "all good");
    }

    // ── handle_list_background_tasks ───────────────────────────────────────────────

    /// Empty registry: render the explicit "No background
    /// sub-agents." line. Without it the user might wonder if
    /// `/agents` is broken.
    #[tokio::test]
    async fn list_background_tasks_empty_renders_explicit_message() {
        let mut buf = ScrollBuffer::new(64);
        let reg = BgAgentRegistry::new();
        handle_list_background_tasks(&mut buf, &reg);
        let text = buffer_text(&buf);
        assert!(
            text.contains("No background sub-agents."),
            "empty registry should render explicit empty-state line, got: {text}"
        );
    }

    /// Populated registry: every task surfaces with id, agent name,
    /// and the Pending status icon. Pinning the row content keeps
    /// the slash command's contract stable across refactors.
    #[tokio::test]
    async fn list_background_tasks_renders_each_pending_task() {
        let mut buf = ScrollBuffer::new(64);
        let reg = BgAgentRegistry::new();
        let (id_a, _tx_a, _stx_a, _cancel_a) = register_entry(&reg, "explore", "map repo");
        let (id_b, _tx_b, _stx_b, _cancel_b) = register_entry(&reg, "verify", "check tests");

        handle_list_background_tasks(&mut buf, &reg);
        let text = buffer_text(&buf);

        // Both ids and both agent names must appear.
        assert!(text.contains(&id_a.to_string()));
        assert!(text.contains(&id_b.to_string()));
        assert!(text.contains("explore"));
        assert!(text.contains("verify"));
        // And the Pending icon — layer-0 default for un-flipped tasks.
        assert!(
            text.contains("Pending"),
            "Pending status label missing, got: {text}"
        );
    }

    /// Status writes via the watch channel are reflected in the
    /// rendered output. This is the contract that lets `/agents`
    /// show live state — if it ever stales, the slash command
    /// becomes useless.
    #[tokio::test]
    async fn list_background_tasks_reflects_running_status() {
        let mut buf = ScrollBuffer::new(64);
        let reg = BgAgentRegistry::new();
        let (_id, _tx, status_tx, _cancel) = register_entry(&reg, "explore", "x");

        // Flip Pending → Running { iter: 7 } before the snapshot.
        status_tx.send(AgentStatus::Running { iter: 7 }).unwrap();

        handle_list_background_tasks(&mut buf, &reg);
        let text = buffer_text(&buf);
        assert!(
            text.contains("Running"),
            "expected 'Running' label, got: {text}"
        );
        // iter > 0 surfaces the per-iter detail.
        assert!(
            text.contains("iter 7/20"),
            "expected per-iter detail when iter > 0, got: {text}"
        );
    }

    /// Layer-0 placeholder semantics: `Running { iter: 0 }` means
    /// "started but no per-iter info yet" — don't render a misleading
    /// `"iter 0/20"`. Layer 4 (PR #1045) will populate the field.
    #[tokio::test]
    async fn list_background_tasks_hides_iter_zero_placeholder() {
        let mut buf = ScrollBuffer::new(64);
        let reg = BgAgentRegistry::new();
        let (_id, _tx, status_tx, _cancel) = register_entry(&reg, "explore", "x");

        status_tx.send(AgentStatus::Running { iter: 0 }).unwrap();

        handle_list_background_tasks(&mut buf, &reg);
        let text = buffer_text(&buf);
        assert!(text.contains("Running"));
        assert!(
            !text.contains("iter 0/20"),
            "iter 0 should not render the per-iter detail (it's a Layer-0 placeholder), got: {text}"
        );
    }

    // ── handle_cancel_background_task ──────────────────────────────────────────────

    /// Happy path: known id → cancel fires, success message
    /// surfaces, and the underlying token observed the cancel.
    #[tokio::test]
    async fn cancel_known_id_reports_success_and_fires_token() {
        let mut buf = ScrollBuffer::new(64);
        let reg = BgAgentRegistry::new();
        let (task_id, _tx, _status_tx, observer) = register_entry(&reg, "explore", "x");

        handle_cancel_background_task(&mut buf, &reg, Some(task_id));

        let text = buffer_text(&buf);
        assert!(
            text.contains(&task_id.to_string()),
            "success message should mention the id, got: {text}"
        );
        assert!(
            observer.is_cancelled(),
            "the cancel token should have been fired"
        );
    }

    /// Unknown id → warn, don't crash. The user should learn the
    /// correct id (or that the task already finished) without us
    /// throwing or silently no-oping.
    #[tokio::test]
    async fn cancel_unknown_id_reports_helpful_error() {
        let mut buf = ScrollBuffer::new(64);
        let reg = BgAgentRegistry::new();
        handle_cancel_background_task(&mut buf, &reg, Some(999));
        let text = buffer_text(&buf);
        assert!(
            text.contains("999") && text.contains("/agents"),
            "warn should name the missing id and point to /agents, got: {text}"
        );
    }

    /// `None` id (user typed `/cancel` with no arg or non-numeric)
    /// renders Usage — not a panic, and not a misleading "task 0
    /// not found."
    #[tokio::test]
    async fn cancel_none_id_renders_usage() {
        let mut buf = ScrollBuffer::new(64);
        let reg = BgAgentRegistry::new();
        handle_cancel_background_task(&mut buf, &reg, None);
        let text = buffer_text(&buf);
        assert!(
            text.contains("Usage:") && text.contains("/cancel"),
            "None id should render a Usage: line, got: {text}"
        );
    }
}
