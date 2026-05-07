//! Sub-agent system: shared agent resources + peer-messaging substrate.
//!
//! ## Existing
//!
//! - `KodaAgent` — shared, immutable per-session agent state
//!   (tools, system prompt, project root). Re-exported at this module
//!   level for backward compatibility (`koda_core::agent::KodaAgent`).
//!
//! ## New (Phase 1 of #1325) — vendored from openai/codex (Apache-2.0)
//!
//! See the top-level `NOTICE` file at the repo root for attribution.
//!
//! - `path` module — typed `AgentPath` (`/root/researcher/worker`).
//! - `inter_agent` module — `InterAgentCommunication` wire format.
//! - `mailbox` module — per-agent inbox: `Mailbox` + `MailboxReceiver`,
//!   an `mpsc` + `watch::Sender<u64>` pair with sequence-numbered wakeups.
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
pub mod mail_message;
pub mod mailbox;
pub mod mailbox_registry;
pub mod path;

pub use inter_agent::InterAgentCommunication;
pub use koda_agent::KodaAgent;
pub use mail_message::mail_to_user_message;
pub use mailbox::{Mailbox, MailboxReceiver};
pub use mailbox_registry::{MailboxRegistry, RegisterOutcome};
pub use path::AgentPath;
