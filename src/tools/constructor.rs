//! UniversalConstructor: dynamic tool creation at runtime.
//!
//! Allows the LLM to define new tools by providing a name, description,
//! parameter schema, and a shell command template. The created tools
//! are persisted to `agents/tools/` as JSON and loaded on next session.
//!
//! This is the "meta-tool" — a tool that creates tools.

use super::safe_resolve_path;
use crate::providers::ToolDefinition;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;

const TOOLS_DIR: &str = "agents/tools";

/// A user-defined tool persisted to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomTool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub command_template: String,
}

/// Return tool definitions for the LLM.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "CreateTool".to_string(),
            description: "Create a reusable tool with a shell command template. \
                Use {{param}} placeholders in the template."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "PascalCase tool name (e.g. 'DockerBuild', 'GitLog')"
                    },
                    "description": {
                        "type": "string",
                        "description": "What the tool does (shown to the LLM)"
                    },
                    "parameters": {
                        "type": "object",
                        "description": "JSON Schema for the tool's parameters"
                    },
                    "command_template": {
                        "type": "string",
                        "description": "Shell command with {{param}} placeholders (e.g. 'git log --oneline -{{count}}')"
                    }
                },
                "required": ["name", "description", "parameters", "command_template"]
            }),
        },
        ToolDefinition {
            name: "ListTools".to_string(),
            description: "List all custom tools created via CreateTool.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDefinition {
            name: "DeleteTool".to_string(),
            description: "Delete a custom tool by name.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Name of the custom tool to delete"
                    }
                },
                "required": ["name"]
            }),
        },
    ]
}

/// Create a new custom tool and persist it.
pub async fn create_tool(project_root: &Path, args: &Value) -> Result<String> {
    let name = args["name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'name'"))?;
    let description = args["description"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'description'"))?;
    let parameters = args
        .get("parameters")
        .ok_or_else(|| anyhow::anyhow!("Missing 'parameters'"))?;
    let command_template = args["command_template"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'command_template'"))?;

    // Validate name is PascalCase-ish (alphanumeric only, starts with uppercase)
    if !is_safe_tool_name(name) {
        anyhow::bail!("Tool name must be PascalCase, alphanumeric only (e.g. 'GitLog')");
    }

    // Prevent overriding built-in tools
    let builtins = [
        "Read",
        "Write",
        "Edit",
        "Delete",
        "List",
        "Glob",
        "Grep",
        "Bash",
        "WebFetch",
        "TodoRead",
        "TodoWrite",
        "InvokeAgent",
        "ListAgents",
        "CreateTool",
        "ListTools",
        "DeleteTool",
    ];
    if builtins.contains(&name) {
        anyhow::bail!("Cannot override built-in tool: {name}");
    }

    let tool = CustomTool {
        name: name.to_string(),
        description: description.to_string(),
        parameters: parameters.clone(),
        command_template: command_template.to_string(),
    };

    let tools_dir = safe_resolve_path(project_root, TOOLS_DIR)?;
    tokio::fs::create_dir_all(&tools_dir).await?;

    let file_path = tools_dir.join(format!("{name}.json"));
    let json = serde_json::to_string_pretty(&tool)?;
    tokio::fs::write(&file_path, json).await?;

    Ok(format!(
        "Created custom tool '{name}'. \
         It will be available immediately and in future sessions.\n\
         Template: {command_template}"
    ))
}

/// List all custom tools.
pub async fn list_custom_tools(project_root: &Path) -> Result<String> {
    let tools_dir = project_root.join(TOOLS_DIR);
    if !tools_dir.exists() {
        return Ok("No custom tools yet. Use UniversalConstructor to create one.".to_string());
    }

    let mut entries = tokio::fs::read_dir(&tools_dir).await?;
    let mut tools = Vec::new();

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "json")
            && let Ok(content) = tokio::fs::read_to_string(&path).await
            && let Ok(tool) = serde_json::from_str::<CustomTool>(&content)
        {
            tools.push(format!("- {}: {}", tool.name, tool.description));
        }
    }

    if tools.is_empty() {
        Ok("No custom tools defined.".to_string())
    } else {
        Ok(format!("Custom tools:\n{}", tools.join("\n")))
    }
}

/// Delete a custom tool.
pub async fn delete_custom_tool(project_root: &Path, args: &Value) -> Result<String> {
    let name = args["name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'name'"))?;

    // Validate name to prevent path traversal (e.g. "../../.koda")
    if !is_safe_tool_name(name) {
        anyhow::bail!("Invalid tool name: must be alphanumeric PascalCase");
    }

    let file_path = project_root.join(TOOLS_DIR).join(format!("{name}.json"));
    if !file_path.exists() {
        anyhow::bail!("Custom tool '{name}' not found.");
    }

    tokio::fs::remove_file(&file_path).await?;
    Ok(format!("Deleted custom tool '{name}'."))
}

