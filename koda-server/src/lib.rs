//! Koda's host: the server end of the client protocol. It owns the
//! database, builds each session's agent, runs turns and answers the
//! requests a client sends, as Codex's `app-server` does.
//!
//! [`in_process`] runs it in the caller's process, the way
//! `koda_client::InProcessClient` (the TUI, `koda exec`) uses it;
//! [`stdio::serve`] runs it for a client on the other end of a pipe,
//! `koda serve --stdio`, as `codex app-server` serves stdio.
//!
//! [`events::EventProjector`] turns a session's engine events into the
//! protocol's notifications; `koda exec --json` prints them.
//!
//! **Versioning:** published so that `cargo install koda-cli` builds. It
//! follows koda-cli's version, and its Rust API makes no semver promise of
//! its own: a 0.7.x release may change it. The Koda crates pin each other
//! exactly, so every koda-cli builds against the crates it shipped with.

mod background;
pub mod builtin_skills;
mod config_file;
pub mod events;
/// `feedback/save`: a report ZIP, as Codex writes one.
pub mod feedback;
mod history;
mod host;
pub mod in_process;
mod mcp;
mod memory;
mod session;
mod settings;
mod skills;
#[cfg(unix)]
pub mod socket;
pub mod stdio;

use koda_state::db::Database;

pub use koda_config::ConfigFile;
pub use koda_protocol as protocol;

/// The input history Koda kept in its database before the TUI kept its own
/// file (`history.jsonl`, as Codex's TUI does), oldest first; empty when
/// there is none or it can't be read. A front end reads it once, to bring
/// it over. `home` is Koda's folder, as [`in_process::HostStart::home`].
pub async fn earlier_input_history(home: &std::path::Path) -> Vec<String> {
    match Database::init(home).await {
        Ok(db) => db.history_load().await.unwrap_or_default(),
        Err(e) => {
            tracing::warn!("couldn't read the earlier input history: {e:#}");
            Vec::new()
        }
    }
}
