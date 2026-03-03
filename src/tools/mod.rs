//! Tool registry and execution engine.
//!
//! Each tool is a function that takes JSON arguments and returns a string result.
//! Path validation is enforced here to prevent directory traversal.

pub mod agent;
pub mod file_tools;
pub mod glob_tool;
pub mod grep;
pub mod memory;
pub mod reasoning;
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
        for def in reasoning::definitions() {
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
        if let Some(ref mcp) = self.mcp_registry {
            if let Ok(registry) = mcp.try_read() {
                defs.extend(registry.all_tool_definitions());
            }
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

            // Shell
            "Bash" => shell::run_shell_command(&self.project_root, &args).await,

            // Web
            "WebFetch" => web_fetch::web_fetch(&args).await,

            // Memory
            "MemoryRead" => memory::memory_read(&self.project_root).await,
            "MemoryWrite" => memory::memory_write(&self.project_root, &args).await,

            // Reasoning
            "ShareReasoning" => reasoning::share_reasoning(&args).await,

            // Agent tools
            "ListAgents" => {
                let detail = args["detail"].as_bool().unwrap_or(false);
                if detail {
                    Ok(agent::list_agents_detail(&self.project_root))
                } else {
                    Ok(agent::list_agents(&self.project_root))
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
