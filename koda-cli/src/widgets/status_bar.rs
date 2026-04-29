//! Status bar widget for the inline TUI viewport.
//!
//! Shows: cwd | model name | approval mode | context usage bar | MCP | inference state

use koda_core::mcp::manager::McpStatusBarInfo;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Widget,
};
use std::path::Path;

/// Soft target for the cwd segment width. Anything longer is
/// progressively shortened by [`format_cwd_compact`]. Chosen so
/// even on an 80-col terminal the cwd consumes <¼ of the bar,
/// leaving room for model name + mode + context%.
const CWD_MAX_LEN: usize = 24;

pub struct StatusBar<'a> {
    /// Session working directory (typically `project_root`). Rendered
    /// as the leftmost segment when present — mirrors shell-prompt
    /// convention so users always know where commands will land
    /// (#1105). `None` skips the segment for callers that don't have
    /// a cwd context (e.g. unit-test fixtures).
    cwd: Option<&'a Path>,
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
            cwd: None,
            model,
            mode_label,
            context_pct,
            queue_len: 0,
            elapsed_secs: 0,
            last_turn: None,
            scroll_info: None,
            mcp_info: None,
        }
    }

    /// Attach a working-directory hint. Rendered as the leftmost
    /// segment, formatted via [`format_cwd_compact`] (HOME-relative
    /// and left-truncated to fit). Builder rather than required ctor
    /// arg so existing call sites keep compiling and tests can omit
    /// the dependency on a real filesystem path.
    pub fn with_cwd(mut self, cwd: &'a Path) -> Self {
        self.cwd = Some(cwd);
        self
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
}

/// Compact display form for a working-directory path.
///
/// Behaviour, in order:
/// 1. If `path` is under `$HOME`, replace the prefix with `~` (shell
///    convention; gives `~/repo/koda` rather than `/Users/.../koda`).
/// 2. If the result fits in `max_len` chars (counted in `char`s, not
///    bytes — multi-byte CJK paths wouldn't otherwise be respected),
///    it's returned as-is.
/// 3. Otherwise truncate from the LEFT with a leading `…/`, keeping
///    the rightmost path segments visible (the part you actually
///    care about for orientation).
/// 4. Hard fallback: even the last segment alone exceeds the budget
///    — return `…/<truncated-last-segment>` so the bar never grows
///    unboundedly. The terminal column count, not aesthetics, is the
///    invariant we protect.
///
/// Pure function for unit-test ergonomics; takes `&str` for $HOME so
/// tests can inject any value without touching the real environment.
fn format_cwd_compact(path: &Path, home: Option<&str>, max_len: usize) -> String {
    let raw = path.to_string_lossy();
    let homed: String = match home {
        Some(h) if !h.is_empty() && raw.starts_with(h) => {
            let rest = &raw[h.len()..];
            if rest.is_empty() {
                "~".to_string()
            } else if rest.starts_with('/') {
                format!("~{rest}")
            } else {
                // $HOME without trailing slash but path has more chars
                // not separated by `/` — don't munge, fall back to raw.
                raw.into_owned()
            }
        }
        _ => raw.into_owned(),
    };

    if homed.chars().count() <= max_len {
        return homed;
    }

    // Step 3: left-truncate with "…/" prefix, keep rightmost segments.
    // Reserve 2 chars for the "…/" prefix.
    let budget = max_len.saturating_sub(2);
    let chars: Vec<char> = homed.chars().collect();
    let suffix: String = chars.iter().rev().take(budget).rev().collect();
    // Try to start the suffix at a `/` boundary so we don't show a
    // half-eaten segment like `koda/widget` → `dgets/status_bar.rs`.
    let aligned = match suffix.find('/') {
        Some(idx) if idx + 1 < suffix.len() => &suffix[idx + 1..],
        _ => suffix.as_str(),
    };
    format!("…/{aligned}")
}

