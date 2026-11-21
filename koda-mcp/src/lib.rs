//! MCP client — connect to external MCP servers (#662).
//!
//! Split out of `koda-core` in 0.7.1, which re-exports this crate as
//! `koda_core::mcp`. This crate follows koda-cli's version, and its Rust API
//! makes no semver promise of its own: it is published for
//! `cargo install koda-cli`, not as a library to build on.
//!
//! Koda can act as an MCP **client**, discovering and invoking tools exposed
//! by third-party MCP servers (Playwright, databases, Slack, etc.).
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────┐     stdio/http      ┌──────────────────┐
//! │  McpManager      │ ◄────────────────► │  MCP Server      │
//! │  (owns clients)  │                    │  (e.g. Playwright)│
//! └────────┬────────┘                    └──────────────────┘
//!          │
//!    ToolRegistry
//!    (server__tool naming)
//! ```
//!
//! ## Config storage
//!
//! MCP server configs are stored globally in the SQLite `kv_store` table
//! under the `mcp:` key prefix. Managed via `/mcp add`, `/mcp add-http`,
//! `/mcp remove`, and `/mcp list` slash commands.
//!
//! Supports both stdio (child process) and HTTP (Streamable HTTP) transports.
//!
//! ## Design decisions (#662)
//!
//! - **Global scope (user-level)** — not per-project. Accepted trade-off.
//! - **Tool annotations → ToolEffect** — `destructiveHint`/`readOnlyHint`
//!   map directly to the TrustMode approval matrix.
//! - **Resources** — not yet implemented.

/// MCP server configuration types and database persistence.
pub mod config;

/// Single MCP server client — wraps rmcp connection lifecycle.
pub mod client;
#[cfg(test)]
mod compat_tests;

/// Multi-server connection manager — parallel startup, tool discovery.
pub mod elicitation;
pub mod manager;
pub mod oauth;
pub mod schema;

/// Bridge MCP tools into Koda's ToolRegistry (naming, classification).
pub mod tool_bridge;

pub use config::McpServerConfig;
pub use manager::McpManager;
pub use tool_bridge::{
    classify_mcp_tool, format_mcp_tool_name, is_mcp_tool_name, parse_mcp_tool_name,
};
