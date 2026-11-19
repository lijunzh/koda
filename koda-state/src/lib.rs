//! Koda's stored state, apart from its engine, as Codex keeps its storage
//! outside `codex-core` (`codex-state`, `codex-rollout`,
//! `codex-thread-store`):
//!
//! - [`session_store`]: the [`session_store::SessionStore`] trait, how
//!   sessions are stored (their messages, turns, events and metadata), as
//!   Codex's `ThreadStore`, with its types;
//! - [`db`]: [`db::Database`], the trait's SQLite implementation, which
//!   also keeps API keys, settings earlier versions stored and the old
//!   input history;
//! - [`sqlite`]: how Koda opens its SQLite files;
//! - [`log_db`]: the diagnostic log database.
//!
//! It depends on no other Koda crate. The engine (`koda-core`) and the
//! host (`koda-server`) use it; a front end that keeps its own logs (the
//! TUI, `koda-logs`) uses it without `koda-core`.
//!
//! **Versioning:** published so that `cargo install koda-cli` builds. It
//! follows koda-cli's version, and its Rust API makes no semver promise of
//! its own: a 0.7.x release may change it. The Koda crates pin each other
//! exactly, so every koda-cli builds against the crates it shipped with.

#![warn(missing_docs)]

pub mod busy_retry;
pub mod db;
pub mod log_db;
pub mod session_store;
pub mod sqlite;
