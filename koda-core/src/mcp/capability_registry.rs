//! MCP capability registry for auto-provisioned servers.
//!
//! Maps tool names to MCP server binaries that can be auto-started
//! when a tool call arrives for a tool that isn't built-in.

use crate::mcp::config::McpServerConfig;
use crate::providers::ToolDefinition;

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
    /// Tool definition for the LLM (so it knows to call the tool).
    pub tool_definitions: &'static [(&'static str, &'static str, &'static str)], // (name, description, params_json)
}

/// Built-in capability registry — auto-provisionable MCP servers.
///
/// Note: koda-ast and koda-email are now first-party library calls
/// (see `ToolRegistry::execute` match arms). Only third-party / external
/// MCP servers belong here.
const REGISTRY: &[CapabilityEntry] = &[];

/// Get tool definitions from all capability registry entries.
///
/// These are injected into the LLM's tool list so it knows auto-provisionable
/// tools exist, even before the MCP server is connected.
pub fn tool_definitions() -> Vec<ToolDefinition> {
    REGISTRY
        .iter()
        .flat_map(|entry| {
            entry
                .tool_definitions
                .iter()
                .map(|(name, desc, params)| ToolDefinition {
                    name: name.to_string(),
                    description: desc.to_string(),
                    parameters: serde_json::from_str(params).unwrap_or_default(),
                })
        })
        .collect()
}

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
    fn test_registry_is_empty_after_library_migration() {
        // koda-ast and koda-email are now direct library calls,
        // so the MCP capability registry should be empty.
        assert!(REGISTRY.is_empty());
        assert!(tool_definitions().is_empty());
    }

    #[test]
    fn test_unknown_tool() {
        assert!(find_server_for_tool("UnknownTool123").is_none());
    }

    #[test]
    fn test_ast_no_longer_in_mcp_registry() {
        assert!(find_server_for_tool("AstAnalysis").is_none());
    }

    #[test]
    fn test_email_no_longer_in_mcp_registry() {
        for name in &["EmailRead", "EmailSend", "EmailSearch"] {
            assert!(find_server_for_tool(name).is_none());
        }
    }
}
