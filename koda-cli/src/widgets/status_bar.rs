//! Status bar widget for the inline TUI viewport.
//!
//! Shows: model name | approval mode | context usage bar | inference state

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Widget,
};

pub struct StatusBar<'a> {
    model: &'a str,
    tier_label: &'a str,
    mode_label: &'a str,
    context_pct: u32,
    queue_len: usize,
    /// Elapsed seconds during inference (0 = idle).
    elapsed_secs: u64,
    /// Last turn stats (shown after inference completes).
    last_turn: Option<&'a TurnStats>,
}

/// Stats from the most recent inference turn.
#[derive(Debug, Clone, Default)]
pub struct TurnStats {
    #[allow(dead_code)]
    pub tokens_in: i64,
    pub tokens_out: i64,
    #[allow(dead_code)]
    pub cache_read: i64,
    pub elapsed_ms: u64,
    pub rate: f64,
    /// Estimated cost in USD (None if model pricing unknown).
    pub cost_usd: Option<f64>,
}

impl<'a> StatusBar<'a> {
    pub fn new(model: &'a str, tier_label: &'a str, mode_label: &'a str, context_pct: u32) -> Self {
        Self {
            model,
            tier_label,
            mode_label,
            context_pct,
            queue_len: 0,
            elapsed_secs: 0,
            last_turn: None,
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
}

impl Widget for StatusBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let mode_color = match self.mode_label {
            "plan" => Color::Yellow,
            "yolo" => Color::Red,
            _ => Color::Cyan,
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

        let mut spans = vec![
            Span::styled(
                format!(" {} ", self.model),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                format!("[{}]", self.tier_label),
                Style::default().fg(Color::Rgb(100, 100, 100)),
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

        // Queue indicator
        if self.queue_len > 0 {
            spans.push(Span::styled(
                "\u{2502}",
                Style::default().fg(Color::Rgb(60, 60, 60)),
            ));
            spans.push(Span::styled(
                format!(" {} queued ", self.queue_len),
                Style::default().fg(Color::Yellow),
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

            let cost_str = match stats.cost_usd {
                Some(c) if c < 0.01 => " · <$0.01".to_string(),
                Some(c) => format!(" · ${c:.2}"),
                None => String::new(),
            };

            spans.push(Span::styled(
                format!(
                    " {} tok · {} · {:.0} t/s{} ",
                    stats.tokens_out, time, stats.rate, cost_str
                ),
                Style::default().fg(Color::DarkGray),
            ));
        }

        Line::from(spans).render(area, buf);
    }
}
