//! Bridge between MCP tools and Koda's tool system.
//!
//! Handles tool name formatting (`server__tool`), parsing, and mapping
//! MCP tool annotations to Koda's `ToolEffect` classification.

use koda_command::ToolEffect;
use koda_providers::ToolDefinition;

/// Separator between server name and tool name in qualified MCP tool names.
///
/// Double underscore avoids ambiguity with single underscores in tool names.
/// Example: `playwright__navigate`, `github__create_issue`.
const MCP_SEPARATOR: &str = "__";

/// Format a qualified MCP tool name: `server__tool`.
pub fn format_mcp_tool_name(server: &str, tool: &str) -> String {
    format!("{server}{MCP_SEPARATOR}{tool}")
}

/// Parse a qualified MCP tool name back into `(server, tool)`.
///
/// Returns `None` if the name doesn't contain the separator.
pub fn parse_mcp_tool_name(name: &str) -> Option<(&str, &str)> {
    // Split on FIRST occurrence of `__` — tool names may contain `__` too.
    let idx = name.find(MCP_SEPARATOR)?;
    let server = &name[..idx];
    let tool = &name[idx + MCP_SEPARATOR.len()..];
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some((server, tool))
}

/// Check whether a tool name looks like a qualified MCP tool name.
pub fn is_mcp_tool_name(name: &str) -> bool {
    parse_mcp_tool_name(name).is_some()
}

/// Classify an MCP tool's effect using its annotations, as Codex decides
/// whether one asks (`requires_mcp_tool_approval`, `core/src/mcp_tool_call.rs`):
/// - `readOnlyHint: true` → `ReadOnly`: runs in both modes.
/// - `destructiveHint: true` → `Destructive`.
/// - `destructiveHint: false` and `openWorldHint: false` → `LocalMutation`:
///   a change the server says stays inside it, which Auto runs.
/// - Anything else, a missing hint included → `RemoteAction`, which asks
///   in Auto: the tool may reach anything.
pub fn classify_mcp_tool(annotations: Option<&McpToolAnnotations>) -> ToolEffect {
    let Some(ann) = annotations else {
        return ToolEffect::RemoteAction;
    };
    if ann.read_only_hint == Some(true) {
        return ToolEffect::ReadOnly;
    }
    if ann.destructive_hint == Some(true) {
        return ToolEffect::Destructive;
    }
    if ann.destructive_hint == Some(false) && ann.open_world_hint == Some(false) {
        return ToolEffect::LocalMutation;
    }
    ToolEffect::RemoteAction
}

/// Subset of MCP ToolAnnotations we care about for classification.
///
/// Extracted from `rmcp::model::ToolAnnotations` during tool discovery
/// so we don't need to hold rmcp types in the tool registry.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct McpToolAnnotations {
    /// If true, the tool does not modify state.
    pub read_only_hint: Option<bool>,
    /// If true, the tool may perform destructive actions.
    pub destructive_hint: Option<bool>,
    /// If true, the tool may reach outside its server (the web, other
    /// systems); false, it stays inside it.
    pub open_world_hint: Option<bool>,
}

