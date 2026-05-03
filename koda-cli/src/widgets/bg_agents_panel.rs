//! Interactive `/agents` panel — list/detail/cancel state machine for
//! the unified background-tasks surface (#1191 Candidate 2).
//!
//! Replaces the flat-text dump previously emitted by
//! [`crate::tui_bg_tasks::handle_list_background_tasks`]. Field-study
//! comparison (issue #1198) showed Codex / Claude Code / Gemini CLI all
//! converge on an *interactive* surface for background work — none stay
//! text-only. This widget adopts Claude Code's footer-pill → list →
//! detail dialog pattern because it matches what koda already has
//! (status pill + unified bg list + dropdown infra).
//!
//! ## State machine
//!
//! ```text
//!     ┌──────────┐  Enter   ┌────────┐
//!     │   List   │─────────▶│ Detail │
//!     │          │◀─────────│        │
//!     └──────────┘   Esc    └────────┘
//!          │ ▲
//!        c │ │ y/n/Esc
//!          ▼ │
//!   ┌──────────────┐
//!   │ ConfirmCancel│
//!   └──────────────┘
//! ```
//!
//! - `↑`/`↓`        navigate selection (List mode only)
//! - `Enter`        toggle Detail for the selected row
//! - `c`            request cancel for the selected row
//! - `y`            confirm cancel (ConfirmCancel mode)
//! - `n` / `Esc`    abort cancel (ConfirmCancel mode)
//! - `Esc` / `q`    back/dismiss (Detail back to List; List closes panel)
//!
//! ## Snapshot semantics
//!
//! State captures a snapshot of agent + process registries at panel-open
//! time and **does not auto-refresh** while the panel is open. This
//! matches the slash-menu and shortcuts-overlay pattern in koda — open
//! menus show frozen content. A future iteration can layer push-refresh
//! on top of `EngineEvent::BgTaskUpdate`; deferred to keep this PR
//! focused (Zen of Python: simple is better than complex).
//!
//! Cancellation does live-mutate the registries (since it must), but the
//! panel doesn't observe the resulting status change until the user
//! reopens the panel. That's an acceptable v1 quirk — the typical
//! workflow is "cancel and walk away", not "cancel and watch".

use crate::tui_bg_tasks::{
    agent_status_spans, command_head, format_age, process_status_spans, summary_preview,
};
use crate::tui_output::{BOLD, CYAN, DIM, GREEN, RED, WARM_ACCENT};
use koda_core::bg_agent::BgTaskSnapshot;
use koda_core::tools::bg_process::BgProcessSnapshot;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};

// ── Row model ──────────────────────────────────────────────────────────
//
// Agents and processes have different snapshot types but render identically
// in the panel. Wrap them in one enum so the renderer / navigation logic
// has a single iteration target — DRY trumps preserving the type split.

/// One row in the panel — either a background agent or a background
/// shell process. Both types render the same column shape (id, name,
/// age, status), and detail mode varies only in what extra fields are
/// shown.
#[derive(Debug, Clone)]
pub enum BgPanelRow {
    Agent(BgTaskSnapshot),
    Process(BgProcessSnapshot),
}

impl BgPanelRow {
    /// Display id with type prefix: `agent:N` or `process:PID`.
    pub fn display_id(&self) -> String {
        match self {
            BgPanelRow::Agent(s) => format!("agent:{}", s.task_id),
            BgPanelRow::Process(s) => format!("process:{}", s.pid),
        }
    }

    /// Display name — agent role for agents, command head for processes.
    pub fn display_name(&self) -> String {
        match self {
            BgPanelRow::Agent(s) => s.agent_name.clone(),
            BgPanelRow::Process(s) => command_head(&s.command),
        }
    }

    /// Wall-clock age string (e.g. `5m`).
    pub fn display_age(&self) -> String {
        match self {
            BgPanelRow::Agent(s) => format_age(s.age),
            BgPanelRow::Process(s) => format_age(s.age),
        }
    }

    /// Status label spans (icon + colored text).
    pub fn status_spans(&self) -> Vec<Span<'static>> {
        match self {
            BgPanelRow::Agent(s) => agent_status_spans(&s.status),
            BgPanelRow::Process(s) => process_status_spans(&s.status),
        }
    }
}

// ── State ──────────────────────────────────────────────────────────────

