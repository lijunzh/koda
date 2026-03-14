//! Koda Core — the engine library for the Koda AI coding agent.
//!
//! This crate contains the pure engine logic with zero terminal dependencies.
//! It communicates exclusively through `EngineEvent` (output) and
//! `EngineCommand` (input) enums.
//!
//! See `DESIGN.md` in the repository root for the full architectural rationale.

#![warn(missing_docs)]

/// Sub-agent configuration, discovery, and invocation.
pub mod agent;
/// Tool approval modes, safety gates, and shared mode state.
pub mod approval;
/// Heuristic path-escape detection for bash commands.
pub mod bash_path_lint;
/// Bash command safety classification (destructive, mutating, read-only).
pub mod bash_safety;
/// Context compaction — summarise old messages to reclaim token budget.
pub mod compact;
/// Global configuration: provider, model, model settings, CLI flags.
pub mod config;
/// Context window management and token budgeting.
pub mod context;
/// SQLite persistence layer — sessions, messages, usage tracking.
pub mod db;
/// Engine protocol: `EngineEvent` / `EngineCommand` enums.
pub mod engine;
/// Git helpers — status, diff, blame, log.
pub mod git;
/// The main inference loop — send messages, stream responses, dispatch tools.
pub mod inference;
/// Shared helpers used by the inference loop.
pub mod inference_helpers;
/// Credential storage (OS keychain via `keyring`).
pub mod keystore;
/// Guardrail against runaway tool-call loops.
pub mod loop_guard;
/// Project memory — `MEMORY.md` / `CLAUDE.md` read/write.
pub mod memory;
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
/// Environment variable access (mockable for tests).
pub mod runtime_env;
/// Session lifecycle — create, resume, list, delete.
pub mod session;
/// User settings persistence (`~/.config/koda/settings.json`).
pub mod settings;
/// Skill discovery and activation (project, user, built-in).
pub mod skills;
/// Cache for sub-agent provider/model config across invocations.
pub mod sub_agent_cache;
/// Tool dispatch — routes tool calls from inference to the registry.
pub mod tool_dispatch;
/// Tool registry, definitions, execution, and path safety.
pub mod tools;
/// Token-safe output truncation.
pub mod truncate;
/// Undo stack for file mutations.
pub mod undo;
/// Version string and update-check helpers.
pub mod version;
