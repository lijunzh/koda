//! Sub-agent system: shared agent resources.
//!
//! - `KodaAgent` — shared, immutable per-session agent state
//!   (tools, system prompt, project root). Re-exported at this module
//!   level for backward compatibility (`koda_core::agent::KodaAgent`).
//!
//! The peer-messaging substrate (a per-agent mailbox, an `AgentPath`
//! spawn-tree identity and the `SendMessage` / `WaitForMail` tools)
//! was removed once sub-agents became synchronous (#1366): nothing
//! could send mail but an agent to itself.

pub mod koda_agent;

pub use koda_agent::KodaAgent;
