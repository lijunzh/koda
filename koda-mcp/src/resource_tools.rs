//! The model's tools for MCP resources, as Codex's `list_mcp_resources`,
//! `list_mcp_resource_templates` and `read_mcp_resource`: offered whenever a
//! session has an MCP server. Before these, only the user could bring a
//! resource in, with `@server:uri`.
//!
//! What a resource holds is the server's, not the user's: its text comes
//! back marked as external content, as MCP tool results are.

use koda_providers::ToolDefinition;
use serde_json::{Value, json};

use crate::client::ResourceContent;
use crate::manager::McpManager;

pub const LIST_RESOURCES: &str = "ListMcpResources";
pub const LIST_TEMPLATES: &str = "ListMcpResourceTemplates";
pub const READ_RESOURCE: &str = "ReadMcpResource";

/// The most of a resource's text one read returns, as a mention's.
const MAX_CHARS: usize = 100_000;

/// Whether `name` is one of these tools.
pub fn is_resource_tool(name: &str) -> bool {
    matches!(name, LIST_RESOURCES | LIST_TEMPLATES | READ_RESOURCE)
}

/// The three tools' definitions.
pub fn definitions() -> Vec<ToolDefinition> {
    let server = |what: &str| {
        json!({
            "type": "string",
            "description": format!("MCP server name. Omit to list {what} from every connected server."),
        })
    };
    vec![
        ToolDefinition {
            name: LIST_RESOURCES.into(),
            description: "List the resources MCP servers offer: files, records, schemas and other \
                data a server shares as context. Read one with ReadMcpResource. Prefer a resource \
                to a web search when a server has it."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": { "server": server("resources") },
                "additionalProperties": false,
            }),
        },
        ToolDefinition {
            name: LIST_TEMPLATES.into(),
            description: "List the resource templates MCP servers offer: URI templates that take \
                parameters. Fill one in and read it with ReadMcpResource."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": { "server": server("resource templates") },
                "additionalProperties": false,
            }),
        },
        ToolDefinition {
            name: READ_RESOURCE.into(),
            description: "Read one resource from an MCP server, by the server's name and the \
                resource's URI as ListMcpResources gives them (or a filled-in template)."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "server": {"type": "string", "description": "The MCP server's name, as ListMcpResources gives it."},
                    "uri": {"type": "string", "description": "The resource's URI."},
                },
                "required": ["server", "uri"],
                "additionalProperties": false,
            }),
        },
    ]
}

/// A tool's result: text for the model, images it read, and whether it
/// worked.
pub struct ResourceToolResult {
    pub output: String,
    pub images: Vec<(String, String)>,
    pub success: bool,
}

fn failed(output: String) -> ResourceToolResult {
    ResourceToolResult {
        output,
        images: Vec::new(),
        success: false,
    }
}

/// Run resource tool `name` with `args` against `manager`.
pub async fn execute(manager: &McpManager, name: &str, args: &Value) -> ResourceToolResult {
    let server = args["server"].as_str().filter(|s| !s.is_empty());
    let servers: Vec<String> = match server {
        Some(server) if !manager.is_connected(server) => {
            return failed(format!("Error: {}", manager.why_not_connected(server)));
        }
        Some(server) => vec![server.to_string()],
        None => manager.connected_servers(),
    };
    match name {
        LIST_RESOURCES => {
            let listed: Vec<Value> = servers
                .iter()
                .flat_map(|server| {
                    manager.resources_of(server).into_iter().map(move |r| {
                        json!({
                            "server": server,
                            "uri": r.uri,
                            "name": r.name,
                            "title": r.title,
                            "description": r.description,
                            "mimeType": r.mime_type,
                        })
                    })
                })
                .collect();
            ok(&json!({ "resources": listed }))
        }
        LIST_TEMPLATES => {
            let mut listed = Vec::new();
            for server in &servers {
                match manager.resource_templates_of(server).await {
                    Ok(templates) => listed.extend(templates.into_iter().map(|t| {
                        json!({
                            "server": server,
                            "uriTemplate": t.uri_template,
                            "name": t.name,
                            "description": t.description,
                            "mimeType": t.mime_type,
                        })
                    })),
                    Err(e) => return failed(format!("Error: {server}: {e:#}")),
                }
            }
            ok(&json!({ "resourceTemplates": listed }))
        }
        READ_RESOURCE => {
            let (Some(server), Some(uri)) = (server, args["uri"].as_str()) else {
                return failed("Error: ReadMcpResource needs `server` and `uri`.".into());
            };
            match manager.read_resource(server, uri).await {
                Ok(contents) => read_result(server, uri, contents),
                Err(e) => failed(format!("Error: {e:#}")),
            }
        }
        other => failed(format!("Error: unknown resource tool {other}")),
    }
}

fn ok(value: &Value) -> ResourceToolResult {
    ResourceToolResult {
        output: serde_json::to_string_pretty(value).unwrap_or_default(),
        images: Vec::new(),
        success: true,
    }
}

/// A read's text, marked as the server's, with images as images.
fn read_result(server: &str, uri: &str, contents: Vec<ResourceContent>) -> ResourceToolResult {
    let mut text = String::new();
    let mut images = Vec::new();
    for content in contents {
        match content {
            ResourceContent::Text { text: t, .. } => {
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(&t);
            }
            ResourceContent::Blob {
                mime_type: Some(mime),
                blob,
            } if mime.starts_with("image/") => images.push((mime, blob)),
            ResourceContent::Blob { mime_type, blob } => {
                text.push_str(&format!(
                    "\n[binary content, {} bytes base64, {}]",
                    blob.len(),
                    mime_type.as_deref().unwrap_or("unknown type")
                ));
            }
        }
    }
    if text.chars().count() > MAX_CHARS {
        text = text.chars().take(MAX_CHARS).collect();
        text.push_str(&format!("\n[cut at {MAX_CHARS} characters]"));
    }
    if text.is_empty() && !images.is_empty() {
        text = format!("{} image(s).", images.len());
    }
    let label = format!("{server}:{uri}");
    ResourceToolResult {
        output: koda_providers::untrusted::wrap(&label, &text),
        images,
        success: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_is_marked_as_the_servers_and_keeps_images_apart() {
        let result = read_result(
            "notes",
            "note://welcome",
            vec![
                ResourceContent::Text {
                    mime_type: Some("text/markdown".into()),
                    text: "Welcome.".into(),
                },
                ResourceContent::Blob {
                    mime_type: Some("image/png".into()),
                    blob: "AA==".into(),
                },
            ],
        );
        assert!(result.success);
        assert!(result.output.starts_with(koda_providers::untrusted::NOTE));
        assert!(result.output.contains("Welcome."));
        assert_eq!(result.images, [("image/png".into(), "AA==".into())]);
    }

    #[test]
    fn the_definitions_name_the_three_tools() {
        let names: Vec<String> = definitions().into_iter().map(|d| d.name).collect();
        assert_eq!(names, [LIST_RESOURCES, LIST_TEMPLATES, READ_RESOURCE]);
        assert!(names.iter().all(|n| is_resource_tool(n)));
    }
}
