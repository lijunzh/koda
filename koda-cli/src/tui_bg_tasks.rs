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
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use tui_output::{BOLD, CYAN, DIM};

/// Render `/agents` — a compact unified table of every currently-pending
/// background task. Reads
/// [`koda_core::bg_agent::BgAgentRegistry::snapshot`] and
/// [`koda_core::tools::bg_process::BgRegistry::snapshot`] (both sync,
/// no async). Empty registries render an explicit "No background tasks."
/// line so the user knows the command worked rather than wondering
/// if `/agents` silently failed.
///
/// Process snapshots are taken *after* a [`reap`] call so freshly-exited
/// processes show their final status instead of stale `Running`.
///
/// [`reap`]: koda_core::tools::bg_process::BgRegistry::reap
pub(crate) fn handle_list_background_tasks(
    buffer: &mut ScrollBuffer,
    bg_agents: &koda_core::bg_agent::BgAgentRegistry,
    bg_processes: &koda_core::tools::bg_process::BgRegistry,
) {
    // Refresh process statuses so users see the latest exit codes
    // rather than stale `Running` entries. Mirrors what the LLM-tool
    // path does in `bg_task_tools::execute_list`.
    bg_processes.reap();

    let agent_snaps = bg_agents.snapshot();
    let process_snaps = bg_processes.snapshot();

    tui_output::blank(buffer);
    tui_output::emit_line(buffer, Line::styled("  \u{1f43e} Background tasks", BOLD));
    tui_output::blank(buffer);

    if agent_snaps.is_empty() && process_snaps.is_empty() {
        tui_output::dim_msg(buffer, "No background tasks.".into());
        tui_output::blank(buffer);
        tui_output::dim_msg(
            buffer,
            "Ask Koda to launch one with `background: true` in InvokeAgent or Bash.".into(),
        );
        return;
    }

    // ── Column widths ─────────────────────────────────────────────
    //
    // ID column holds prefixed ids like `agent:9999` or `process:65535`,
    // so we size to the longest actual id rather than a fixed 4.
    // NAME column shows agent name OR process command head; pad to
    // the longest with a sane minimum of 8.
    let id_strings: Vec<String> = agent_snaps
        .iter()
        .map(|s| format!("agent:{}", s.task_id))
        .chain(process_snaps.iter().map(|s| format!("process:{}", s.pid)))
        .collect();
    let id_col = id_strings.iter().map(|s| s.len()).max().unwrap_or(8).max(8);

    let name_col = agent_snaps
        .iter()
        .map(|s| s.agent_name.len())
        .chain(process_snaps.iter().map(|s| command_head(&s.command).len()))
        .max()
        .unwrap_or(8)
        .max(8);

    // Header.
    tui_output::emit_line(
        buffer,
        Line::from(vec![Span::styled(
            format!(
                "  {:<id_col$}  {:<name_col$}  {:<6}  STATUS",
                "ID", "NAME", "AGE"
            ),
            DIM,
        )]),
    );

    // Render agent rows first (they're the more common case for now),
    // then process rows. Ordering inside each group is registry order
    // (ascending task_id / pid).
    for snap in &agent_snaps {
        let id = format!("agent:{}", snap.task_id);
        let mut spans = vec![Span::raw(format!(
            "  {:<id_col$}  {:<name_col$}  {:<6}  ",
            id,
            snap.agent_name,
            format_age(snap.age),
        ))];
        spans.extend(agent_status_spans(&snap.status));
        tui_output::emit_line(buffer, Line::from(spans));
    }

    for snap in &process_snaps {
        let id = format!("process:{}", snap.pid);
        let name = command_head(&snap.command);
        let mut spans = vec![Span::raw(format!(
            "  {:<id_col$}  {:<name_col$}  {:<6}  ",
            id,
            name,
            format_age(snap.age),
        ))];
        spans.extend(process_status_spans(&snap.status));
        tui_output::emit_line(buffer, Line::from(spans));
    }

    tui_output::blank(buffer);
    tui_output::dim_msg(
        buffer,
        "Use `/cancel agent:<id>` or `/cancel process:<id>` to stop one.".into(),
    );
}

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
    task_id: Option<TaskId>,
) {
    let Some(id) = task_id else {
        tui_output::warn_msg(
            buffer,
            "Usage: /cancel <agent:id|process:id>  (run /agents to see ids)".into(),
        );
        return;
    };

    match id {
        TaskId::Agent(n) => {
            if bg_agents.cancel(n) {
                tui_output::ok_msg(
                    buffer,
                    format!("Cancellation requested for agent:{n}. Result will inject shortly."),
                );
            } else {
                tui_output::warn_msg(
                    buffer,
                    format!(
                        "No background sub-agent with id agent:{n}. Run /agents to see active tasks."
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
                        "No background process with id process:{n}. Run /agents to see active tasks."
                    ),
                );
            }
        }
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

/// Truncate a command string to its head for the NAME column.
///
/// Bash commands can be arbitrarily long (`"cargo test --workspace
/// --features=foo,bar -- --nocapture | tee log.txt"`). For the table
/// we only want enough to identify the process. We strip everything
/// after the first 24 chars and append an ellipsis. Whitespace inside
/// the head is preserved so `cargo test` reads naturally.
const COMMAND_HEAD_CHARS: usize = 24;

fn command_head(cmd: &str) -> String {
    let trimmed = cmd.trim();
    if trimmed.chars().count() <= COMMAND_HEAD_CHARS {
        trimmed.to_string()
    } else {
        let truncated: String = trimmed.chars().take(COMMAND_HEAD_CHARS).collect();
        format!("{truncated}\u{2026}")
    }
}

/// Color-coded status icon + label for bg **agents**, matching Codex's
/// `multi_agents::status_summary_spans` palette: cyan running, green
/// completed, red errored, dim cancelled / pending.
fn agent_status_spans(status: &koda_core::bg_agent::AgentStatus) -> Vec<Span<'static>> {
    use koda_core::bg_agent::AgentStatus;
    match status {
        AgentStatus::Pending => vec![Span::styled("\u{25d0} Pending", CYAN)],
        AgentStatus::Running { iter } => {
            let label = if *iter == 0 {
                // PR #1041's `Running { iter: 0 }` is a Layer-0
                // placeholder for "started but no per-iter info
                // wired yet." Foreground sub-agents still send 0;
                // background agents emit live updates (Layer 4, #1058). Don't
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

/// Color-coded status icon + label for bg **processes**.
///
/// Reuses the agent palette for visual consistency:
/// - `Running` → `▶ Running` (cyan bold)
/// - `Killed`  → `⊗ Killed` (dim) — we sent SIGTERM but the reaper
///   hasn't observed exit yet
/// - `Exited { code: Some(0) }` → `✓ Exited (0)` (green) — clean exit
/// - `Exited { code: Some(c) }` where c≠0 → `✗ Exited (c)` (red)
/// - `Exited { code: None }` → `✗ Exited (signal)` (red) — POSIX returns
///   no code for signal-killed processes
fn process_status_spans(
    status: &koda_core::tools::bg_process::BgProcessStatus,
) -> Vec<Span<'static>> {
    use koda_core::tools::bg_process::BgProcessStatus;
    match status {
        BgProcessStatus::Running => vec![Span::styled(
            "\u{25b6} Running",
            CYAN.add_modifier(Modifier::BOLD),
        )],
        BgProcessStatus::Killed => vec![Span::styled("\u{2297} Killed", DIM)],
        BgProcessStatus::Exited { code: Some(0) } => {
            vec![Span::styled("\u{2713} Exited (0)", tui_output::GREEN)]
        }
        BgProcessStatus::Exited { code: Some(c) } => vec![Span::styled(
            format!("\u{2717} Exited ({c})"),
            tui_output::RED,
        )],
        BgProcessStatus::Exited { code: None } => {
            vec![Span::styled("\u{2717} Exited (signal)", tui_output::RED)]
        }
    }
}

/// Truncate a status summary/error preview to fit on one row of the
/// `/agents` table. Codex uses 80 graphemes
/// (`COLLAB_AGENT_RESPONSE_PREVIEW_GRAPHEMES`); we use 60 to keep
/// rows readable on a 100-col terminal after the ID/NAME/AGE prefix.
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
    use koda_core::tools::bg_process::BgRegistry;
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

    // ── command_head ───────────────────────────────────────────────────────────────

    /// Short commands pass through (after a trim) — no spurious ellipsis.
    #[test]
    fn command_head_short_passes_through() {
        assert_eq!(command_head("cargo test"), "cargo test");
        assert_eq!(command_head("  ls -la  "), "ls -la");
    }

    /// Long commands truncate to `COMMAND_HEAD_CHARS` graphemes plus
    /// an ellipsis. The exact threshold is what stops table rows from
    /// wrapping on a 100-col terminal, so we pin it.
    #[test]
    fn command_head_long_truncates_with_ellipsis() {
        let long = "cargo test --workspace --features=foo,bar -- --nocapture | tee log.txt";
        let head = command_head(long);
        assert_eq!(head.chars().count(), COMMAND_HEAD_CHARS + 1);
        assert!(head.ends_with('\u{2026}'));
        assert!(head.starts_with("cargo test"));
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

    /// Both registries empty: render the explicit "No background
    /// tasks." line. Without it the user might wonder if `/agents`
    /// is broken.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_background_tasks_empty_renders_explicit_message() {
        let mut buf = ScrollBuffer::new(64);
        let reg = BgAgentRegistry::new();
        let procs = BgRegistry::new();
        handle_list_background_tasks(&mut buf, &reg, &procs);
        let text = buffer_text(&buf);
        assert!(
            text.contains("No background tasks."),
            "empty registries should render explicit empty-state line, got: {text}"
        );
    }

    /// Populated agent registry: every task surfaces with its
    /// **prefixed** id (`agent:N`), agent name, and the Pending
    /// status icon. Pinning the row content keeps the slash command's
    /// contract stable across refactors.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_background_tasks_renders_each_pending_agent_with_prefix() {
        let mut buf = ScrollBuffer::new(64);
        let reg = BgAgentRegistry::new();
        let procs = BgRegistry::new();
        let (id_a, _tx_a, _stx_a, _cancel_a) = register_entry(&reg, "explore", "map repo");
        let (id_b, _tx_b, _stx_b, _cancel_b) = register_entry(&reg, "verify", "check tests");

        handle_list_background_tasks(&mut buf, &reg, &procs);
        let text = buffer_text(&buf);

        // Both ids must appear with the `agent:` prefix — bare numerics
        // would be ambiguous now that processes share the table.
        assert!(text.contains(&format!("agent:{id_a}")));
        assert!(text.contains(&format!("agent:{id_b}")));
        assert!(text.contains("explore"));
        assert!(text.contains("verify"));
        assert!(
            text.contains("Pending"),
            "Pending status label missing, got: {text}"
        );
    }

    /// Status writes via the watch channel are reflected in the
    /// rendered output. This is the contract that lets `/agents`
    /// show live state — if it ever stales, the slash command
    /// becomes useless.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_background_tasks_reflects_running_status() {
        let mut buf = ScrollBuffer::new(64);
        let reg = BgAgentRegistry::new();
        let procs = BgRegistry::new();
        let (_id, _tx, status_tx, _cancel) = register_entry(&reg, "explore", "x");

        // Flip Pending → Running { iter: 7 } before the snapshot.
        status_tx.send(AgentStatus::Running { iter: 7 }).unwrap();

        handle_list_background_tasks(&mut buf, &reg, &procs);
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
    /// `"iter 0/20"`. Layer 4 (#1058) populated the field for bg agents.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_background_tasks_hides_iter_zero_placeholder() {
        let mut buf = ScrollBuffer::new(64);
        let reg = BgAgentRegistry::new();
        let procs = BgRegistry::new();
        let (_id, _tx, status_tx, _cancel) = register_entry(&reg, "explore", "x");

        status_tx.send(AgentStatus::Running { iter: 0 }).unwrap();

        handle_list_background_tasks(&mut buf, &reg, &procs);
        let text = buffer_text(&buf);
        assert!(text.contains("Running"));
        assert!(
            !text.contains("iter 0/20"),
            "iter 0 should not render the per-iter detail (it's a Layer-0 placeholder), got: {text}"
        );
    }

    /// Phase F: process registry rows surface alongside agent rows,
    /// each with the `process:` prefix. We spawn a real `sleep 60`
    /// so the snapshot reads from a live entry and we can
    /// later kill it in cleanup.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_background_tasks_renders_processes_with_prefix() {
        let mut buf = ScrollBuffer::new(64);
        let reg = BgAgentRegistry::new();
        let procs = BgRegistry::new();
        let pid = spawn_sleep_in_registry(&procs);

        handle_list_background_tasks(&mut buf, &reg, &procs);
        let text = buffer_text(&buf);

        assert!(
            text.contains(&format!("process:{pid}")),
            "process row missing or unprefixed, got: {text}"
        );
        // The command head should appear — at least the `sleep` prefix.
        assert!(
            text.contains("sleep"),
            "expected command head 'sleep' in row, got: {text}"
        );
        // Status is live: should be Running when we just spawned.
        assert!(
            text.contains("Running"),
            "freshly-spawned process should report Running, got: {text}"
        );

        // Cleanup so we don't leak a child past the test.
        procs.kill(pid);
    }

    /// Phase F: agents and processes share one table. With both
    /// registries populated we should see both prefixes in the
    /// output — neither registry should crowd the other out.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_background_tasks_unifies_both_registries() {
        let mut buf = ScrollBuffer::new(64);
        let reg = BgAgentRegistry::new();
        let procs = BgRegistry::new();
        let (agent_id, _tx, _stx, _c) = register_entry(&reg, "explore", "x");
        let pid = spawn_sleep_in_registry(&procs);

        handle_list_background_tasks(&mut buf, &reg, &procs);
        let text = buffer_text(&buf);

        assert!(text.contains(&format!("agent:{agent_id}")));
        assert!(text.contains(&format!("process:{pid}")));

        procs.kill(pid);
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

        handle_cancel_background_task(&mut buf, &reg, &procs, Some(TaskId::Agent(task_id)));

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

        handle_cancel_background_task(&mut buf, &reg, &procs, Some(TaskId::Process(pid)));

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
        handle_cancel_background_task(&mut buf, &reg, &procs, Some(TaskId::Agent(999)));
        let text = buffer_text(&buf);
        assert!(
            text.contains("agent:999") && text.contains("/agents"),
            "warn should name the missing prefixed id and point to /agents, got: {text}"
        );
    }

    /// Unknown process id → warn, don't crash. Same shape as the
    /// agent equivalent so users get a consistent error story.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_unknown_process_id_reports_helpful_error() {
        let mut buf = ScrollBuffer::new(64);
        let reg = BgAgentRegistry::new();
        let procs = BgRegistry::new();
        handle_cancel_background_task(&mut buf, &reg, &procs, Some(TaskId::Process(999_999)));
        let text = buffer_text(&buf);
        assert!(
            text.contains("process:999999") && text.contains("/agents"),
            "warn should name the missing prefixed id and point to /agents, got: {text}"
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
        handle_cancel_background_task(&mut buf, &reg, &procs, None);
        let text = buffer_text(&buf);
        assert!(
            text.contains("Usage:") && text.contains("agent:") && text.contains("process:"),
            "None id should render a Usage: line with both prefixes, got: {text}"
        );
    }
}
