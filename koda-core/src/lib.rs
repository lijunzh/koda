//! Koda Core — the engine library for the Koda AI coding agent.
//!
//! This crate contains the pure engine logic with zero terminal dependencies.
//! It communicates exclusively through [`engine::EngineEvent`] (output) and
//! [`engine::EngineCommand`] (input) enums.
//!
//! ## Quick navigation
//!
//! | Module | What it does |
//! |---|---|
//! | [`agent`] | Agent construction and built-in agent catalog |
//! | [`tools`] | The built-in tools (files, shell, search, web, memory, agents) |
//! | [`providers`] | `koda-providers`, re-exported: Anthropic, Gemini and OpenAI-compatible clients |
//! | [`inference`] | Streaming inference loop with tool execution |
//! | [`config`] | Configuration loading, agent discovery, settings |
//! | [`trust`] | Safety gates and approval modes |
//! | [`engine`] | Event/command protocol (client–engine boundary) |
//! | [`compact`] | Context compaction (summarize old messages) |
//! | [`memory`] | Instruction files (`AGENTS.md`, `CLAUDE.md`) and Koda's notes |
//! | [`skills`] | Expertise modules (prompt injection, zero LLM cost) |
//!
//! Sessions are stored by `koda-state` (`koda_state::db`, behind the
//! `koda_state::session_store::SessionStore` trait), as Codex keeps its
//! storage outside `codex-core`.
//!
//! ## Architecture
//!
//! Clients don't use this crate directly: the TUI, `koda exec` and
//! `koda serve` reach the engine through the client API (`koda-client`),
//! whose host (`koda-server`) drives it with [`engine::EngineCommand`]s and
//! turns its [`engine::EngineEvent`]s into the client protocol.
//!
//! ```text
//! ┌──────────────┐  client   ┌─────────────┐  EngineCommand  ┌─────────────┐
//! │   koda-cli   │ protocol  │ koda-client │ ──────────────→ │  koda-core  │
//! │ (TUI, exec,  │ ←───────→ │ koda-server │ ←────────────── │  (engine)   │
//! │   serve)     │           │   (host)    │   EngineEvent   └──────┬──────┘
//! └──────────────┘           └─────────────┘                        │
//!                                    koda-providers · koda-sandbox · koda-state
//! ```
//!
//! See `DESIGN.md` in the repository root for the full architectural rationale.
//!
//! **Versioning:** published so that `cargo install koda-cli` builds. It
//! follows koda-cli's version, and its Rust API makes no semver promise of
//! its own: a 0.7.x release may change it. The Koda crates pin each other
//! exactly, so every koda-cli builds against the crates it shipped with.

// koda-core requires a Unix-like OS (macOS or Linux).
// The built-in shell tool uses `sh` which does not exist on Windows.
// Windows users: install WSL2 — https://learn.microsoft.com/windows/wsl
#![cfg_attr(
    not(unix),
    compile_error(
        "koda-core requires a Unix-like operating system (macOS or Linux). \
         Windows is not supported. On Windows, use WSL2 instead: \
         https://learn.microsoft.com/windows/wsl"
    )
)]
#![warn(missing_docs)]

/// Sub-agent configuration, discovery, and invocation.
pub mod agent;
/// Approval flow and user interaction during tool execution.
pub mod approval_flow;
/// A turn's token budget (`turn_token_budget`).
pub mod budget;
/// Registry of the sub-agents running inside the current turn, for live status.
pub mod child_agent;
/// Context compaction — summarise old messages to reclaim token budget.
pub mod compact;
/// Global configuration: provider, model, model settings, CLI flags.
pub mod config;
/// Context window management and token budgeting (engine-internal).
pub(crate) mod context;
/// Context analysis — per-tool token breakdown and duplicate detection.
pub mod context_analysis;
/// Engine protocol: `EngineEvent` / `EngineCommand` enums.
pub mod engine;
/// File lifecycle tracker — tracks files created by Koda per session (#465).
/// Git context injection — branch, staged/unstaged diffs, recent commits.
pub mod git;
pub mod goal;
pub(crate) mod goal_judge;
/// The main inference loop — send messages, stream responses, dispatch tools.
pub mod inference;
/// Shared helpers used by the inference loop.
pub mod inference_helpers;
/// Credential storage — SQLite KV store with env var fallback.
pub mod keystore;
/// Last-used provider recall (stored in SQLite KV store, not a config file).
pub mod last_provider;
/// Diagnostic logs in their own SQLite database, read by `koda-logs`.
pub use koda_state::log_db;
/// Loop detection — catches runaway repeated tool calls.
pub mod loop_guard;
/// Shell command classification and path linting: `koda-command`'s, under
/// their old paths.
pub use koda_command::{bash_path_lint, bash_safety};
/// The MCP client: `koda-mcp`, under its old path.
pub use koda_mcp as mcp;
/// Instruction files (`AGENTS.md`, `CLAUDE.md`) and Koda's own notes: the
/// `koda-memory` crate, under its old path.
pub use koda_memory as memory;
/// Context-scaled output caps for tool results.
pub mod output_caps;
pub mod output_schema;
/// Diff preview generation for file mutations.
pub mod preview;
/// System prompt construction.
pub mod prompt;
/// Thread-safe runtime environment — replaces `std::env::set_var`.
pub use koda_config::runtime_env;
/// LLM provider abstraction — Anthropic, Gemini, OpenAI-compatible: the
/// `koda-providers` crate, under its old path.
pub use koda_providers as providers;
/// The model tables, from `koda-providers`, under their old paths.
pub use koda_providers::{model_alias, model_context, model_effort, provider_catalog};
/// Process sandboxing for the Bash tool (macOS seatbelt / Linux bwrap).
pub mod sandbox;
/// Session lifecycle — create, resume, list, delete.
pub mod session;
/// Skill-scoped tool filtering — hard enforcement of `allowed_tools`.
pub mod skill_scope;
/// Skill discovery and activation (project, user, built-in): the
/// `koda-skills` crate, under its old path.
pub use koda_skills as skills;
/// Sub-agent result caching — zero-cost retries after compaction.
pub mod sub_agent_cache;
/// Sub-agent invocation and lifecycle management.
pub(crate) mod sub_agent_dispatch;
/// Tool dispatch — routes tool calls from inference to the registry.
pub mod tool_dispatch;
/// Tool name normalization — maps model-emitted variants to canonical PascalCase.
pub mod tool_normalize;
/// Tool registry, definitions, execution, and path safety.
pub mod tools;
/// Unified trust mode — single permission knob replacing ApprovalMode × SandboxMode.
pub mod trust;
/// Per-turn ambient context (`TurnContext`, `ToolExecutionContext`)
/// — see #1265 item 4.
pub mod turn_context;
/// Content the agent reads but the user didn't write: `koda-providers`'s,
/// under its old path.
pub use koda_providers::untrusted;
