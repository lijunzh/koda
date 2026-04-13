//! Status bar widget for the inline TUI viewport.
//!
//! Shows: model name | approval mode | context usage bar | MCP | inference state

use koda_core::mcp::manager::McpStatusBarInfo;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
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

        let mut spans = vec![
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
        ];

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
}
