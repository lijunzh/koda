//! Status bar widget for the inline TUI viewport.
//!
//! Shows: mode | model name | context usage bar | (conditional segments)
//!
//! # Why no `cwd` segment?
//!
//! The cwd was previously rendered as the leftmost segment (#1105),
//! but the welcome banner already shows `cwd` and it can't change
//! during a session. Carrying it in the persistent footer was pure
//! redundancy — the status bar is prime real estate that should only
//! show information that changes. Removed in #1194/#1195 follow-up;
//! the codex / gemini-cli status lines also omit cwd for the same
//! reason.

use koda_core::mcp::manager::McpStatusBarInfo;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Widget,
};

pub struct StatusBar<'a> {
    model: &'a str,
    mode_label: &'a str,
    context_pct: u32,
    queue_len: usize,
    /// Elapsed seconds during inference (0 = idle).
    elapsed_secs: u64,
    /// Last turn stats (shown after inference completes).
    last_turn: Option<&'a TurnStats>,
    /// Scroll position info (offset, total) — shown when not at bottom.
    scroll_info: Option<(usize, usize)>,
    /// MCP server status (None = no servers configured, hidden).
    mcp_info: Option<McpStatusBarInfo>,
    /// Vim mode label (e.g. "NORMAL", "INSERT", "OPERATOR-PENDING").
    /// `None` hides the segment entirely (vim editing disabled).
    /// Sourced from `composer::textarea::TextArea::vim_mode_label()`
    /// which the status-bar caller passes in via [`with_vim_label`].
    /// Added in PR 3 of #1178 (vim mode wire-up).
    vim_label: Option<&'a str>,
}

/// Stats from the most recent inference turn.
#[derive(Debug, Clone, Default)]
pub struct TurnStats {
    /// Input tokens billed for this turn.
    pub tokens_in: i64,
    /// Output tokens generated this turn.
    pub tokens_out: i64,
    /// Tokens served from the prompt cache (cost $0).
    pub cache_read: i64,
    pub elapsed_ms: u64,
    pub rate: f64,
}

impl<'a> StatusBar<'a> {
    pub fn new(model: &'a str, mode_label: &'a str, context_pct: u32) -> Self {
        Self {
            model,
            mode_label,
            context_pct,
            queue_len: 0,
            elapsed_secs: 0,
            last_turn: None,
            scroll_info: None,
            mcp_info: None,
            vim_label: None,
        }
    }

    pub fn with_queue(mut self, queue_len: usize) -> Self {
        self.queue_len = queue_len;
        self
    }

    pub fn with_elapsed(mut self, secs: u64) -> Self {
        self.elapsed_secs = secs;
        self
    }

    pub fn with_last_turn(mut self, stats: &'a TurnStats) -> Self {
        self.last_turn = Some(stats);
        self
    }

    pub fn with_scroll_info(mut self, offset: usize, total: usize) -> Self {
        self.scroll_info = Some((offset, total));
        self
    }

    pub fn with_mcp_info(mut self, info: McpStatusBarInfo) -> Self {
        self.mcp_info = Some(info);
        self
    }

    /// Set the active vim-mode label (NORMAL / INSERT / OPERATOR-PENDING).
    ///
    /// `None` (or any call site that doesn't invoke this) hides the segment
    /// entirely — vim editing is opt-in, so users who don't toggle it never
    /// see VIM clutter in the status bar. PR 3 of #1178; sourced from
    /// `composer::textarea::TextArea::vim_mode_label()`.
    pub fn with_vim_label(mut self, label: Option<&'a str>) -> Self {
        self.vim_label = label;
        self
    }
}