/// Convert an rmcp Tool into a Koda ToolDefinition.
///
/// The tool name is qualified with the server name (`server__tool`).
/// The JSON Schema `properties` field is fixed up if missing (some MCP
/// servers omit it).
pub fn mcp_tool_to_definition(
    server_name: &str,
    tool: &rmcp::model::Tool,
) -> (ToolDefinition, McpToolAnnotations) {
    let qualified_name = format_mcp_tool_name(server_name, &tool.name);

    // Build the parameters JSON Schema.
    // rmcp gives us the input_schema as a serde_json::Map.
    let schema_value = serde_json::to_value(&tool.input_schema).unwrap_or_default();
    let mut params = if schema_value.is_object() {
        schema_value
    } else {
        serde_json::json!({"type": "object", "properties": {}})
    };

    // Fix up missing `properties` — some MCP servers omit it.
    if let Some(obj) = params.as_object_mut()
        && !obj.contains_key("properties")
    {
        obj.insert(
            "properties".to_string(),
            serde_json::Value::Object(serde_json::Map::new()),
        );
    }

    let description = tool
        .description
        .as_deref()
        .unwrap_or("MCP tool (no description)")
        .to_string();

    let def = ToolDefinition {
        name: qualified_name,
        description: format!("[MCP:{server_name}] {description}"),
        parameters: params,
    };

    // Extract annotations.
    let annotations = tool
        .annotations
        .as_ref()
        .map(|ann| McpToolAnnotations {
            read_only_hint: ann.read_only_hint,
            destructive_hint: ann.destructive_hint,
            open_world_hint: ann.open_world_hint,
        })
        .unwrap_or_default();

    (def, annotations)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_and_parse_roundtrip() {
        let name = format_mcp_tool_name("playwright", "navigate");
        assert_eq!(name, "playwright__navigate");
        let (server, tool) = parse_mcp_tool_name(&name).unwrap();
        assert_eq!(server, "playwright");
        assert_eq!(tool, "navigate");
    }

    #[test]
    fn parse_tool_with_underscores() {
        let name = format_mcp_tool_name("github", "create_pull_request");
        assert_eq!(name, "github__create_pull_request");
        let (server, tool) = parse_mcp_tool_name(&name).unwrap();
        assert_eq!(server, "github");
        assert_eq!(tool, "create_pull_request");
    }

    #[test]
    fn parse_rejects_no_separator() {
        assert!(parse_mcp_tool_name("plain_tool").is_none());
    }

    #[test]
    fn parse_rejects_empty_parts() {
        assert!(parse_mcp_tool_name("__tool").is_none());
        assert!(parse_mcp_tool_name("server__").is_none());
    }

    #[test]
    fn is_mcp_tool_name_works() {
        assert!(is_mcp_tool_name("playwright__navigate"));
        assert!(!is_mcp_tool_name("Read"));
        assert!(!is_mcp_tool_name("Bash"));
    }

    #[test]
    fn classify_no_annotations() {
        assert_eq!(classify_mcp_tool(None), ToolEffect::RemoteAction);
    }

    #[test]
    fn classify_read_only() {
        let ann = McpToolAnnotations {
            read_only_hint: Some(true),
            destructive_hint: None,
            open_world_hint: None,
        };
        assert_eq!(classify_mcp_tool(Some(&ann)), ToolEffect::ReadOnly);
    }

    #[test]
    fn classify_destructive() {
        let ann = McpToolAnnotations {
            read_only_hint: None,
            destructive_hint: Some(true),
            open_world_hint: None,
        };
        assert_eq!(classify_mcp_tool(Some(&ann)), ToolEffect::Destructive);
    }

    #[test]
    fn classify_read_only_beats_destructive() {
        // If both are set, read-only wins (safer interpretation).
        let ann = McpToolAnnotations {
            read_only_hint: Some(true),
            destructive_hint: Some(true),
            open_world_hint: None,
        };
        assert_eq!(classify_mcp_tool(Some(&ann)), ToolEffect::ReadOnly);
    }

    /// Codex runs a tool its server calls non-destructive and closed-world
    /// without asking; one that may reach anything asks.
    #[test]
    fn a_closed_world_change_runs_and_an_open_one_asks() {
        let ann = |destructive, open_world| McpToolAnnotations {
            read_only_hint: None,
            destructive_hint: destructive,
            open_world_hint: open_world,
        };
        assert_eq!(
            classify_mcp_tool(Some(&ann(Some(false), Some(false)))),
            ToolEffect::LocalMutation
        );
        assert_eq!(
            classify_mcp_tool(Some(&ann(Some(false), Some(true)))),
            ToolEffect::RemoteAction
        );
        assert_eq!(
            classify_mcp_tool(Some(&ann(Some(false), None))),
            ToolEffect::RemoteAction
        );
        assert_eq!(
            classify_mcp_tool(Some(&ann(None, Some(false)))),
            ToolEffect::RemoteAction
        );
    }

    #[test]
    fn classify_explicit_false() {
        let ann = McpToolAnnotations {
            read_only_hint: Some(false),
            destructive_hint: Some(false),
            open_world_hint: None,
        };
        assert_eq!(classify_mcp_tool(Some(&ann)), ToolEffect::RemoteAction);
    }
}
