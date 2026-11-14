//! The engine events the TUI renders.
//!
//! Koda's host runs the turns and hands the TUI each engine event as it is
//! (`koda_client::StartArgs::raw_events`), until the TUI renders the
//! protocol's items instead.

use koda_core::engine::EngineEvent;

/// An engine event on its way to the renderer.
pub enum UiEvent {
    Engine(EngineEvent),
}