/// What the panel is currently showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BgPanelMode {
    /// List of all bg tasks; arrow keys select.
    List,
    /// Expanded detail of the selected task (full prompt / summary / error).
    Detail,
    /// `[y]/[n]` confirmation before firing the cancel.
    ConfirmCancel,
}

/// Panel state. Owned by `MenuContent::BgAgentsPanel(...)` for the
/// duration of the popup.
#[derive(Debug, Clone)]
pub struct BgAgentsPanelState {
    /// Unified row list captured at panel-open time. Agents come first,
    /// then processes — same ordering the legacy flat printer used
    /// (registry order within each group).
    pub rows: Vec<BgPanelRow>,
    /// Index of the highlighted row. Always in `0..rows.len()` when
    /// `!rows.is_empty()`; meaningless and ignored when `rows.is_empty()`.
    pub selection: usize,
    /// Current display mode.
    pub mode: BgPanelMode,
}

impl BgAgentsPanelState {
    /// Build panel state from current registry snapshots. Both lists
    /// are concatenated in the order the legacy `/agents` flat dump
    /// used (agents first, processes second).
    pub fn new(agents: Vec<BgTaskSnapshot>, processes: Vec<BgProcessSnapshot>) -> Self {
        let mut rows: Vec<BgPanelRow> = agents.into_iter().map(BgPanelRow::Agent).collect();
        rows.extend(processes.into_iter().map(BgPanelRow::Process));
        Self {
            rows,
            selection: 0,
            mode: BgPanelMode::List,
        }
    }

    /// `true` when there's nothing to show. The renderer uses this
    /// to draw the empty-state message instead of the table.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Move selection up by 1 (List mode only); wraps at the top.
    /// No-op when empty.
    pub fn up(&mut self) {
        if !self.rows.is_empty() && self.mode == BgPanelMode::List {
            self.selection = if self.selection == 0 {
                self.rows.len() - 1
            } else {
                self.selection - 1
            };
        }
    }

    /// Move selection down by 1 (List mode only); wraps at the bottom.
    /// No-op when empty.
    pub fn down(&mut self) {
        if !self.rows.is_empty() && self.mode == BgPanelMode::List {
            self.selection = (self.selection + 1) % self.rows.len();
        }
    }

    /// Toggle Detail mode for the selected row. Pressing Enter in
    /// Detail mode goes back to List.
    pub fn toggle_detail(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        self.mode = match self.mode {
            BgPanelMode::List => BgPanelMode::Detail,
            BgPanelMode::Detail => BgPanelMode::List,
            // ConfirmCancel doesn't toggle to detail — Enter in
            // ConfirmCancel is a no-op (use y/n explicitly).
            BgPanelMode::ConfirmCancel => BgPanelMode::ConfirmCancel,
        };
    }

    /// Open the cancel confirmation for the selected row. No-op if
    /// empty or already in ConfirmCancel.
    pub fn request_cancel(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        if self.mode != BgPanelMode::ConfirmCancel {
            self.mode = BgPanelMode::ConfirmCancel;
        }
    }

    /// Back out one mode level: Detail/ConfirmCancel → List. Returns
    /// `true` when state actually changed (caller treats this as
    /// "consumed the key"); `false` from List mode (caller should
    /// close the whole panel).
    pub fn back(&mut self) -> bool {
        match self.mode {
            BgPanelMode::Detail | BgPanelMode::ConfirmCancel => {
                self.mode = BgPanelMode::List;
                true
            }
            BgPanelMode::List => false,
        }
    }

    /// The currently-selected row, if any.
    pub fn selected(&self) -> Option<&BgPanelRow> {
        self.rows.get(self.selection)
    }
}

// ── Rendering ──────────────────────────────────────────────────────────

/// Total visible row count for the panel — used by the viewport layout
/// to size the menu_area Rect. Includes header + list/detail body +
/// hint footer.
pub fn visible_height(state: &BgAgentsPanelState) -> u16 {
    if state.is_empty() {
        // header (1) + blank (1) + "no tasks" (1) + blank (1) + hint (1) = 5
        return 5;
    }
    match state.mode {
        BgPanelMode::List => {
            // header (1) + column header (1) + N rows + blank (1) + hint (1)
            (4 + state.rows.len() as u16).min(20)
        }
        BgPanelMode::Detail => {
            // header (1) + ID line (1) + status line (1) + blank (1)
            // + body (up to 8 wrapped lines) + blank (1) + hint (1)
            13
        }
        BgPanelMode::ConfirmCancel => {
            // header (1) + question (1) + blank (1) + hint (1) = 4
            4
        }
    }
}

