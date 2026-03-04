//! Engine module: the protocol boundary between Koda's core and any client.
//!
//! The engine communicates exclusively through [`EngineEvent`] (output) and
//! [`EngineCommand`] (input) enums. This decoupling allows the same engine
//! to power the CLI, a future ACP server, VS Code extension, or desktop app.
//!
//! See `DESIGN.md` for the full architectural rationale.

pub mod event;
pub mod sink;

pub use event::*;
pub use sink::*;
