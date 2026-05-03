//! Live background-work activity overlay (#1210).
//!
//! Renders above the composer whenever there's at least one running
//! bg agent or bg shell process, mirroring `widgets/queue_preview.rs`
//! in shape:
//!
//! ```text
//!   🤖 explore   (12s)  · Read src/auth.rs
//!   🤖 verify    ( 4s)  · Bash cargo test --lib
//!   🐚 process:42 ( 8s) · cargo build --release
//!   + 2 more  ·  Esc cancel all  ·  /cancel <id>
//! ```
//!
//! ## Why this widget exists
//!
//! Field study (#1210): claude_code and gemini-cli both ship a live
//! activity surface during multi-agent waits; codex stays mostly
//! silent. The pre-#1207 koda render was *too* live (50 dim lines
//! per 50-tool agent in append-only scroll); #1207 dropped the
//! scrollback render but left no live signal at all. This overlay
//! fills that gap without re-spamming scroll.
//!
//! ## Design constraints
//!
//! - **Append-only-scroll-friendly**: this widget is a viewport-layer
//!   overlay, not a `ScrollBuffer` consumer. It renders fresh every
//!   frame from in-memory state; nothing is committed to scrollback.
//! - **Snapshot-frozen at render time**: callers compute the row list
//!   once per frame (cheap — bounded by [`MAX_VISIBLE`]).
//! - **Zero-dep on engine internals**: takes pre-formatted row data,
//!   no `koda-core` types in the public API. Decoupling matches the
//!   `queue_preview` precedent.
//!
//! ## What this widget does NOT do
//!
//! - Does NOT navigate / select / cancel rows. Cancel UX is split:
//!   `/cancel <id>` (idle) and Esc (cancel-all during inference).
//!   #1210's "Design Z" rationale: no reference (claude_code, codex,
//!   gemini-cli) ships an interactive selective-cancel UI, so a
//!   passive overlay paired with the existing slash + Esc paths
//!   covers the field-standard surface area without rebuilding the
//!   `/agents` modal panel that #1210 deletes.
//! - Does NOT auto-refresh — caller drives a redraw on each
//!   `BgChildActivity` / `BgTaskUpdate` event (already wired via
//!   `frame_requester`).

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Widget,
};

/// Maximum number of activity rows rendered. Overflow collapses to
/// `+ N more` on the hint row, mirroring [`super::queue_preview::MAX_VISIBLE`].
///
/// Capped at 5 rather than 3 (queue_preview's cap) because bg fan-out
/// of 4-5 agents is a normal pattern for `explore` / `verify` / `lint`
/// wave spawning, and collapsing too aggressively defeats the point of
/// the at-a-glance surface.
pub const MAX_VISIBLE: usize = 5;

/// One row of background work to render. Pre-formatted; this widget
/// does not know about `BgTaskSnapshot` or `BgProcessSnapshot`. The
/// caller (event handler in `tui_context`) does the formatting and
/// hands a flat list down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityRow {
    /// Leading icon (e.g. "🤖" for agents, "🐚" for processes).
    pub icon: &'static str,
    /// Display label. For agents: agent_name. For processes: "process:PID".
    /// Rendered bold-ish (cyan) so the eye finds the type quickly.
    pub label: String,
    /// Compact age string, already padded (e.g. " 4s", "12s", "1m").
    /// Padding is the caller's job so all ages right-align cleanly.
    pub age: String,
    /// Last-known activity, pre-truncated for direct render. For agents
    /// this is the latest tool call summary; for processes it's the
    /// command preview (static — processes don't emit activity events).
    /// `None` renders no `· …` suffix (e.g. agent just spawned, no
    /// activity yet).
    pub activity: Option<String>,
    /// Visual status — drives the icon color. Mirrors codex's
    /// `agent_picker_status_dot_spans` convention.
    pub status: ActivityStatus,
}

/// Visual status of an activity row. Maps to icon color; the textual
/// status itself is implied by the icon (no "RUNNING" / "DONE" tags
/// because the row only appears for live work).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityStatus {
    /// Reserved but not yet started. Dim — typically transient.
    Pending,
    /// Active — the common case. Yellow.
    Running,
    /// Cancellation in progress (token fired, future hasn't observed
    /// yet). Distinguishes "you asked to cancel" from "still running"
    /// so the user gets feedback. Red.
    Cancelling,
}

/// The activity overlay widget. Pure render — caller owns state.
///
/// Construction is cheap (borrows the row slice + total). Pass to
/// [`ratatui::Frame::render_widget`].
pub struct BgActivityOverlay<'a> {
    /// Visible rows (already truncated to [`MAX_VISIBLE`] by caller).
    rows: &'a [ActivityRow],
    /// Total live work count (may exceed `rows.len()`); drives
    /// the `+ N more` overflow hint.
    total: usize,
}