/// Render the panel into `area`. The widget is responsible for its own
/// header/footer; the caller (`tui_viewport`) just provides the Rect
/// and the buffer.
pub fn render(state: &BgAgentsPanelState, area: Rect, buf: &mut Buffer) {
    let mut lines: Vec<Line<'static>> = Vec::new();

    // Header — always shown, mode-aware so the user knows where they are.
    let header = match state.mode {
        BgPanelMode::List => format!(" 🐾 Background tasks ({})", state.rows.len()),
        BgPanelMode::Detail => " 🐾 Background tasks — detail".to_string(),
        BgPanelMode::ConfirmCancel => " 🐾 Background tasks — confirm cancel".to_string(),
    };
    lines.push(Line::from(Span::styled(header, BOLD)));

    if state.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled("  No background tasks.", DIM)));
        lines.push(Line::default());
        lines.push(Line::from(Span::styled("  esc/q close", DIM)));
        Paragraph::new(lines).render(area, buf);
        return;
    }

    match state.mode {
        BgPanelMode::List => render_list(state, &mut lines),
        BgPanelMode::Detail => render_detail(state, &mut lines),
        BgPanelMode::ConfirmCancel => render_confirm(state, &mut lines),
    }

    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .render(area, buf);
}

fn render_list(state: &BgAgentsPanelState, lines: &mut Vec<Line<'static>>) {
    // Column widths sized to the longest actual id/name (min 8 each).
    let id_col = state
        .rows
        .iter()
        .map(|r| r.display_id().len())
        .max()
        .unwrap_or(8)
        .max(8);
    let name_col = state
        .rows
        .iter()
        .map(|r| r.display_name().len())
        .max()
        .unwrap_or(8)
        .max(8);

    lines.push(Line::from(Span::styled(
        format!(
            "  {:<id_col$}  {:<name_col$}  {:<6}  STATUS",
            "ID", "NAME", "AGE"
        ),
        DIM,
    )));

    for (i, row) in state.rows.iter().enumerate() {
        let is_sel = i == state.selection;
        let cursor = if is_sel { "▶ " } else { "  " };
        let prefix_style = if is_sel {
            WARM_ACCENT
        } else {
            Style::default()
        };
        let mut spans = vec![
            Span::styled(cursor, prefix_style),
            Span::styled(
                format!(
                    "{:<id_col$}  {:<name_col$}  {:<6}  ",
                    row.display_id(),
                    row.display_name(),
                    row.display_age(),
                ),
                if is_sel {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                },
            ),
        ];
        spans.extend(row.status_spans());
        lines.push(Line::from(spans));
    }

    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "  ↑↓ navigate  enter detail  c cancel  esc/q close",
        DIM,
    )));
}

fn render_detail(state: &BgAgentsPanelState, lines: &mut Vec<Line<'static>>) {
    let Some(row) = state.selected() else {
        return;
    };

    // ID line.
    lines.push(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(row.display_id(), CYAN.add_modifier(Modifier::BOLD)),
        Span::styled("  ", Style::default()),
        Span::styled(row.display_name(), Style::default()),
        Span::styled(format!("  ({})", row.display_age()), DIM),
    ]));

    // Status line.
    let mut status_line = vec![Span::styled("  Status: ", DIM)];
    status_line.extend(row.status_spans());
    lines.push(Line::from(status_line));

    lines.push(Line::default());

    // Body — type-specific detail content.
    match row {
        BgPanelRow::Agent(snap) => {
            lines.push(Line::from(Span::styled("  Prompt:", DIM)));
            // Show the prompt verbatim — it's what the parent delegated.
            // Wrap is applied at render time by Paragraph::wrap so we
            // don't pre-truncate here.
            lines.push(Line::from(Span::raw(format!("  {}", snap.prompt))));
            // If completed/errored, also show the summary/error.
            use koda_core::bg_agent::AgentStatus;
            match &snap.status {
                AgentStatus::Completed { summary } if !summary.is_empty() => {
                    lines.push(Line::default());
                    lines.push(Line::from(Span::styled("  Result:", DIM)));
                    lines.push(Line::from(Span::styled(
                        format!("  {}", summary_preview(summary)),
                        GREEN,
                    )));
                }
                AgentStatus::Errored { error } if !error.is_empty() => {
                    lines.push(Line::default());
                    lines.push(Line::from(Span::styled("  Error:", DIM)));
                    lines.push(Line::from(Span::styled(
                        format!("  {}", summary_preview(error)),
                        RED,
                    )));
                }
                _ => {}
            }
        }
        BgPanelRow::Process(snap) => {
            lines.push(Line::from(Span::styled("  Command:", DIM)));
            lines.push(Line::from(Span::raw(format!("  {}", snap.command))));
        }
    }

    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "  enter back  c cancel  esc back",
        DIM,
    )));
}

