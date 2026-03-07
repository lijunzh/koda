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

pub mod capability_registry;
pub mod client;
pub mod config;
pub mod registry;

pub use registry::McpRegistry;
