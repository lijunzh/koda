//! Engine module: the protocol boundary between Koda's core and any client.
//!
//! The engine communicates exclusively through [`EngineEvent`] (output) and
//! [`EngineCommand`] (input) enums. This decoupling allows the same engine
//! to power the CLI, a future ACP server, VS Code extension, or desktop app.
//!
//! See `DESIGN.md` for the full architectural rationale.

// These types are the foundation for #40 (EngineSink) and #38 (server mode).
// They aren't consumed yet, so suppress dead_code until wired in.
#[allow(dead_code)]
pub mod event;
#[allow(dead_code)]
pub mod sink;

#[allow(unused_imports)]
pub use event::*;
#[allow(unused_imports)]
pub use sink::*;
