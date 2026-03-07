//! Headless sink — simple println rendering for headless mode.
//!
//! Auto-approves everything (headless always runs in yolo mode).
//! Streams text directly to stdout. No markdown rendering, no spinners,
//! no TUI widgets.

use koda_core::engine::{ApprovalDecision, EngineCommand, EngineEvent, EngineSink};
use std::io::Write;

pub struct HeadlessSink {
    cmd_tx: tokio::sync::mpsc::Sender<EngineCommand>,
}

impl HeadlessSink {
    pub fn new(cmd_tx: tokio::sync::mpsc::Sender<EngineCommand>) -> Self {
        Self { cmd_tx }
    }
}

impl EngineSink for HeadlessSink {
    fn emit(&self, event: EngineEvent) {
        match event {
            // ── Auto-approve (headless = yolo) ──────────────────
            EngineEvent::ApprovalRequest { id, .. } => {
                let _ = self.cmd_tx.blocking_send(EngineCommand::ApprovalResponse {
                    id,
                    decision: ApprovalDecision::Approve,
                });
            }
            EngineEvent::LoopCapReached { .. } => {
                let _ = self.cmd_tx.blocking_send(EngineCommand::LoopDecision {
                    action: koda_core::loop_guard::LoopContinuation::Continue200,
                });
            }

            // ── Streaming text ──────────────────────────────────
            EngineEvent::TextDelta { text } => {
                print!("{text}");
                let _ = std::io::stdout().flush();
            }
            EngineEvent::TextDone => {
                println!();
            }

            // ── Thinking ────────────────────────────────────────
            EngineEvent::ThinkingStart => {
                eprintln!("\x1b[90m  \u{1f4ad} thinking...\x1b[0m");
            }
            EngineEvent::ThinkingDelta { .. } => {}
            EngineEvent::ThinkingDone => {}

            // ── Tool calls ──────────────────────────────────────
            EngineEvent::ToolCallStart { name, .. } => {
                eprintln!("\x1b[36m  \u{26a1} {name}\x1b[0m");
            }
            EngineEvent::ToolCallResult { name, output, .. } => {
                let summary = truncate(&output, 200);
                eprintln!("\x1b[32m  \u{2713} {name}\x1b[0m: {summary}");
            }

            // ── Sub-agents ──────────────────────────────────────
            EngineEvent::SubAgentStart { agent_name } => {
                eprintln!("\x1b[35m  \u{1f916} {agent_name}\x1b[0m");
            }

            // ── Blocked actions ──────────────────────────────────
            EngineEvent::ActionBlocked {
                detail, preview, ..
            } => {
                eprintln!("\x1b[33m  \u{1f4cb} Would execute: {detail}\x1b[0m");
                if let Some(ref p) = preview {
                    let rendered = crate::diff_render::render(p);
                    for line in rendered.lines() {
                        eprintln!("  {line}");
                    }
                }
            }

            // ── Info/Warn/Error ──────────────────────────────────
            EngineEvent::Info { message } => eprintln!("\x1b[36m  {message}\x1b[0m"),
            EngineEvent::Warn { message } => eprintln!("\x1b[33m  \u{26a0} {message}\x1b[0m"),
            EngineEvent::Error { message } => eprintln!("\x1b[31m  \u{2717} {message}\x1b[0m"),

            // ── Ignored in headless ─────────────────────────────
            EngineEvent::ResponseStart => {}
            EngineEvent::SpinnerStart { .. } => {}
            EngineEvent::SpinnerStop => {}
            EngineEvent::StatusUpdate { .. } => {}
            EngineEvent::TurnStart { .. } => {}
            EngineEvent::TurnEnd { .. } => {}
            EngineEvent::Footer {
                completion_tokens,
                total_chars,
                elapsed_ms,
                rate,
                ..
            } => {
                let tokens = if completion_tokens > 0 {
                    completion_tokens
                } else {
                    (total_chars / 4) as i64
                };
                let secs = elapsed_ms as f64 / 1000.0;
                eprintln!(
                    "\x1b[90m  {tokens} tokens \u{00b7} {secs:.1}s \u{00b7} {rate:.0} t/s\x1b[0m"
                );
            }
        }
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        // Find a safe char boundary
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_ascii() {
        assert_eq!(truncate("hello world", 5), "hello");
        assert_eq!(truncate("hi", 10), "hi");
    }

    #[test]
    fn test_truncate_unicode() {
        // '🐶' is 4 bytes — truncating at 2 should give empty
        let s = "🐶hello";
        let t = truncate(s, 2);
        assert!(t.len() <= 2);
    }
}