/// Execute a custom tool by name.
pub async fn execute_custom_tool(
    project_root: &Path,
    tool_name: &str,
    args: &Value,
) -> Result<String> {
    // Validate tool name to prevent path traversal
    if !is_safe_tool_name(tool_name) {
        anyhow::bail!("Invalid tool name: must be alphanumeric PascalCase");
    }

    let file_path = project_root
        .join(TOOLS_DIR)
        .join(format!("{tool_name}.json"));

    if !file_path.exists() {
        anyhow::bail!("Custom tool '{tool_name}' not found.");
    }

    let content = tokio::fs::read_to_string(&file_path).await?;
    let tool: CustomTool = serde_json::from_str(&content)?;

    // Expand {{param}} placeholders in the command template.
    // All values are shell-escaped to prevent command injection.
    let mut command = tool.command_template.clone();
    if let Some(obj) = args.as_object() {
        for (key, value) in obj {
            let placeholder = format!("{{{{{key}}}}}");
            let raw_value = match value {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            // Shell-escape to prevent injection (e.g. "; rm -rf /")
            let escaped = shell_escape(&raw_value);
            command = command.replace(&placeholder, &escaped);
        }
    }

    // Execute via the shell runner
    let shell_args = json!({ "command": command });
    super::shell::run_shell_command(project_root, &shell_args).await
}

/// Load custom tools as ToolDefinitions for the LLM.
pub fn load_custom_tool_definitions(project_root: &Path) -> Vec<ToolDefinition> {
    let tools_dir = project_root.join(TOOLS_DIR);
    let Ok(entries) = std::fs::read_dir(&tools_dir) else {
        return Vec::new();
    };

    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension()?.to_str()? != "json" {
                return None;
            }
            let content = std::fs::read_to_string(&path).ok()?;
            let tool: CustomTool = serde_json::from_str(&content).ok()?;
            Some(ToolDefinition {
                name: tool.name,
                description: tool.description,
                parameters: tool.parameters,
            })
        })
        .collect()
}

/// Shell-escape a string to prevent command injection.
/// Wraps the value in single quotes and escapes any embedded single quotes.
fn shell_escape(s: &str) -> String {
    // Single-quoting prevents all shell interpretation except for single quotes.
    // We handle embedded single quotes by ending the quote, adding an escaped
    // single quote, then re-opening the quote: 'can'''t' → can't
    format!("'{}'", s.replace("'", "'\''"))
}

/// Check if a tool name corresponds to a custom (user-defined) tool.
pub fn is_custom_tool(project_root: &Path, tool_name: &str) -> bool {
    if !is_safe_tool_name(tool_name) {
        return false;
    }
    let file_path = project_root
        .join(TOOLS_DIR)
        .join(format!("{tool_name}.json"));
    file_path.exists()
}

/// Validate that a tool name contains only safe characters (alphanumeric + underscore).
fn is_safe_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().next().unwrap().is_uppercase()
        && !name.contains(' ')
        && name.chars().all(|c| c.is_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_create_and_list_tool() {
        let tmp = TempDir::new().unwrap();

        let args = json!({
            "name": "GitLog",
            "description": "Show recent git commits",
            "parameters": {
                "type": "object",
                "properties": {
                    "count": { "type": "integer", "description": "Number of commits" }
                }
            },
            "command_template": "git log --oneline -{{count}}"
        });

        let result = create_tool(tmp.path(), &args).await.unwrap();
        assert!(result.contains("GitLog"));

        let list = list_custom_tools(tmp.path()).await.unwrap();
        assert!(list.contains("GitLog"));
        assert!(list.contains("Show recent git commits"));
    }

    #[tokio::test]
    async fn test_create_tool_prevents_builtin_override() {
        let tmp = TempDir::new().unwrap();
        let args = json!({
            "name": "Read",
            "description": "Override built-in",
            "parameters": {},
            "command_template": "cat file"
        });
        let result = create_tool(tmp.path(), &args).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_create_tool_validates_name() {
        let tmp = TempDir::new().unwrap();
        let args = json!({
            "name": "bad name",
            "description": "Invalid",
            "parameters": {},
            "command_template": "echo hi"
        });
        let result = create_tool(tmp.path(), &args).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_delete_custom_tool() {
        let tmp = TempDir::new().unwrap();

        let args = json!({
            "name": "Temp",
            "description": "Temporary",
            "parameters": {},
            "command_template": "echo temp"
        });
        create_tool(tmp.path(), &args).await.unwrap();

        let del_args = json!({ "name": "Temp" });
        let result = delete_custom_tool(tmp.path(), &del_args).await.unwrap();
        assert!(result.contains("Deleted"));

        let list = list_custom_tools(tmp.path()).await.unwrap();
        assert!(!list.contains("Temp"));
    }

    #[test]
    fn test_shell_escape_prevents_injection() {
        let escaped = shell_escape("10; rm -rf /");
        assert_eq!(escaped, "'10; rm -rf /'");
        // Should not be interpreted as multiple commands
    }

    #[test]
    fn test_shell_escape_handles_single_quotes() {
        let escaped = shell_escape("it's dangerous");
        assert_eq!(escaped, "'it'\''s dangerous'");
    }

    #[test]
    fn test_shell_escape_subshell() {
        let escaped = shell_escape("$(whoami)");
        assert_eq!(escaped, "'$(whoami)'");
    }

    #[test]
    fn test_is_safe_tool_name() {
        assert!(is_safe_tool_name("GitLog"));
        assert!(is_safe_tool_name("Docker_Build"));
        assert!(!is_safe_tool_name("../../etc"));
        assert!(!is_safe_tool_name("bad name"));
        assert!(!is_safe_tool_name("lowercase"));
        assert!(!is_safe_tool_name(""));
    }

    #[test]
    fn test_load_definitions() {
        let tmp = TempDir::new().unwrap();
        let tools_dir = tmp.path().join(TOOLS_DIR);
        std::fs::create_dir_all(&tools_dir).unwrap();

        let tool = CustomTool {
            name: "MyTool".to_string(),
            description: "Does stuff".to_string(),
            parameters: json!({ "type": "object", "properties": {} }),
            command_template: "echo hello".to_string(),
        };
        let json = serde_json::to_string(&tool).unwrap();
        std::fs::write(tools_dir.join("MyTool.json"), json).unwrap();

        let defs = load_custom_tool_definitions(tmp.path());
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "MyTool");
    }
}