impl Widget for StatusBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let mode_color = match self.mode_label {
            "auto" => Color::Green,
            "strict" => Color::Cyan,
            "safe" => Color::Yellow,
            _ => Color::DarkGray,
        };

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

        // CWD segment (#1105) — leftmost, mirrors shell-prompt
        // convention. Hidden when no cwd was attached.
        if let Some(cwd) = self.cwd {
            // Resolve $HOME at render time. We re-read it each render
            // because the alternative — caching at construction —
            // would be a hidden state-bag for tests; the call is a
            // single env lookup with no allocation cost worth caring
            // about at TUI render frequency.
            let home = std::env::var("HOME").ok();
            let cwd_display = format_cwd_compact(cwd, home.as_deref(), CWD_MAX_LEN);
            spans.push(Span::styled(
                format!(" {cwd_display} "),
                Style::default().fg(Color::Rgb(140, 140, 140)),
            ));
            spans.push(Span::styled(
                "\u{2502}",
                Style::default().fg(Color::Rgb(60, 60, 60)),
            ));
        }

        spans.extend([
            Span::styled(
                format!(" {model_display} "),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled("\u{2502}", Style::default().fg(Color::Rgb(60, 60, 60))),
            Span::styled(
                format!(" {} ", self.mode_label),
                Style::default().fg(mode_color),
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

    // ── #1105: cwd display ALPHA ────────────────────────────
    // The helper is pure: tests inject `home` directly so they don't
    // depend on the test runner's actual $HOME (CI vs laptop drift).

    #[test]
    fn format_cwd_short_path_passes_through_with_home_substitution() {
        let path = Path::new("/Users/lijun/repo/koda");
        let out = format_cwd_compact(path, Some("/Users/lijun"), 32);
        assert_eq!(out, "~/repo/koda");
    }

    #[test]
    fn format_cwd_no_home_match_keeps_absolute_path() {
        let path = Path::new("/srv/koda");
        let out = format_cwd_compact(path, Some("/Users/lijun"), 32);
        assert_eq!(out, "/srv/koda");
    }

    #[test]
    fn format_cwd_no_home_set_keeps_absolute_path() {
        let path = Path::new("/srv/koda");
        let out = format_cwd_compact(path, None, 32);
        assert_eq!(out, "/srv/koda");
    }

    #[test]
    fn format_cwd_home_root_renders_as_tilde() {
        let path = Path::new("/Users/lijun");
        let out = format_cwd_compact(path, Some("/Users/lijun"), 32);
        assert_eq!(out, "~");
    }

    #[test]
    fn format_cwd_long_path_truncates_from_left_at_segment_boundary() {
        let path = Path::new("/Users/lijun/repo/koda/koda-cli/src/widgets/status_bar.rs");
        let out = format_cwd_compact(path, Some("/Users/lijun"), 24);
        // Length budget honoured.
        assert!(
            out.chars().count() <= 24,
            "output `{out}` exceeds budget (len {})",
            out.chars().count()
        );
        // Starts with the truncation marker.
        assert!(out.starts_with("…/"), "missing …/ prefix: {out}");
        // Should preserve the most recent segment so users still see
        // "where they are" — status_bar.rs is the file in cwd.
        assert!(out.contains("status_bar.rs"), "last segment dropped: {out}");
    }

    #[test]
    fn format_cwd_truncation_does_not_split_segment_mid_name() {
        // Construct a path where naive char-wise left-trim would produce
        // "…/dgets/status_bar.rs" (mid-segment cut). The aligned trim
        // should re-snap to the next `/` boundary.
        let path = Path::new("/Users/lijun/repo/koda/widgets/status_bar.rs");
        let out = format_cwd_compact(path, Some("/Users/lijun"), 22);
        assert!(
            !out.contains("dgets"),
            "truncation cut a segment mid-name: {out}"
        );
    }

    #[test]
    fn cwd_segment_appears_in_rendered_bar_when_set() {
        // Use SAFE::current_dir-free path; with_cwd takes any &Path.
        let p = Path::new("/tmp/short");
        let bar = StatusBar::new("gpt-4", "safe", 50).with_cwd(p);
        let text = render_bar(bar, 200);
        assert!(text.contains("/tmp/short"), "cwd missing from bar: {text}");
    }

    #[test]
    fn cwd_segment_hidden_when_not_set() {
        // Default StatusBar::new has no cwd — segment must be absent.
        let bar = StatusBar::new("gpt-4", "safe", 50);
        let text = render_bar(bar, 120);
        // The model name `gpt-4` should appear within the first few
        // visible chars (after the leading space) — i.e. nothing was
        // prepended ahead of it.
        assert!(
            text.trim_start().starts_with("gpt-4"),
            "unexpected leading content before model: `{text}`"
        );
    }

    #[test]
    fn cwd_segment_renders_leftmost_before_model() {
        let p = Path::new("/tmp/koda");
        let bar = StatusBar::new("gpt-4", "safe", 50).with_cwd(p);
        let text = render_bar(bar, 200);
        let cwd_pos = text.find("/tmp/koda").expect("cwd should render: {text}");
        let model_pos = text.find("gpt-4").expect("model should render");
        assert!(
            cwd_pos < model_pos,
            "cwd ({cwd_pos}) must come before model ({model_pos}) in: {text}"
        );
    }
}
