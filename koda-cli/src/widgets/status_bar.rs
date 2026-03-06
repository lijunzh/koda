//! Status bar widget for the inline TUI viewport.
//!
//! Shows: model name | approval mode | context usage bar

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
}

impl<'a> StatusBar<'a> {
    pub fn new(model: &'a str, mode_label: &'a str, context_pct: u32) -> Self {
        Self {
            model,
            mode_label,
            context_pct,
            queue_len: 0,
        }
    }

    pub fn with_queue(mut self, queue_len: usize) -> Self {
        self.queue_len = queue_len;
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

        let line = Line::from(vec![
            Span::styled(
                format!(" {} ", self.model),
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

        // Append queue indicator if any
        if self.queue_len > 0 {
            let mut spans = line.into_iter().collect::<Vec<_>>();
            spans.push(Span::styled(
                "\u{2502}",
                Style::default().fg(Color::Rgb(60, 60, 60)),
            ));
            spans.push(Span::styled(
                format!(" {} queued ", self.queue_len),
                Style::default().fg(Color::Yellow),
            ));
            Line::from(spans).render(area, buf);
        } else {
            line.render(area, buf);
        }
    }
}
