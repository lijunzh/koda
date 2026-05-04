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
//! | [`tools`] | 19 built-in tools (file ops, shell, search, web, memory, agents) |
//! | [`providers`] | 14 LLM providers (Anthropic, OpenAI, Gemini, local, etc.) |
//! | [`inference`] | Streaming inference loop with tool execution |
//! | [`config`] | Configuration loading, agent discovery, settings |
//! | [`trust`] | Safety gates and approval modes |
//! | [`engine`] | Event/command protocol (client–engine boundary) |
//! | [`compact`] | Context compaction (summarize old messages) |
//! | [`memory`] | Persistent semantic memory (`MEMORY.md`) |
//! | [`skills`] | Expertise modules (prompt injection, zero LLM cost) |
//! | [`db`] | SQLite persistence (sessions, history, file ownership) |
//!
//! ## Architecture
//!
//! ```text
//! ┌───────────────┐  EngineEvent   ┌─────────────────┐
//! │   koda-cli    │ ←───────────── │    koda-core    │
//! │  (TUI/REPL)   │ ─────────────→ │   (engine lib)  │
//! └───────────────┘  EngineCommand └────────┬────────┘
//!                                           │
//!                                    ┌──────┴──────┐
//!                                    │  providers  │
//!                                    │ (LLM APIs)  │
//!                                    └─────────────┘
//! ```
//!
//! See `DESIGN.md` in the repository root for the full architectural rationale.

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
pub(crate) mod approval_flow;
/// Heuristic path-escape detection for bash commands.
pub mod bash_path_lint;
/// Bash command safety classification — ReadOnly, LocalMutation, Destructive.
pub mod bash_safety;
/// Background sub-agent registry — tracks and drains async agent results.
pub mod child_agent;
/// Context compaction — summarise old messages to reclaim token budget.
pub mod compact;
/// Global configuration: provider, model, model settings, CLI flags.
pub mod config;
/// Context window management and token budgeting (engine-internal).
pub(crate) mod context;
/// Context analysis — per-tool token breakdown and duplicate detection.
pub mod context_analysis;
/// SQLite persistence layer — sessions, messages, usage tracking.
pub mod db;
/// Engine protocol: `EngineEvent` / `EngineCommand` enums.
pub mod engine;
/// File lifecycle tracker — tracks files created by Koda per session (#465).
pub mod file_tracker;
/// Git context injection — branch, staged/unstaged diffs, recent commits.
pub mod git;
/// The main inference loop — send messages, stream responses, dispatch tools.
pub mod inference;
/// Shared helpers used by the inference loop.
pub mod inference_helpers;
/// Credential storage — `keys.toml` with env var fallback.
pub mod keystore;
/// Last-used provider recall (stored in SQLite KV store, not a config file).
pub mod last_provider;
/// Loop detection — catches runaway repeated tool calls.
pub mod loop_guard;
/// MCP client — connect to external MCP servers (#662).
pub mod mcp;
/// Project memory — `MEMORY.md` / `CLAUDE.md` read/write.
pub mod memory;
/// Microcompact — lightweight tool result aging without full compaction.
pub mod microcompact;
/// Hardcoded model aliases for stable, cross-provider model selection.
pub mod model_alias;
/// Hardcoded context-window lookup table (fallback when API doesn't report).
pub mod model_context;
/// Context-scaled output caps for tool results.
pub mod output_caps;
/// `Persistence` trait — the database contract.
pub mod persistence;
/// Diff preview generation for file mutations.
pub mod preview;
/// System prompt construction.
pub mod prompt;
/// Static provider catalog: `ProviderType` + `ProviderMeta` lookup tables.
pub mod provider_catalog;
/// LLM provider abstraction — Anthropic, Gemini, OpenAI-compatible.
pub mod providers;
/// Thread-safe runtime environment — replaces `std::env::set_var`.
pub mod runtime_env;
/// Process sandboxing for the Bash tool (macOS seatbelt / Linux bwrap).
pub mod sandbox;
/// Session lifecycle — create, resume, list, delete.
pub mod session;
/// Skill-scoped tool filtering — hard enforcement of `allowed_tools`.
pub mod skill_scope;
/// Skill discovery and activation (project, user, built-in).
pub mod skills;
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
/// Token-safe output truncation.
pub mod truncate;
/// Unified trust mode — single permission knob replacing ApprovalMode × SandboxMode.
pub mod trust;
/// Undo stack for file mutations.
pub mod undo;
/// Version string and update-check helpers.
pub mod version;
