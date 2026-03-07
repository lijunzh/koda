//! Koda Core — the engine library for the Koda AI coding agent.
//!
//! This crate contains the pure engine logic with zero terminal dependencies.
//! It communicates exclusively through [`engine::EngineEvent`] (output) and
//! [`engine::EngineCommand`] (input) enums.
//!
//! See `DESIGN.md` in the repository root for the full architectural rationale.

pub mod agent;
pub mod approval;
pub mod bash_safety;
pub mod compact;
pub mod config;
pub mod context;
pub mod db;
pub mod engine;
pub mod inference;
pub mod inference_helpers;
pub mod keystore;
pub mod loop_guard;
pub mod mcp;
pub mod memory;
pub mod model_context;
pub mod model_tier;
pub mod preview;
pub mod prompt;
pub mod providers;
pub mod runtime_env;
pub mod session;
pub mod skills;
pub mod tool_dispatch;
pub mod tools;
pub mod undo;
pub mod version;