impl Widget for StatusBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // **#1232 §8a**: trust mode is the single most-safety-relevant
        // piece of state in the entire UI — the user must never be
        // unsure whether they're in Safe (confirm every mutation) or
        // Auto (auto-approve in-project mutations). Pre-fix the bar
        // showed lowercase ` safe ` / ` auto ` in plain colored text;
        // the prompt indicator showed a tiny `🔒>`. Easy to miss
        // both. Bug-review session that opened #1232: "User wasn't
        // sure whether they were in Safe or Auto."
        //
        // The new badge is icon + UPPERCASE + bold for every mode.
        // Auto used to render as a black-on-green inverted badge for
        // extra loudness, but a hardcoded `bg: Green` clashes badly
        // with terminal color schemes that already use a bright or
        // light green palette — the black foreground washes out and
        // "AUTO" becomes unreadable. Bold green text on the user's
        // own background works on every scheme. (#1243 → reverted in
        // this commit's PR; if #1241 needs more loudness post-flip,
        // use `Modifier::REVERSED` which inverts using the user's own
        // terminal colors instead of imposing fixed colors.)
        //
        // Color/icon choices match the prompt indicator in
        // `tui_viewport.rs` so users see the SAME color for the SAME
        // mode across both surfaces (pre-#1243 Safe was Cyan in the
        // prompt and Yellow in the bar — confusing inconsistency).
        // Drive-by: the dead `"strict"` arm is gone (TrustMode never
        // emits that label; only `plan`/`safe`/`auto`).
        let (mode_icon, mode_fg, mode_bg) = match self.mode_label {
            "plan" => ("\u{1f4cb}", Color::Cyan, None),
            "safe" => ("\u{1f512}", Color::Yellow, None),
            "auto" => ("\u{26a1}", Color::Green, None),
            // Defensive default for any future mode label that gets added
            // to `TrustMode` without updating this match. Renders as a
            // visible-but-bland "?" badge so it's obvious something is
            // miswired (rather than silently disappearing).
            _ => ("?", Color::DarkGray, None),
        };
        let mut mode_style = Style::default().fg(mode_fg).add_modifier(Modifier::BOLD);
        if let Some(bg) = mode_bg {
            mode_style = mode_style.bg(bg);
        }
        let mode_label_upper = self.mode_label.to_ascii_uppercase();

        let bar_width: u32 = 10;
        let filled = (self.context_pct * bar_width / 100).min(bar_width);
        let empty = bar_width - filled;
        let ctx_color = if self.context_pct >= 90 {
            Color::Red
        } else if self.context_pct >= 75 {
            Color::Yellow
        } else {
            Color::DarkGray
        };

        // Truncate long model names to keep status bar readable
        let model_display = if self.model.len() > 32 {
            format!("{}…", &self.model[..31])
        } else {
            self.model.to_string()
        };

        let mut spans = vec![];

        // Segment order (#1194/#1195 follow-up): mode → model → context.
        // Mode is FIRST because it's the most-frequently-changing piece
        // (Shift+Tab cycles Plan/Safe/Auto on demand) and has a distinct
        // color, so eye-anchoring it leftmost matches user attention.
        // Model is second; context bar last because it's the widest
        // segment and naturally tail-anchors the always-on cluster.
        spans.extend([
            Span::styled(format!(" {mode_icon} {mode_label_upper} "), mode_style),
            Span::styled("\u{2502}", Style::default().fg(Color::Rgb(60, 60, 60))),
            Span::styled(
                format!(" {model_display} "),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled("\u{2502}", Style::default().fg(Color::Rgb(60, 60, 60))),
            Span::styled(
                format!(
                    " {}{} {}%",
                    "\u{2588}".repeat(filled as usize),
                    "\u{2591}".repeat(empty as usize),
                    self.context_pct,
                ),
                Style::default().fg(ctx_color),
            ),
        ]);

        // Vim mode pill (PR 3 of #1178). Hidden when vim editing is
        // disabled (the textarea returns `None` from `vim_mode_label()`),
        // so non-vim users never see this segment. Magenta to make the
        // mode visually distinct from the trust-mode segment.
        if let Some(label) = self.vim_label {
            spans.push(Span::styled(
                "\u{2502}",
                Style::default().fg(Color::Rgb(60, 60, 60)),
            ));
            spans.push(Span::styled(
                format!(" VIM:{label} "),
                Style::default().fg(Color::Magenta),
            ));
        }

        // MCP server indicator (hidden when no servers configured)
        if let Some(mcp) = self.mcp_info {
            let mcp_color = if mcp.failed == 0 {
                Color::Green
            } else if mcp.connected > 0 {
                Color::Yellow
            } else {
                Color::Red
            };
            spans.push(Span::styled(
                "\u{2502}",
                Style::default().fg(Color::Rgb(60, 60, 60)),
            ));
            spans.push(Span::styled(
                format!(" \u{26a1}{}/{} ", mcp.connected, mcp.total),
                Style::default().fg(mcp_color),
            ));
        }

        // (#1158's bg-activity pill was removed in #1210 — the live
        // bg-activity overlay above the status bar shows "what's
        // running and what each task is doing", which is strictly
        // more useful than a count. The pill became redundant the
        // moment the live surface existed.)

        // Elapsed time during inference
        if self.elapsed_secs > 0 {
            spans.push(Span::styled(
                "\u{2502}",
                Style::default().fg(Color::Rgb(60, 60, 60)),
            ));
            spans.push(Span::styled(
                format!(" \u{23f3} {}s ", self.elapsed_secs),
                Style::default().fg(Color::Cyan),
            ));
        }

        // Queue indicator — show count + Ctrl+U hint
        if self.queue_len > 0 {
            spans.push(Span::styled(
                "\u{2502}",
                Style::default().fg(Color::Rgb(60, 60, 60)),
            ));
            spans.push(Span::styled(
                format!(" \u{1f4cb} {} queued ", self.queue_len),
                Style::default().fg(Color::Yellow),
            ));
            spans.push(Span::styled(
                "^U clear ",
                Style::default().fg(Color::Rgb(100, 100, 100)),
            ));
        }

        // Last turn stats (shown after inference, cleared on next turn)
        if let Some(stats) = self.last_turn {
            spans.push(Span::styled(
                "\u{2502}",
                Style::default().fg(Color::Rgb(60, 60, 60)),
            ));
            let time = if stats.elapsed_ms >= 1000 {
                format!("{:.1}s", stats.elapsed_ms as f64 / 1000.0)
            } else {
                format!("{}ms", stats.elapsed_ms)
            };

            // Show ↑in ↓out token counts so users can see full turn cost.
            let mut stat_str = format!(
                " ↑{} ↓{} · {} · {:.0} t/s ",
                stats.tokens_in, stats.tokens_out, time, stats.rate
            );
            // Cache hit indicator — only shown when nonzero (costs nothing).
            if stats.cache_read > 0 && stats.tokens_in > 0 {
                let pct = (stats.cache_read * 100) / stats.tokens_in;
                stat_str = format!(
                    " ↑{} ↓{} 🗄{pct}% · {} · {:.0} t/s ",
                    stats.tokens_in, stats.tokens_out, time, stats.rate
                );
            }
            spans.push(Span::styled(stat_str, Style::default().fg(Color::DarkGray)));
        }

        // Scroll position (when not at bottom)
        if let Some((offset, total)) = self.scroll_info {
            spans.push(Span::styled(
                "\u{2502}",
                Style::default().fg(Color::Rgb(60, 60, 60)),
            ));
            spans.push(Span::styled(
                format!(" \u{2191}{offset}/{total} "),
                Style::default().fg(Color::Yellow),
            ));
        }

        Line::from(spans).render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::widgets::Widget;

    /// Render a StatusBar into a buffer and return the text content.
    fn render_bar(bar: StatusBar<'_>, width: u16) -> String {
        let area = Rect::new(0, 0, width, 1);
        let mut buf = Buffer::empty(area);
        bar.render(area, &mut buf);
        // Extract text from buffer cells.
        (0..width)
            .map(|x| buf.cell((x, 0)).map(|c| c.symbol()).unwrap_or(" "))
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn mcp_indicator_hidden_when_no_servers() {
        let bar = StatusBar::new("gpt-4", "safe", 50);
        let text = render_bar(bar, 120);
        // No MCP info → no lightning bolt indicator.
        assert!(
            !text.contains('⚡'),
            "MCP indicator should be hidden: {text}"
        );
    }

    #[test]
    fn mcp_indicator_shows_connected_count() {
        let bar = StatusBar::new("gpt-4", "safe", 50).with_mcp_info(McpStatusBarInfo {
            connected: 2,
            failed: 0,
            total: 3,
        });
        let text = render_bar(bar, 120);
        assert!(text.contains("2/3"), "should show 2/3: {text}");
    }

    /// Find the fg color of the ⚡ MCP indicator in a rendered buffer.
    fn mcp_indicator_color(info: McpStatusBarInfo) -> Color {
        let bar = StatusBar::new("gpt-4", "safe", 50).with_mcp_info(info);
        let area = Rect::new(0, 0, 120, 1);
        let mut buf = Buffer::empty(area);
        bar.render(area, &mut buf);
        let mcp_cell = (0..120u16)
            .find(|&x| buf.cell((x, 0)).map(|c| c.symbol()) == Some("⚡"))
            .expect("should have ⚡ cell");
        buf.cell((mcp_cell, 0)).unwrap().fg
    }

    #[test]
    fn mcp_color_green_when_all_connected() {
        let fg = mcp_indicator_color(McpStatusBarInfo {
            connected: 3,
            failed: 0,
            total: 3,
        });
        assert_eq!(fg, Color::Green, "all connected → green");
    }

    #[test]
    fn mcp_color_yellow_when_partial() {
        let fg = mcp_indicator_color(McpStatusBarInfo {
            connected: 1,
            failed: 1,
            total: 2,
        });
        assert_eq!(fg, Color::Yellow, "partial → yellow");
    }

    #[test]
    fn mcp_color_red_when_all_failed() {
        let fg = mcp_indicator_color(McpStatusBarInfo {
            connected: 0,
            failed: 2,
            total: 2,
        });
        assert_eq!(fg, Color::Red, "all failed → red");
    }

    // ── #1194/#1195 follow-up: segment ordering ──────────────────────
    // The cwd segment was removed (banner already shows it; can't
    // change in a session). Mode now leads the bar because it changes
    // most often (Shift+Tab cycles trust modes) and has a distinct
    // color, so eye-anchoring it leftmost matches user attention.

    #[test]
    fn mode_renders_leftmost_before_model() {
        let bar = StatusBar::new("gpt-4", "auto", 50);
        let text = render_bar(bar, 200);
        // #1232 §8a: labels are now UPPERCASE for visibility.
        let mode_pos = text.find("AUTO").expect("mode label should render");
        let model_pos = text.find("gpt-4").expect("model should render");
        assert!(
            mode_pos < model_pos,
            "mode ({mode_pos}) must come before model ({model_pos}) in: {text}"
        );
    }

    // (#1158's bg-pill tests removed in #1210 along with the pill
    // itself; the live bg-activity overlay is covered by
    // `widgets::child_activity_overlay::tests` and `child_activity::tests`.)

    /// Vim segment is hidden when the textarea reports `None`
    /// (vim editing disabled). PR 3 of #1178.
    #[test]
    fn vim_segment_hidden_when_label_is_none() {
        let bar = StatusBar::new("gpt-4", "safe", 50).with_vim_label(None);
        let out = render_bar(bar, 200);
        assert!(
            !out.contains("VIM"),
            "vim segment must not render when label is None: {out}"
        );
    }

    /// Vim segment renders the label when set. Covers the
    /// `with_vim_label(Some(...))` happy path used by `tui_viewport`
    /// when the textarea is in a vim mode.
    #[test]
    fn vim_segment_renders_label_when_some() {
        let bar = StatusBar::new("gpt-4", "safe", 50).with_vim_label(Some("NORMAL"));
        let out = render_bar(bar, 200);
        assert!(
            out.contains("VIM:NORMAL"),
            "vim pill must render label: {out}"
        );
    }

    // ── #1232 §8a: loud trust-mode badge ─────────────────────────────
    //
    // The bug-review session that opened #1232 reported that the user
    // couldn't tell whether they were in Safe or Auto. The fix makes
    // the trust-mode segment unmissable: icon + UPPERCASE + bold for
    // every mode. (#1243 originally rendered Auto as an inverted
    // black-on-green badge for extra loudness, but the hardcoded
    // green bg clashed with terminal color schemes that use bright
    // green and made "AUTO" unreadable; reverted to bold green text
    // matching Plan/Safe styling. The icon + uppercase + bold trio
    // still makes Auto distinguishable at a glance.)
    //
    // Tests below pin the contract so a future "let's use a single
    // muted color for all status segments" refactor fails loudly.

    /// Find the cell index of the first occurrence of `needle` in the
    /// rendered buffer, returning `(x, fg, bg, modifier)`. Lets tests
    /// assert on the styling of a SPECIFIC character without having
    /// to scan every cell or guess offsets.
    fn cell_style_at(bar: StatusBar<'_>, needle: &str, width: u16) -> (Color, Color, Modifier) {
        let area = Rect::new(0, 0, width, 1);
        let mut buf = Buffer::empty(area);
        bar.render(area, &mut buf);
        let row: String = (0..width)
            .map(|x| buf.cell((x, 0)).map(|c| c.symbol()).unwrap_or(" "))
            .collect();
        let pos = row
            .find(needle)
            .unwrap_or_else(|| panic!("needle {needle:?} not found in: {row}"));
        // Convert byte offset back to cell column. ASCII-only labels
        // ("AUTO", "SAFE", "PLAN") so byte offset == column for the
        // labels we test here. The icon is BMP-or-above and lives at
        // a SEPARATE position; tests below only sniff the labels.
        let cell = buf.cell((pos as u16, 0)).expect("cell in bounds");
        (cell.fg, cell.bg, cell.modifier)
    }

    #[test]
    fn auto_badge_is_bold_green_text_no_background() {
        // Auto used to render as inverted black-on-green for extra
        // loudness, but a hardcoded Green background clashed with
        // terminal color schemes that already use bright/light green
        // — the black fg washed out and "AUTO" became unreadable.
        // Now matches Plan/Safe styling: bold colored text on the
        // user's own background, which works on every scheme. The
        // ICON (⚡), the UPPERCASE label, and the bold modifier still
        // make Auto distinguishable from Plan/Safe at a glance.
        //
        // If #1241 (flip default to Auto) needs more loudness, use
        // `Modifier::REVERSED` here — it inverts using the user's
        // own terminal colors instead of imposing fixed colors,
        // guaranteed-readable on any scheme.
        let bar = StatusBar::new("gpt-4", "auto", 50);
        let (fg, bg, modifier) = cell_style_at(bar, "AUTO", 200);
        assert_eq!(fg, Color::Green, "Auto badge fg must be green");
        assert_eq!(bg, Color::Reset, "Auto badge must have NO background fill");
        assert!(
            modifier.contains(Modifier::BOLD),
            "Auto badge must be bold; got modifier: {modifier:?}"
        );
    }

    #[test]
    fn safe_badge_is_bold_yellow_text_no_background() {
        // Safe is conservative-default — visible (bold yellow) but
        // matches Plan/Auto styling: bold colored text on the user's
        // own background, no fill.
        // Yellow matches the prompt indicator in `tui_viewport.rs`
        // (post-fix); pre-fix Safe was Cyan in the prompt, Yellow in
        // the bar — confusing inconsistency this test pins gone.
        let bar = StatusBar::new("gpt-4", "safe", 50);
        let (fg, bg, modifier) = cell_style_at(bar, "SAFE", 200);
        assert_eq!(fg, Color::Yellow, "Safe badge fg must be yellow");
        assert_eq!(bg, Color::Reset, "Safe badge must have NO background fill");
        assert!(
            modifier.contains(Modifier::BOLD),
            "Safe badge must be bold; got modifier: {modifier:?}"
        );
    }

    #[test]
    fn plan_badge_is_bold_cyan_text_no_background() {
        // Plan is read-only — calm but visible. Bold cyan, no bg.
        // Pre-fix Plan was DarkGray in the prompt indicator, which
        // actively HID the indicator on a dark terminal background.
        let bar = StatusBar::new("gpt-4", "plan", 50);
        let (fg, bg, modifier) = cell_style_at(bar, "PLAN", 200);
        assert_eq!(fg, Color::Cyan, "Plan badge fg must be cyan");
        assert_eq!(bg, Color::Reset, "Plan badge must have NO background fill");
        assert!(
            modifier.contains(Modifier::BOLD),
            "Plan badge must be bold; got modifier: {modifier:?}"
        );
    }

    #[test]
    fn mode_label_renders_uppercase_for_every_known_mode() {
        // Every supported mode label must render UPPERCASE. Pre-fix
        // they were lowercase (`safe` / `auto` / `plan`), which read
        // as ordinary status text instead of a foregrounded mode
        // indicator. The uppercasing is the cheap-but-loud half of
        // "don't let the user be unsure which mode they're in."
        for label in ["plan", "safe", "auto"] {
            let bar = StatusBar::new("gpt-4", label, 50);
            let text = render_bar(bar, 200);
            let upper = label.to_ascii_uppercase();
            assert!(
                text.contains(&upper),
                "label {label:?} must render uppercase {upper:?}, got: {text}"
            );
            // Negative: the lowercase form must NOT appear in the
            // visible text (defends against "oops, formatted twice").
            assert!(
                !text.contains(label),
                "lowercase {label:?} must NOT appear (got: {text})"
            );
        }
    }

    #[test]
    fn mode_segment_renders_icon_prefix() {
        // Icon prefix matches the prompt indicator in `tui_viewport.rs`
        // for visual continuity across both surfaces. If the icon goes
        // missing or changes character, that's a UX regression worth
        // catching.
        let cases = [
            ("plan", '\u{1f4cb}'), // 📋
            ("safe", '\u{1f512}'), // 🔒
            ("auto", '\u{26a1}'),  // ⚡
        ];
        for (label, expected_icon) in cases {
            let bar = StatusBar::new("gpt-4", label, 50);
            let text = render_bar(bar, 200);
            assert!(
                text.contains(expected_icon),
                "{label:?} must render icon {expected_icon:?}, got: {text}"
            );
        }
    }

    #[test]
    fn unknown_mode_renders_question_mark_badge_not_silent() {
        // The defensive `_` arm in the match must produce a VISIBLE
        // "?" badge so a future TrustMode variant added without
        // updating this match is obvious in the UI (instead of
        // rendering as nothing or as the previous mode's color).
        let bar = StatusBar::new("gpt-4", "future_unknown_mode", 50);
        let text = render_bar(bar, 200);
        assert!(
            text.contains("?"),
            "unknown mode label must render `?` placeholder, got: {text}"
        );
    }
}