fn render_confirm(state: &BgAgentsPanelState, lines: &mut Vec<Line<'static>>) {
    let Some(row) = state.selected() else {
        return;
    };
    lines.push(Line::from(vec![
        Span::styled("  Cancel ", DIM),
        Span::styled(row.display_id(), CYAN.add_modifier(Modifier::BOLD)),
        Span::styled(" (", DIM),
        Span::styled(row.display_name(), Style::default()),
        Span::styled(")?", DIM),
    ]));
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "  [y] confirm   [n]/esc abort",
        DIM,
    )));
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    //! Panel state-machine + render smoke tests.
    //!
    //! `BgTaskSnapshot` and `BgProcessSnapshot` are `#[non_exhaustive]`,
    //! so we can't construct them by literal. Instead we drive the real
    //! [`BgAgentRegistry`] API to mint genuine snapshots — same pattern
    //! the [`crate::tui_bg_tasks`] tests use. We don't exercise process
    //! rows here (would need to spawn a real child); process rendering
    //! is covered transitively by [`crate::tui_bg_tasks`]'s formatter
    //! tests since `BgPanelRow::Process` just delegates.

    use super::*;
    use koda_core::bg_agent::{AgentStatus, BgAgentRegistry};
    use tokio::sync::watch;
    use tokio_util::sync::CancellationToken;

    /// Reserve + attach a real entry on the registry and return its
    /// task_id. Mirrors [`crate::tui_bg_tasks`]'s `register_entry` but
    /// trimmed to what the panel tests need (we don't care about the
    /// returned senders — the registry holds them).
    ///
    /// Allows overriding the live status by sending on the returned
    /// status sender before snapshotting (so tests can exercise
    /// Running/Completed/Errored render paths).
    fn register(
        reg: &BgAgentRegistry,
        agent_name: &str,
        prompt: &str,
    ) -> (u32, watch::Sender<AgentStatus>) {
        let parent = CancellationToken::new();
        let r = reg.reserve(&parent, None);
        let task_id = r.task_id;
        let status_tx = r.status_tx;
        let noop = tokio::spawn(async {});
        // Same shape as `tui_bg_tasks::tests::register_entry` — see
        // that helper's docstring for the spawner=None rationale.
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
        (task_id, status_tx)
    }

    /// Build a panel state from a registry's current snapshot. Tiny
    /// helper to keep each test focused on the assertion, not the
    /// fixture plumbing.
    fn panel_from(reg: &BgAgentRegistry) -> BgAgentsPanelState {
        BgAgentsPanelState::new(reg.snapshot(), vec![])
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_state_renders_no_tasks() {
        let state = BgAgentsPanelState::new(vec![], vec![]);
        assert!(state.is_empty());
        assert_eq!(visible_height(&state), 5);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn new_concatenates_agents_then_processes() {
        let reg = BgAgentRegistry::new();
        let (id1, _) = register(&reg, "explore", "x");
        let (id2, _) = register(&reg, "verify", "y");
        let state = panel_from(&reg);
        assert_eq!(state.rows.len(), 2);
        assert_eq!(state.rows[0].display_id(), format!("agent:{id1}"));
        assert_eq!(state.rows[1].display_id(), format!("agent:{id2}"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn navigation_wraps_at_both_ends() {
        let reg = BgAgentRegistry::new();
        register(&reg, "a", "x");
        register(&reg, "b", "y");
        let mut state = panel_from(&reg);
        assert_eq!(state.selection, 0);
        state.up(); // wrap to last
        assert_eq!(state.selection, 1);
        state.down(); // wrap back to first
        assert_eq!(state.selection, 0);
        state.down();
        assert_eq!(state.selection, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn navigation_noop_in_detail_mode() {
        let reg = BgAgentRegistry::new();
        register(&reg, "a", "x");
        register(&reg, "b", "y");
        let mut state = panel_from(&reg);
        state.toggle_detail();
        assert_eq!(state.mode, BgPanelMode::Detail);
        state.down();
        assert_eq!(state.selection, 0, "selection must not move in detail mode");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn toggle_detail_round_trips() {
        let reg = BgAgentRegistry::new();
        register(&reg, "a", "x");
        let mut state = panel_from(&reg);
        assert_eq!(state.mode, BgPanelMode::List);
        state.toggle_detail();
        assert_eq!(state.mode, BgPanelMode::Detail);
        state.toggle_detail();
        assert_eq!(state.mode, BgPanelMode::List);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn request_cancel_sets_confirm_mode() {
        let reg = BgAgentRegistry::new();
        register(&reg, "a", "x");
        let mut state = panel_from(&reg);
        state.request_cancel();
        assert_eq!(state.mode, BgPanelMode::ConfirmCancel);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn back_returns_false_from_list_to_signal_panel_close() {
        let reg = BgAgentRegistry::new();
        register(&reg, "a", "x");
        let mut state = panel_from(&reg);
        assert!(
            !state.back(),
            "List back should return false (panel closes)"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn back_returns_true_from_detail() {
        let reg = BgAgentRegistry::new();
        register(&reg, "a", "x");
        let mut state = panel_from(&reg);
        state.toggle_detail();
        assert!(state.back());
        assert_eq!(state.mode, BgPanelMode::List);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn back_returns_true_from_confirm() {
        let reg = BgAgentRegistry::new();
        register(&reg, "a", "x");
        let mut state = panel_from(&reg);
        state.request_cancel();
        assert!(state.back());
        assert_eq!(state.mode, BgPanelMode::List);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_state_navigation_is_safe() {
        let mut state = BgAgentsPanelState::new(vec![], vec![]);
        // None of these should panic on an empty list.
        state.up();
        state.down();
        state.toggle_detail();
        state.request_cancel();
        assert_eq!(state.mode, BgPanelMode::List);
        assert_eq!(state.selection, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn visible_height_grows_with_rows_in_list_mode() {
        let reg_one = BgAgentRegistry::new();
        register(&reg_one, "a", "x");
        let one = panel_from(&reg_one);

        let reg_three = BgAgentRegistry::new();
        register(&reg_three, "a", "x");
        register(&reg_three, "b", "y");
        register(&reg_three, "c", "z");
        let three = panel_from(&reg_three);

        assert!(visible_height(&three) > visible_height(&one));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn visible_height_caps_at_20_for_huge_lists() {
        let reg = BgAgentRegistry::new();
        for i in 0..50 {
            register(&reg, &format!("a{i}"), "x");
        }
        let state = panel_from(&reg);
        assert_eq!(visible_height(&state), 20);
    }

    /// Visual smoke: render a multi-status panel into a fixed buffer
    /// and assert key strings appear. Cheap regression net for "did
    /// the column layout silently break".
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_render_smoke() {
        let reg = BgAgentRegistry::new();
        let (id1, status1) = register(&reg, "explore", "map repo");
        register(&reg, "verify", "check tests");
        // Bump one task to Running so the smoke covers the colored
        // status path, not just Pending.
        let _ = status1.send(AgentStatus::Running { iter: 3 });

        let state = panel_from(&reg);
        let area = Rect::new(0, 0, 80, visible_height(&state));
        let mut buf = Buffer::empty(area);
        render(&state, area, &mut buf);

        let text: String = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            text.contains("Background tasks (2)"),
            "header missing:\n{text}"
        );
        assert!(
            text.contains(&format!("agent:{id1}")),
            "agent row missing:\n{text}"
        );
        assert!(text.contains("explore"), "agent name missing:\n{text}");
        assert!(text.contains("verify"), "second agent missing:\n{text}");
        assert!(text.contains("↑↓ navigate"), "footer hint missing:\n{text}");
    }
}
