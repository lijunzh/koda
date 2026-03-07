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
const REGISTRY: &[CapabilityEntry] = &[
    CapabilityEntry {
        server_name: "koda-ast",
        command: "koda-ast",
        tools: &["AstAnalysis"],
        description: "Tree-sitter AST analysis for Rust, Python, JS, TS",
        install_hint: "brew install koda (includes koda-ast) or cargo install koda-ast",
        tool_definitions: &[(
            "AstAnalysis",
            "Read-only AST code analysis. Supports .rs, .py, .js, .ts. \
             Use action 'analyze_file' for structure summary or 'get_call_graph' with a symbol.",
            r#"{"type":"object","properties":{"action":{"type":"string","description":"'analyze_file' or 'get_call_graph'"},"file_path":{"type":"string","description":"Path to file"},"symbol":{"type":"string","description":"Symbol for get_call_graph"}},"required":["action","file_path"]}"#,
        )],
    },
    CapabilityEntry {
        server_name: "koda-email",
        command: "koda-email",
        tools: &["EmailRead", "EmailSend", "EmailSearch"],
        description: "Email read/send/search via IMAP/SMTP",
        install_hint: "brew install koda (includes koda-email) or cargo install koda-email",
        tool_definitions: &[
            (
                "EmailRead",
                "Read recent emails from INBOX. Returns subject, sender, date, and a text snippet. \
                 Use 'count' to control how many (default 5, max 20).",
                r#"{"type":"object","properties":{"count":{"type":"integer","description":"Number of recent emails to fetch (default 5, max 20)"}},"required":[]}"#,
            ),
            (
                "EmailSend",
                "Send an email via SMTP. Requires 'to' (recipient), 'subject', and 'body'.",
                r#"{"type":"object","properties":{"to":{"type":"string","description":"Recipient email address"},"subject":{"type":"string","description":"Email subject line"},"body":{"type":"string","description":"Email body text"}},"required":["to","subject","body"]}"#,
            ),
            (
                "EmailSearch",
                "Search emails in INBOX. Plain text searches subject and body. \
                 Use 'from:addr' to search by sender, 'subject:text' to search by subject.",
                r#"{"type":"object","properties":{"query":{"type":"string","description":"Search query. Use 'from:' or 'subject:' prefixes for targeted search."},"max_results":{"type":"integer","description":"Max results (default 10, max 50)"}},"required":["query"]}"#,
            ),
        ],
    },
];

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
    fn test_find_ast_analysis() {
        let entry = find_server_for_tool("AstAnalysis");
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().server_name, "koda-ast");
    }

    #[test]
    fn test_unknown_tool() {
        assert!(find_server_for_tool("UnknownTool123").is_none());
    }

    #[test]
    fn test_tool_definitions_include_ast() {
        let defs = tool_definitions();
        assert!(!defs.is_empty());
        let ast = defs.iter().find(|d| d.name == "AstAnalysis");
        assert!(ast.is_some(), "AstAnalysis should be in tool definitions");
    }

    #[test]
    fn test_find_email_tools() {
        for tool_name in &["EmailRead", "EmailSend", "EmailSearch"] {
            let entry = find_server_for_tool(tool_name);
            assert!(entry.is_some(), "{tool_name} should be in registry");
            assert_eq!(entry.unwrap().server_name, "koda-email");
        }
    }

    #[test]
    fn test_tool_definitions_include_email() {
        let defs = tool_definitions();
        for name in &["EmailRead", "EmailSend", "EmailSearch"] {
            let found = defs.iter().find(|d| d.name == *name);
            assert!(found.is_some(), "{name} should be in tool definitions");
        }
    }

    #[tokio::test]
    async fn test_auto_provision_returns_install_hint() {
        // When koda-ast is not on PATH and MCP registry is None,
        // executing AstAnalysis should return an install hint, not "Unknown tool"
        let registry = crate::tools::ToolRegistry::new(std::path::PathBuf::from("/tmp/test"));
        let result = registry.execute("AstAnalysis", "{}").await;
        assert!(
            !result.output.contains("Unknown tool"),
            "Should not return 'Unknown tool', got: {}",
            result.output
        );
        assert!(
            result.output.contains("koda-ast"),
            "Should mention koda-ast server, got: {}",
            result.output
        );
    }
}
