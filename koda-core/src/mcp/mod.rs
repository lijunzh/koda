//! MCP (Model Context Protocol) support.
//!
//! Connects Koda to external MCP servers, exposing their tools alongside
//! built-in tools. Uses the `rmcp` crate for the protocol implementation
//! and reads `.mcp.json` configs (same format as Claude Code / Cursor).
//!
//! Architecture:
//! - `config` — loads `.mcp.json` from project root and user config
//! - `client` — wraps `rmcp` to connect, list tools, and call tools
//! - `registry` — manages multiple MCP server connections

/// Built-in MCP tool registry for auto-provisioning.
pub mod capability_registry;
/// Single MCP server connection wrapper.
pub mod client;
/// `.mcp.json` configuration loading.
pub mod config;
/// Multi-server MCP registry — manages connections and routes tool calls.
pub mod registry;

/// Re-export the registry type for convenience.
pub use registry::McpRegistry;
