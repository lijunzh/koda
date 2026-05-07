//! Sub-agent system: shared agent resources + peer-messaging substrate.
//!
//! ## Existing
//!
//! - [`koda_agent::KodaAgent`] — shared, immutable per-session agent state
//!   (tools, system prompt, project root). Re-exported at this module
//!   level for backward compatibility (`koda_core::agent::KodaAgent`).
//!
//! ## New (Phase 1 of #1325) — vendored from openai/codex (Apache-2.0)
//!
//! See top-level [`NOTICE`](../../../NOTICE) file for attribution.
//!
//! - [`path`] — typed `AgentPath` (`/root/researcher/worker`).
//! - [`inter_agent`] — `InterAgentCommunication` wire format.
//! - [`mailbox`] — per-agent inbox: `mpsc` + `watch::Sender<u64>`
//!   sequence-numbered wakeups.
//!
//! Future phases (#1325):
//! - Phase 2 — wire `Mailbox` into `KodaSession`; drain at turn start
//!   and inject mail as user-role input.
//! - Phase 3 — `spawn_agent` / `wait_agent` / `send_message` /
//!   `list_agents` peer tools built on this substrate.
//! - Phase 4 — make `InvokeAgent` an internal convenience that
//!   spawn-and-waits.
//! - Phase 5 — delete `WaitTask` / `ListBackgroundTasks`.

pub mod inter_agent;
pub mod koda_agent;
pub mod mailbox;
pub mod path;

pub use inter_agent::InterAgentCommunication;
pub use koda_agent::KodaAgent;
pub use mailbox::{Mailbox, MailboxReceiver};
pub use path::AgentPath;
