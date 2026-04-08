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
//! | [`approval`] | Safety gates and approval modes |
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

#![warn(missing_docs)]

/// Sub-agent configuration, discovery, and invocation.
pub mod agent;
/// Tool approval modes, safety gates, and shared mode state.
pub mod approval;
/// Approval flow and user interaction during tool execution.
pub(crate) mod approval_flow;
/// Heuristic path-escape detection for bash commands.
pub mod bash_path_lint;
/// Bash command safety classification — ReadOnly, LocalMutation, Destructive.
pub mod bash_safety;
/// Background sub-agent registry — tracks and drains async agent results.
pub mod bg_agent;
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
/// Loop detection — catches runaway repeated tool calls.
pub mod loop_guard;
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
/// Progress reporting helpers for long operations.
pub mod progress;
/// System prompt construction.
pub mod prompt;
/// LLM provider abstraction — Anthropic, Gemini, OpenAI-compatible.
pub mod providers;
/// Thread-safe runtime environment — replaces `std::env::set_var`.
pub mod runtime_env;
/// Session lifecycle — create, resume, list, delete.
pub mod session;
/// Last-used provider persistence (`~/.config/koda/settings.toml`).
pub mod settings;
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
/// Undo stack for file mutations.
pub mod undo;
/// Version string and update-check helpers.
pub mod version;
/// Git worktree provisioning for sub-agent isolation.
pub mod worktree;
