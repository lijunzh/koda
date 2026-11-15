//! Koda's stored state, apart from its engine, as Codex keeps its own in
//! `codex-state`: how Koda opens its SQLite files ([`sqlite`]) and the
//! diagnostic log database ([`log_db`]). A front end that keeps its own logs
//! (the TUI, `koda-logs`) uses this crate without `koda-core`.
//!
//! The session store still lives in `koda-core`, and moves here next.

#![warn(missing_docs)]

pub mod log_db;
pub mod sqlite;
