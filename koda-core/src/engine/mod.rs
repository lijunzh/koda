//! Engine module — the protocol boundary between Koda's core and any client.
//!
//! The engine communicates exclusively through `EngineEvent` (output) and
//! `EngineCommand` (input) enums. This decoupling allows the same engine
//! to power the CLI, a future ACP server, VS Code extension, or desktop app.
//!
//! ## Event flow
//!
//! ```text
//! Client (TUI/CLI)
//!   ── EngineCommand::UserMessage → Engine
//!   ── EngineCommand::ApproveAction → Engine
//!   ── EngineCommand::Cancel → Engine
//!
//! Engine
//!   ── EngineEvent::Token → Client  (streaming text)
//!   ── EngineEvent::ToolCall → Client  (tool invocation)
//!   ── EngineEvent::ToolResult → Client  (tool output)
//!   ── EngineEvent::NeedApproval → Client  (confirmation request)
//!   ── EngineEvent::Done → Client  (turn complete)
//! ```
//!
//! See `DESIGN.md` for the full architectural rationale.

/// Event and command enums — the engine's public protocol.
pub mod event;
/// Event sink trait — how clients receive engine events.
pub mod sink;

/// Re-export all event/command types at module level.
pub use event::*;
/// Re-export sink types at module level.
pub use sink::*;
