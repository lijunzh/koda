//! Tool registry and execution engine.
//!
//! Each tool is a function that takes JSON arguments and returns a string result.
//! Path validation is enforced here to prevent directory traversal.

/// Normalize a tool name to PascalCase.
///
/// Small models sometimes send lowercase names (`bash`, `read`, `list`)
/// instead of PascalCase (`Bash`, `Read`, `List`). This maps common
/// variants to the canonical name. See issue #49.
pub fn normalize_tool_name(name: &str) -> String {
    match name.to_lowercase().as_str() {
        "bash" | "shell" => "Bash".to_string(),
        "read" => "Read".to_string(),
        "write" => "Write".to_string(),
        "edit" => "Edit".to_string(),
        "delete" => "Delete".to_string(),
        "list" => "List".to_string(),
        "grep" | "search" => "Grep".to_string(),
        "glob" => "Glob".to_string(),
        "webfetch" | "web_fetch" | "fetch" => "WebFetch".to_string(),
        "memoryread" | "memory_read" => "MemoryRead".to_string(),
        "memorywrite" | "memory_write" => "MemoryWrite".to_string(),
        "todowrite" | "todo_write" | "todo" => "TodoWrite".to_string(),
        "todoread" | "todo_read" => "TodoRead".to_string(),
        "listagents" | "list_agents" => "ListAgents".to_string(),
        "createagent" | "create_agent" => "CreateAgent".to_string(),
        "invokeagent" | "invoke_agent" => "InvokeAgent".to_string(),
        "astanalysis" | "ast_analysis" => "AstAnalysis".to_string(),
        _ => name.to_string(), // pass through unknown names (e.g., MCP tools)
    }
}

pub mod agent;
pub mod ast;
pub mod file_tools;
pub mod glob_tool;
pub mod grep;
pub mod memory;
pub mod shell;
pub mod todo;
pub mod web_fetch;

use anyhow::Result;
use path_clean::PathClean;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::providers::ToolDefinition;

/// Result of executing a tool.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub output: String,
}

/// The tool registry: maps tool names to their definitions and handlers.
pub struct ToolRegistry {
    project_root: PathBuf,
    definitions: HashMap<String, ToolDefinition>,
    read_cache: std::sync::Mutex<HashMap<String, (u64, SystemTime)>>,
    /// Connected MCP servers providing additional tools.
    mcp_registry: Option<std::sync::Arc<tokio::sync::RwLock<crate::mcp::McpRegistry>>>,
}

impl ToolRegistry {
    /// Create a new registry with all built-in tools.
    pub fn new(project_root: PathBuf) -> Self {
        let mut definitions = HashMap::new();

        // Register all built-in tools
        for def in file_tools::definitions() {
            definitions.insert(def.name.clone(), def);
        }
        for def in ast::definitions() {
            definitions.insert(def.name.clone(), def);
        }
        for def in grep::definitions() {
            definitions.insert(def.name.clone(), def);
        }
        for def in shell::definitions() {
            definitions.insert(def.name.clone(), def);
        }
        for def in agent::definitions() {
            definitions.insert(def.name.clone(), def);
        }
        for def in glob_tool::definitions() {
            definitions.insert(def.name.clone(), def);
        }
        for def in web_fetch::definitions() {
            definitions.insert(def.name.clone(), def);
        }
        for def in memory::definitions() {
            definitions.insert(def.name.clone(), def);
        }
        for def in todo::definitions() {
            definitions.insert(def.name.clone(), def);
        }

        Self {
            project_root,
            definitions,
            read_cache: std::sync::Mutex::new(HashMap::new()),
            mcp_registry: None,
        }
    }

    /// Attach an MCP registry for external tool support.
    pub fn with_mcp_registry(
        mut self,
        registry: std::sync::Arc<tokio::sync::RwLock<crate::mcp::McpRegistry>>,
    ) -> Self {
        self.mcp_registry = Some(registry);
        self
    }

