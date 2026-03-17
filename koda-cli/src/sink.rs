//! CLI sink — forwards EngineEvents to the TUI event loop.
//!
//! The TUI uses `CliSink::channel()` to forward all events to the
//! main event loop via `UiEvent`. The headless path uses
//! `HeadlessSink` (see `headless_sink.rs`).

use koda_core::engine::{EngineEvent, EngineSink};

// ── UiEvent ───────────────────────────────────────────────

/// Events forwarded from `CliSink` to the main event loop.
pub(crate) enum UiEvent {
    Engine(EngineEvent),
}

// ── CliSink ───────────────────────────────────────────────

/// Channel-forwarding sink for the TUI event loop.
///
/// Uses an **unbounded** channel so engine events are never dropped.
///
/// Memory safety: the engine produces events sequentially (single
/// turn loop, I/O-bound on LLM streaming at ~50–100 tokens/sec).
/// The TUI drains events every frame (~16 ms). Even in the worst
/// case (large `ToolCallResult` output), only a handful of events
/// queue up — each a few KB — which is negligible. A bounded
/// channel with `try_send` silently dropped `TextDelta` events
/// when the TUI couldn’t keep up, truncating model output.
pub struct CliSink {
    ui_tx: tokio::sync::mpsc::UnboundedSender<UiEvent>,
}

impl CliSink {
    /// Create a channel-forwarding sink for the TUI event loop.
    pub fn channel(ui_tx: tokio::sync::mpsc::UnboundedSender<UiEvent>) -> Self {
        Self { ui_tx }
    }
}

impl EngineSink for CliSink {
    fn emit(&self, event: EngineEvent) {
        if let Err(e) = self.ui_tx.send(UiEvent::Engine(event)) {
            tracing::warn!("UI channel closed, event lost: {e}");
        }
    }
}
