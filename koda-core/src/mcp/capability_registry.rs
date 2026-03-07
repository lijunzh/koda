//! MCP capability registry for auto-provisioned servers.
//!
//! Maps tool names to MCP server binaries that can be auto-started
//! when a tool call arrives for a tool that isn't built-in.

use crate::mcp::config::McpServerConfig;

/// An entry in the capability registry.
#[derive(Debug, Clone)]
pub struct CapabilityEntry {
    /// Server name (used as MCP namespace).
    pub server_name: &'static str,
    /// Binary command to start the server.
    pub command: &'static str,
    /// Tool names this server provides (without namespace prefix).
    pub tools: &'static [&'static str],
    /// Human-readable description.
    pub description: &'static str,
    /// Install command hint (shown if binary not found).
    pub install_hint: &'static str,
}

/// Built-in capability registry — auto-provisionable MCP servers.
const REGISTRY: &[CapabilityEntry] = &[CapabilityEntry {
    server_name: "koda-ast",
    command: "koda-ast",
    tools: &["AstAnalysis"],
    description: "Tree-sitter AST analysis for Rust, Python, JS, TS",
    install_hint: "cargo install koda-ast",
}];

/// Look up which MCP server provides a given tool name.
pub fn find_server_for_tool(tool_name: &str) -> Option<&'static CapabilityEntry> {
    REGISTRY
        .iter()
        .find(|entry| entry.tools.contains(&tool_name))
}

/// Check if a binary is available on PATH.
pub fn binary_exists(command: &str) -> bool {
    which::which(command).is_ok()
}

/// Create an McpServerConfig for a capability entry.
pub fn to_mcp_config(entry: &CapabilityEntry) -> McpServerConfig {
    McpServerConfig {
        command: entry.command.to_string(),
        args: Vec::new(),
        env: Default::default(),
        timeout: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_ast_analysis() {
        let entry = find_server_for_tool("AstAnalysis");
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().server_name, "koda-ast");
    }

    #[test]
    fn test_unknown_tool() {
        assert!(find_server_for_tool("UnknownTool123").is_none());
    }
}
