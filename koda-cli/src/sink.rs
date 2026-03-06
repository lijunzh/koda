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
pub struct CliSink {
    ui_tx: tokio::sync::mpsc::Sender<UiEvent>,
}

impl CliSink {
    /// Create a channel-forwarding sink for the TUI event loop.
    pub fn channel(ui_tx: tokio::sync::mpsc::Sender<UiEvent>) -> Self {
        Self { ui_tx }
    }
}

impl EngineSink for CliSink {
    fn emit(&self, event: EngineEvent) {
        let _ = self.ui_tx.try_send(UiEvent::Engine(event));
    }
}