    /// Get all built-in tool names (excludes MCP tools).
    /// Used by wiring tests to verify every tool is properly integrated.
    pub fn all_builtin_tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.definitions.keys().cloned().collect();
        names.sort();
        names
    }

    /// Get tool definitions, optionally filtered by an allow-list.
    /// Includes MCP tools merged with built-in tools.
    pub fn get_definitions(&self, allowed: &[String]) -> Vec<ToolDefinition> {
        let mut defs: Vec<ToolDefinition> = if allowed.is_empty() {
            self.definitions.values().cloned().collect()
        } else {
            allowed
                .iter()
                .filter_map(|name| self.definitions.get(name).cloned())
                .collect()
        };

        // Merge MCP tool definitions (always included, not filtered by allow-list)
        if let Some(ref mcp) = self.mcp_registry
            && let Ok(registry) = mcp.try_read()
        {
            defs.extend(registry.all_tool_definitions());
        }

        defs
    }

    /// Execute a tool by name with the given JSON arguments.
    pub async fn execute(&self, name: &str, arguments: &str) -> ToolResult {
        // Check if this is an MCP tool (contains '.' separator and belongs to an MCP server)
        if let Some(ref mcp) = self.mcp_registry {
            let is_mcp = {
                let registry = mcp.read().await;
                registry.is_mcp_tool(name)
            };
            if is_mcp {
                let registry = mcp.read().await;
                return match registry.call_tool(name, arguments).await {
                    Ok(output) => ToolResult { output },
                    Err(e) => ToolResult {
                        output: format!("MCP Error: {e}"),
                    },
                };
            }
        }

        let args: Value = match serde_json::from_str(arguments) {
            Ok(v) => v,
            Err(e) => {
                return ToolResult {
                    output: format!("Invalid JSON arguments: {e}"),
                };
            }
        };

        tracing::info!(
            "Executing tool: {name} with args: [{} chars]",
            arguments.len()
        );

        let result = match name {
            // File tools
            "Read" => file_tools::read_file(&self.project_root, &args, &self.read_cache).await,
            "Write" => file_tools::write_file(&self.project_root, &args).await,
            "Edit" => file_tools::edit_file(&self.project_root, &args).await,
            "Delete" => file_tools::delete_file(&self.project_root, &args).await,
            "List" => file_tools::list_files(&self.project_root, &args).await,

            // Search tools
            "Grep" => grep::grep(&self.project_root, &args).await,
            "Glob" => glob_tool::glob_search(&self.project_root, &args).await,
            "AstAnalysis" => ast::ast_analysis(&self.project_root, &args).await,

            // Shell
            "Bash" => shell::run_shell_command(&self.project_root, &args).await,

            // Web
            "WebFetch" => web_fetch::web_fetch(&args).await,

            // Memory
            "MemoryRead" => memory::memory_read(&self.project_root).await,
            "MemoryWrite" => memory::memory_write(&self.project_root, &args).await,

            // Agent tools
            "ListAgents" => {
                let detail = args["detail"].as_bool().unwrap_or(false);
                if detail {
                    Ok(agent::list_agents_detail(&self.project_root))
                } else {
                    let agents = agent::list_agents(&self.project_root);
                    if agents.is_empty() {
                        Ok("No sub-agents configured.".to_string())
                    } else {
                        let lines: Vec<String> = agents
                            .iter()
                            .map(|(name, desc, source)| {
                                if source == "built-in" {
                                    format!("  {name} — {desc}")
                                } else {
                                    format!("  {name} — {desc} [{source}]")
                                }
                            })
                            .collect();
                        Ok(lines.join("\n"))
                    }
                }
            }
            "CreateAgent" => Ok(agent::create_agent(&self.project_root, &args)),
            "InvokeAgent" => {
                // Handled externally by the event loop (needs access to config/db).
                return ToolResult {
                    output: "__INVOKE_AGENT__".to_string(),
                };
            }
            "TodoWrite" => {
                // Handled externally by the event loop (needs access to db/session_id).
                return ToolResult {
                    output: "__TODO_WRITE__".to_string(),
                };
            }
            "TodoRead" => {
                // Handled externally by the event loop (needs access to db/session_id).
                return ToolResult {
                    output: "__TODO_READ__".to_string(),
                };
            }

            other => Err(anyhow::anyhow!("Unknown tool: {other}")),
        };

        match result {
            Ok(output) => ToolResult { output },
            Err(e) => ToolResult {
                output: format!("Error: {e}"),
            },
        }
    }
}