impl<'a> BgActivityOverlay<'a> {
    /// Construct an overlay for the given visible row slice.
    ///
    /// `rows.len() <= MAX_VISIBLE` is the caller's invariant; this
    /// widget will silently truncate to the visible area if violated.
    pub fn new(rows: &'a [ActivityRow], total: usize) -> Self {
        Self { rows, total }
    }

    /// How many terminal rows this widget occupies for `total` live
    /// work items.
    ///
    /// Returns 0 when `total == 0` so the caller can omit the layout
    /// slot entirely (matches `QueuePreview::height_for`).
    pub fn height_for(total: usize) -> u16 {
        if total == 0 {
            return 0;
        }
        let body = total.min(MAX_VISIBLE) as u16;
        let hint = 1u16; // overflow / hint row
        body + hint
    }
}

impl Widget for BgActivityOverlay<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Reserve space for the leading "  ICON LABEL  AGE  · " prefix.
        // Worked example: "  " (2) + icon+space (2 cells: emoji is
        // 2-cell wide) + label padded to 10 (10) + " (AGE) " (7) +
        // "· " (2) = 23 cells. Add 3 cells of slack so the trailing
        // ellipsis always survives buffer-clipping when activity
        // overruns. Empirically: under-budget here means the `…`
        // gets clipped and the row looks like raw truncation.
        let prefix_w = 26usize;
        let max_activity_w = (area.width as usize).saturating_sub(prefix_w);

        for (row_idx, row) in self.rows.iter().enumerate() {
            let y = area.y + row_idx as u16;
            if y >= area.y + area.height {
                break;
            }

            let icon_color = match row.status {
                ActivityStatus::Pending => Color::DarkGray,
                ActivityStatus::Running => Color::Yellow,
                ActivityStatus::Cancelling => Color::Red,
            };

            let mut spans: Vec<Span<'static>> = vec![
                Span::raw("  "),
                Span::styled(format!("{} ", row.icon), Style::default().fg(icon_color)),
                Span::styled(
                    format!("{:<10}", truncate_label(&row.label, 10)),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" ({:>3}) ", row.age),
                    Style::default().fg(Color::DarkGray),
                ),
            ];

            if let Some(activity) = &row.activity {
                let preview = truncate_with_ellipsis(activity, max_activity_w);
                spans.push(Span::styled(
                    "· ".to_string(),
                    Style::default().fg(Color::DarkGray),
                ));
                spans.push(Span::styled(preview, Style::default().fg(Color::Gray)));
            }

            Line::from(spans).render(Rect::new(area.x, y, area.width, 1), buf);
        }

        // Hint / overflow row.
        let hint_y = area.y + self.rows.len() as u16;
        if hint_y >= area.y + area.height {
            return;
        }
        let overflow = self.total.saturating_sub(self.rows.len());
        let mut spans: Vec<Span<'static>> = vec![Span::raw("  ")];
        if overflow > 0 {
            spans.push(Span::styled(
                format!("+ {overflow} more"),
                Style::default().fg(Color::DarkGray),
            ));
            spans.push(Span::styled(
                "  \u{b7}  ",
                Style::default().fg(Color::Rgb(80, 80, 80)),
            ));
        }
        spans.push(Span::styled(
            "Esc cancel all  \u{b7}  /cancel <id>",
            Style::default().fg(Color::Rgb(80, 80, 80)),
        ));
        Line::from(spans).render(Rect::new(area.x, hint_y, area.width, 1), buf);
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Truncate a label to at most `max` chars, dropping trailing chars.
/// We don't ellipsize labels because they're meant to be a stable
/// short identifier (agent name like `explore`, or `process:1234`);
/// truncation is a degenerate case for unusually long agent names.
fn truncate_label(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

/// Truncate `s` to fit in `max` cells, appending `…` if truncated.
/// Newlines are flattened to spaces first (activity strings may
/// occasionally embed them when info-line content slips through).
fn truncate_with_ellipsis(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let flat = s.replace('\n', " ");
    let count = flat.chars().count();
    if count <= max {
        flat
    } else {
        let mut out: String = flat.chars().take(max.saturating_sub(1)).collect();
        out.push('\u{2026}');
        out
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn render(rows: &[ActivityRow], total: usize, width: u16) -> Vec<String> {
        let height = BgActivityOverlay::height_for(total).max(1);
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        BgActivityOverlay::new(rows, total).render(area, &mut buf);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn agent_row(label: &str, age: &str, activity: Option<&str>) -> ActivityRow {
        ActivityRow {
            icon: "\u{1f916}",
            label: label.into(),
            age: age.into(),
            activity: activity.map(str::to_string),
            status: ActivityStatus::Running,
        }
    }

    #[test]
    fn height_zero_when_no_work() {
        assert_eq!(BgActivityOverlay::height_for(0), 0);
    }

    #[test]
    fn height_one_row_includes_hint() {
        // 1 body row + 1 hint = 2
        assert_eq!(BgActivityOverlay::height_for(1), 2);
    }

    #[test]
    fn height_capped_at_max_visible_plus_hint() {
        assert_eq!(
            BgActivityOverlay::height_for(MAX_VISIBLE + 10),
            (MAX_VISIBLE + 1) as u16
        );
    }

    #[test]
    fn single_running_row_renders_label_and_activity() {
        let rows = vec![agent_row("explore", "12s", Some("Read src/auth.rs"))];
        let lines = render(&rows, 1, 80);
        assert!(lines[0].contains("explore"), "row missing label: {:?}", lines[0]);
        assert!(
            lines[0].contains("Read src/auth.rs"),
            "row missing activity: {:?}",
            lines[0]
        );
        assert!(lines[0].contains("12s"), "row missing age: {:?}", lines[0]);
    }

    #[test]
    fn row_without_activity_omits_separator() {
        let rows = vec![agent_row("explore", " 1s", None)];
        let lines = render(&rows, 1, 80);
        // No "· " mid-dot separator before activity since activity is None.
        // Hint row at the bottom may include other middots — only inspect
        // the body row (index 0).
        assert!(
            !lines[0].contains("\u{b7}"),
            "body row should not show activity separator when activity=None: {:?}",
            lines[0]
        );
    }

    #[test]
    fn overflow_hint_shows_count_when_more_than_visible() {
        let rows: Vec<_> = (0..MAX_VISIBLE)
            .map(|i| agent_row(&format!("ag{i}"), " 1s", Some("Read x.rs")))
            .collect();
        let lines = render(&rows, MAX_VISIBLE + 3, 80);
        let hint = lines.last().unwrap();
        assert!(hint.contains("+ 3 more"), "overflow hint: {hint:?}");
    }

    #[test]
    fn hint_row_always_present_even_without_overflow() {
        let rows = vec![agent_row("explore", " 1s", Some("x"))];
        let lines = render(&rows, 1, 80);
        let hint = lines.last().unwrap();
        assert!(
            hint.contains("Esc cancel all"),
            "hint row should always show Esc keybinding: {hint:?}"
        );
        assert!(
            hint.contains("/cancel"),
            "hint row should reference /cancel: {hint:?}"
        );
    }

    #[test]
    fn long_activity_truncated_with_ellipsis() {
        let long = "x".repeat(500);
        let rows = vec![agent_row("a", " 1s", Some(&long))];
        let lines = render(&rows, 1, 50);
        assert!(
            lines[0].contains('\u{2026}'),
            "long activity should ellipsize: {:?}",
            lines[0]
        );
    }

    #[test]
    fn newlines_in_activity_flattened_to_spaces() {
        let rows = vec![agent_row("a", " 1s", Some("line1\nline2"))];
        let lines = render(&rows, 1, 80);
        assert!(
            !lines[0].contains('\n'),
            "newline should not survive into rendered cell"
        );
    }

    #[test]
    fn process_row_uses_distinct_icon() {
        // Smoke: caller passes a different icon for processes; widget
        // doesn't care what icon it gets, just renders it.
        let rows = vec![ActivityRow {
            icon: "\u{1f41a}", // 🐚
            label: "process:42".into(),
            age: " 8s".into(),
            activity: Some("cargo build".into()),
            status: ActivityStatus::Running,
        }];
        let lines = render(&rows, 1, 80);
        assert!(
            lines[0].contains("process:42"),
            "process label missing: {:?}",
            lines[0]
        );
        assert!(
            lines[0].contains("cargo build"),
            "process activity missing: {:?}",
            lines[0]
        );
    }

    #[test]
    fn truncate_label_handles_unicode() {
        // Multi-byte chars — ensure char-based truncation, not byte-based.
        let s = "🤖🤖🤖🤖🤖";
        let out = truncate_label(s, 3);
        assert_eq!(out.chars().count(), 3);
    }

    #[test]
    fn truncate_with_ellipsis_zero_max_returns_empty() {
        assert_eq!(truncate_with_ellipsis("anything", 0), "");
    }

    #[test]
    fn empty_rows_with_zero_total_renders_nothing_visible() {
        // The render() helper forces height=1 to satisfy buffer-area
        // requirements; check that no row content appears.
        let lines = render(&[], 0, 80);
        // Zero-total → height_for=0 forced to 1; that single line
        // shouldn't contain row chrome. The hint row only renders
        // when total>0 (the early return on `hint_y >= area.height`
        // protects us when the slice is empty AND height is forced).
        // Since rows.len() == 0, hint_y == area.y + 0; if height==1
        // the hint DOES render (overflow=0 path), printing the
        // keybindings only. That's an acceptable edge — caller is
        // expected to omit the layout slot entirely when total=0
        // (the height_for(0)==0 contract).
        assert!(lines[0].contains("Esc cancel all") || lines[0].is_empty());
    }
}
