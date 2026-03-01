//! Tool registry and execution engine.
//!
//! Each tool is a function that takes JSON arguments and returns a string result.
//! Path validation is enforced here to prevent directory traversal.

pub mod agent;
pub mod constructor;
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

use crate::providers::ToolDefinition;

/// Result of executing a tool.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ToolResult {
    pub output: String,
    pub success: bool,
}

/// The tool registry: maps tool names to their definitions and handlers.
pub struct ToolRegistry {
    project_root: PathBuf,
    definitions: HashMap<String, ToolDefinition>,
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
        for def in todo::definitions() {
            definitions.insert(def.name.clone(), def);
        }
        for def in memory::definitions() {
            definitions.insert(def.name.clone(), def);
        }
        for def in constructor::definitions() {
            definitions.insert(def.name.clone(), def);
        }

        // Load custom tools from agents/tools/
        for def in constructor::load_custom_tool_definitions(&project_root) {
            definitions.insert(def.name.clone(), def);
        }

        Self {
            project_root,
            definitions,
        }
    }

    /// Get tool definitions, optionally filtered by an allow-list.
    pub fn get_definitions(&self, allowed: &[String]) -> Vec<ToolDefinition> {
        if allowed.is_empty() {
            return self.definitions.values().cloned().collect();
        }
        allowed
            .iter()
            .filter_map(|name| self.definitions.get(name).cloned())
            .collect()
    }

    /// Execute a tool by name with the given JSON arguments.
    pub async fn execute(&self, name: &str, arguments: &str) -> ToolResult {
        let args: Value = match serde_json::from_str(arguments) {
            Ok(v) => v,
            Err(e) => {
                return ToolResult {
                    output: format!("Invalid JSON arguments: {e}"),
                    success: false,
                };
            }
        };

        tracing::info!(
            "Executing tool: {name} with args: [{} chars]",
            arguments.len()
        );

        let result = match name {
            // File tools
            "Read" => file_tools::read_file(&self.project_root, &args).await,
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

            // Task tracking
            "TodoRead" => todo::todo_read(&self.project_root).await,
            "TodoWrite" => todo::todo_write(&self.project_root, &args).await,

            // Memory
            "MemoryRead" => memory::memory_read(&self.project_root).await,
            "MemoryWrite" => memory::memory_write(&self.project_root, &args).await,

            // Agent tools
            "ListAgents" => Ok(agent::list_agents(&self.project_root)),
            "InvokeAgent" => {
                // Handled externally by the event loop (needs access to config/db).
                return ToolResult {
                    output: "__INVOKE_AGENT__".to_string(),
                    success: true,
                };
            }

            // Tool constructor
            "CreateTool" => constructor::create_tool(&self.project_root, &args).await,
            "ListTools" => constructor::list_custom_tools(&self.project_root).await,
            "DeleteTool" => constructor::delete_custom_tool(&self.project_root, &args).await,

            // Check custom tools
            other => {
                // Try to execute as a custom tool
                match constructor::execute_custom_tool(&self.project_root, other, &args).await {
                    Ok(output) => Ok(output),
                    Err(_) => Err(anyhow::anyhow!("Unknown tool: {other}")),
                }
            }
        };

        match result {
            Ok(output) => ToolResult {
                output,
                success: true,
            },
            Err(e) => ToolResult {
                output: format!("Error: {e}"),
                success: false,
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