/// Validate and resolve a path, preventing directory traversal.
/// Works for both existing and non-existing files (no canonicalize!).
pub fn safe_resolve_path(project_root: &Path, requested: &str) -> Result<PathBuf> {
    let requested_path = Path::new(requested);

    // Build absolute path and normalize (removes .., . etc.)
    let resolved = if requested_path.is_absolute() {
        requested_path.to_path_buf().clean()
    } else {
        project_root.join(requested_path).clean()
    };

    // Security check: must be within project root
    if !resolved.starts_with(project_root) {
        anyhow::bail!(
            "Path escapes project root. Requested: {requested:?}, Resolved: {resolved:?}"
        );
    }

    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn root() -> PathBuf {
        PathBuf::from("/home/user/project")
    }

    #[test]
    fn test_relative_path_resolves_inside_root() {
        let result = safe_resolve_path(&root(), "src/main.rs").unwrap();
        assert_eq!(result, PathBuf::from("/home/user/project/src/main.rs"));
    }

    #[test]
    fn test_dot_path_resolves_to_root() {
        let result = safe_resolve_path(&root(), ".").unwrap();
        assert_eq!(result, PathBuf::from("/home/user/project"));
    }

    #[test]
    fn test_new_file_in_new_dir_resolves() {
        let result = safe_resolve_path(&root(), "src/brand_new/feature.rs").unwrap();
        assert_eq!(
            result,
            PathBuf::from("/home/user/project/src/brand_new/feature.rs")
        );
    }

    #[test]
    fn test_dotdot_traversal_blocked() {
        let result = safe_resolve_path(&root(), "../../etc/passwd");
        assert!(result.is_err());
    }

    #[test]
    fn test_dotdot_sneaky_traversal_blocked() {
        let result = safe_resolve_path(&root(), "src/../../etc/passwd");
        assert!(result.is_err());
    }

    #[test]
    fn test_absolute_path_inside_root_allowed() {
        let result = safe_resolve_path(&root(), "/home/user/project/src/lib.rs").unwrap();
        assert_eq!(result, PathBuf::from("/home/user/project/src/lib.rs"));
    }

    #[test]
    fn test_absolute_path_outside_root_blocked() {
        let result = safe_resolve_path(&root(), "/etc/shadow");
        assert!(result.is_err());
    }

    #[test]
    fn test_empty_path_resolves_to_root() {
        let result = safe_resolve_path(&root(), "").unwrap();
        assert_eq!(result, PathBuf::from("/home/user/project"));
    }
}

// ── Tool action descriptions ──────────────────────────────────

/// Generate a human-readable description of a tool action for approval prompts.
pub fn describe_action(tool_name: &str, args: &serde_json::Value) -> String {
    match tool_name {
        "Bash" => {
            let cmd = args
                .get("command")
                .or(args.get("cmd"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            cmd.to_string()
        }
        "Delete" => {
            let path = args
                .get("file_path")
                .or(args.get("path"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let recursive = args
                .get("recursive")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if recursive {
                format!("Delete directory (recursive): {path}")
            } else {
                format!("Delete: {path}")
            }
        }
        "Write" => {
            let path = args
                .get("path")
                .or(args.get("file_path"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let overwrite = args
                .get("overwrite")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if overwrite {
                format!("Overwrite file: {path}")
            } else {
                format!("Create file: {path}")
            }
        }
        "Edit" => {
            let path = if let Some(payload) = args.get("payload") {
                payload
                    .get("file_path")
                    .or(payload.get("path"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
            } else {
                args.get("file_path")
                    .or(args.get("path"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
            };
            format!("Edit file: {path}")
        }
        "WebFetch" => {
            let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("?");
            format!("Fetch URL: {url}")
        }
        _ => format!("Execute: {tool_name}"),
    }
}

#[cfg(test)]
mod describe_action_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_describe_bash() {
        let desc = describe_action("Bash", &json!({"command": "cargo build"}));
        assert!(desc.contains("cargo build"));
    }

    #[test]
    fn test_describe_delete() {
        let desc = describe_action("Delete", &json!({"file_path": "old.rs"}));
        assert!(desc.contains("old.rs"));
    }

    #[test]
    fn test_describe_edit() {
        let desc = describe_action("Edit", &json!({"payload": {"file_path": "src/main.rs"}}));
        assert!(desc.contains("src/main.rs"));
    }

    #[test]
    fn test_describe_write() {
        let desc = describe_action("Write", &json!({"path": "new.rs"}));
        assert!(desc.contains("Create file"));
        assert!(desc.contains("new.rs"));
    }

    #[test]
    fn test_describe_write_overwrite() {
        let desc = describe_action("Write", &json!({"path": "x.rs", "overwrite": true}));
        assert!(desc.contains("Overwrite"));
    }
}
