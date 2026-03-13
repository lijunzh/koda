//! Tool registry and execution engine.
//!
//! Each tool is a function that takes JSON arguments and returns a string result.
//! Path validation is enforced here to prevent directory traversal.

/// Effect classification for tool calls.
///
/// Two-axis model: what does the tool touch (local vs. remote)
/// and how severe are its effects (read vs. mutate vs. destroy)?
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum ToolEffect {
    /// No side-effects: file reads, grep, git status.
    ReadOnly,
    /// Side-effects on remote services only: GitHub API, WebFetch POST, MCP remote tools.
    RemoteAction,
    /// Mutates local filesystem or state: Write, Edit, Delete, MemoryWrite.
    LocalMutation,
    /// Irreversible or high-blast-radius: rm -rf, git push --force, DROP TABLE.
    Destructive,
}

/// Classify a built-in tool by name.
///
/// For `Bash`, this returns the *default* classification (`LocalMutation`);
/// the actual effect depends on the command string and must be refined
/// via [`crate::bash_safety::classify_bash_command`].
pub fn classify_tool(name: &str) -> ToolEffect {
    match name {
        // Pure reads — zero side-effects
        "Read" | "List" | "Grep" | "Glob" | "MemoryRead" | "ListAgents" | "ListSkills"
        | "ActivateSkill" | "RecallContext" | "AstAnalysis" => ToolEffect::ReadOnly,

        // Remote actions — side-effects on remote services only
        "WebFetch" => ToolEffect::ReadOnly,    // GET-only fetch
        "InvokeAgent" => ToolEffect::ReadOnly, // sub-agents inherit parent's mode

        // Local mutations — write to filesystem or local state
        "Write" | "Edit" | "MemoryWrite" => ToolEffect::LocalMutation,

        // Bash — default to LocalMutation; refined by classify_bash_command()
        "Bash" => ToolEffect::LocalMutation,

        // Delete is destructive (irreversible without undo)
        "Delete" => ToolEffect::Destructive,

        // Email tools
        "EmailRead" | "EmailSearch" => ToolEffect::ReadOnly,
        "EmailSend" => ToolEffect::RemoteAction,

        // Unknown tools (including MCP) — default to LocalMutation (conservative)
        _ => ToolEffect::LocalMutation,
    }
}

/// Returns true if the tool performs a mutating operation.
///
/// Convenience wrapper over [`classify_tool`] for call sites that only
/// need a bool (e.g., loop guard).
pub fn is_mutating_tool(name: &str) -> bool {
    !matches!(classify_tool(name), ToolEffect::ReadOnly)
}

/// Sub-agent invocation tool (`InvokeAgent`, `ListAgents`).
pub mod agent;
/// File CRUD tools (`Read`, `Write`, `Edit`, `Delete`, `List`).
pub mod file_tools;
/// Glob pattern search tool (`Glob`).
pub mod glob_tool;
/// Recursive text search tool (`Grep`).
pub mod grep;
/// Project memory read/write tools (`MemoryRead`, `MemoryWrite`).
pub mod memory;
/// On-demand conversation history retrieval (`RecallContext`).
pub mod recall;
/// Shell command execution tool (`Bash`).
pub mod shell;
/// Skill discovery and activation tools (`ListSkills`, `ActivateSkill`).
pub mod skill_tools;
/// HTTP fetch tool (`WebFetch`).  
pub mod web_fetch;

use anyhow::Result;
use path_clean::PathClean;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use crate::output_caps::OutputCaps;
use crate::providers::ToolDefinition;

/// Shared file-read cache: tracks (size, mtime) per cache key so we can
/// detect stale reads and avoid re-streaming unchanged files.
///
/// Wrapped in `Arc` so parent and sub-agent `ToolRegistry` instances
/// share the same cache — reads by one agent benefit all others.
pub type FileReadCache = Arc<std::sync::Mutex<HashMap<String, (u64, SystemTime)>>>;

/// Result of executing a tool.
#[derive(Debug, Clone)]
pub struct ToolResult {
    /// The tool's output string.
    pub output: String,
}

/// The tool registry: maps tool names to their definitions and handlers.
pub struct ToolRegistry {
    project_root: PathBuf,
    definitions: HashMap<String, ToolDefinition>,
    read_cache: FileReadCache,
    /// Connected MCP servers providing additional tools.
    mcp_registry: Option<std::sync::Arc<tokio::sync::RwLock<crate::mcp::McpRegistry>>>,
    /// Undo stack for file mutations.
    pub undo: std::sync::Mutex<crate::undo::UndoStack>,
    /// Discovered skills.
    pub skill_registry: crate::skills::SkillRegistry,
    /// Database handle for tools that need session access (RecallContext).
    db: std::sync::RwLock<Option<std::sync::Arc<crate::db::Database>>>,
    /// Current session ID (for RecallContext).
    session_id: std::sync::RwLock<Option<String>>,
    /// Context-scaled output caps for all tools.
    pub caps: OutputCaps,
}

impl ToolRegistry {
    /// Create a new registry with all built-in tools.
    ///
    /// `max_context_tokens` scales all output caps (see `OutputCaps`).
    pub fn new(project_root: PathBuf, max_context_tokens: usize) -> Self {
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
        for def in skill_tools::definitions() {
            definitions.insert(def.name.clone(), def);
        }
        // RecallContext — on-demand history retrieval
        let recall_def = recall::definition();
        definitions.insert(recall_def.name.clone(), recall_def);
        // First-party library tools (direct calls, no MCP IPC)
        for td in koda_ast::tool_definitions() {
            definitions.insert(
                td.name.to_string(),
                ToolDefinition {
                    name: td.name.to_string(),
                    description: td.description.to_string(),
                    parameters: serde_json::from_str(td.parameters_json).unwrap_or_default(),
                },
            );
        }
        for td in koda_email::tool_definitions() {
            definitions.insert(
                td.name.to_string(),
                ToolDefinition {
                    name: td.name.to_string(),
                    description: td.description.to_string(),
                    parameters: serde_json::from_str(td.parameters_json).unwrap_or_default(),
                },
            );
        }

        // Auto-provisionable MCP tools (registered so the LLM knows they exist)
        for def in crate::mcp::capability_registry::tool_definitions() {
            definitions.insert(def.name.clone(), def);
        }

        let skill_registry = crate::skills::SkillRegistry::discover(&project_root);

        Self {
            project_root,
            definitions,
            read_cache: Arc::new(std::sync::Mutex::new(HashMap::new())),
            mcp_registry: None,
            undo: std::sync::Mutex::new(crate::undo::UndoStack::new()),
            skill_registry,
            db: std::sync::RwLock::new(None),
            session_id: std::sync::RwLock::new(None),
            caps: OutputCaps::for_context(max_context_tokens),
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

    /// Share an existing file-read cache (e.g. from the parent agent).
    ///
    /// Sub-agents that share the parent's cache avoid redundant disk reads
    /// for files already loaded in the same session.
    pub fn with_shared_cache(mut self, cache: FileReadCache) -> Self {
        self.read_cache = cache;
        self
    }

    /// Get a clone of the `Arc` file-read cache for sharing with sub-agents.
    pub fn file_read_cache(&self) -> FileReadCache {
        Arc::clone(&self.read_cache)
    }

    /// Attach database + session for tools that need history access.
    pub fn set_session(&self, db: std::sync::Arc<crate::db::Database>, session_id: String) {
        if let Ok(mut guard) = self.db.write() {
            *guard = Some(db);
        }
        if let Ok(mut guard) = self.session_id.write() {
            *guard = Some(session_id);
        }
    }

    /// Get all built-in tool names (excludes MCP tools).
    /// Used by wiring tests to verify every tool is properly integrated.
    pub fn all_builtin_tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.definitions.keys().cloned().collect();
        names.sort();
        names
    }

    /// Check whether a tool name is known (built-in or MCP).
    pub fn has_tool(&self, name: &str) -> bool {
        self.definitions.contains_key(name)
    }

    /// List all available skills as `(name, description, source)` tuples.
    pub fn list_skills(&self) -> Vec<(String, String, String)> {
        self.skill_registry
            .list()
            .into_iter()
            .map(|m| {
                let source = match m.source {
                    crate::skills::SkillSource::BuiltIn => "built-in",
                    crate::skills::SkillSource::User => "user",
                    crate::skills::SkillSource::Project => "project",
                };
                (m.name.clone(), m.description.clone(), source.to_string())
            })
            .collect()
    }

    /// Search skills by query, returning `(name, description, source)` tuples.
    pub fn search_skills(&self, query: &str) -> Vec<(String, String, String)> {
        self.skill_registry
            .search(query)
            .into_iter()
            .map(|m| {
                let source = match m.source {
                    crate::skills::SkillSource::BuiltIn => "built-in",
                    crate::skills::SkillSource::User => "user",
                    crate::skills::SkillSource::Project => "project",
                };
                (m.name.clone(), m.description.clone(), source.to_string())
            })
            .collect()
    }

    /// Get tool definitions, optionally filtered by an allow-list.
    /// Includes MCP tools merged with built-in tools.
    pub fn get_definitions(&self, allowed: &[String]) -> Vec<ToolDefinition> {
        let mut defs: Vec<ToolDefinition> = if !allowed.is_empty() {
            allowed
                .iter()
                .filter_map(|name| self.definitions.get(name).cloned())
                .collect()
        } else {
            self.definitions.values().cloned().collect()
        };

        // Merge MCP tool definitions (always included)
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

        // Snapshot file before mutation (for /undo)
        if let Some(file_path) = crate::undo::is_mutating_tool(name)
            .then(|| crate::undo::extract_file_path(name, &args))
            .flatten()
        {
            let resolved = self.project_root.join(&file_path);
            if let Ok(mut undo) = self.undo.lock() {
                undo.snapshot(&resolved);
            }
        }

        let result = match name {
            // File tools
            "Read" => file_tools::read_file(&self.project_root, &args, &self.read_cache).await,
            "Write" => file_tools::write_file(&self.project_root, &args).await,
            "Edit" => file_tools::edit_file(&self.project_root, &args).await,
            "Delete" => file_tools::delete_file(&self.project_root, &args).await,
            "List" => {
                file_tools::list_files(&self.project_root, &args, self.caps.list_entries).await
            }

            // Search tools
            "Grep" => grep::grep(&self.project_root, &args, self.caps.grep_matches).await,
            "Glob" => {
                glob_tool::glob_search(&self.project_root, &args, self.caps.glob_results).await
            }

            // Shell
            "Bash" => {
                shell::run_shell_command(&self.project_root, &args, self.caps.shell_output_lines)
                    .await
            }

            // Web
            "WebFetch" => web_fetch::web_fetch(&args, self.caps.web_body_chars).await,

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
            // Skill tools
            "ListSkills" => Ok(skill_tools::list_skills(&self.skill_registry, &args)),
            "ActivateSkill" => Ok(skill_tools::activate_skill(&self.skill_registry, &args)),

            // Recall context tool
            "RecallContext" => {
                let db_opt = self.db.read().ok().and_then(|g| g.clone());
                let sid_opt = self.session_id.read().ok().and_then(|g| g.clone());
                if let (Some(db), Some(sid)) = (db_opt, sid_opt) {
                    Ok(recall::recall_context(&db, &sid, &args).await)
                } else {
                    Ok("RecallContext requires an active session.".to_string())
                }
            }

            // First-party library tools — direct calls, no MCP IPC
            "AstAnalysis" => {
                let action = args["action"].as_str().unwrap_or("");
                let file_path = args["file_path"].as_str().unwrap_or("");
                let symbol = args["symbol"].as_str();
                koda_ast::execute(&self.project_root, action, file_path, symbol)
                    .map_err(|e| anyhow::anyhow!(e))
            }

            "EmailRead" => {
                let config = match koda_email::config::EmailConfig::from_env() {
                    Ok(c) => c,
                    Err(e) => {
                        return ToolResult {
                            output: format!(
                                "Email not configured: {e:#}\n\n{}",
                                koda_email::config::EmailConfig::setup_instructions()
                            ),
                        };
                    }
                };
                let count = args["count"].as_u64().unwrap_or(5).clamp(1, 20) as u32;
                match koda_email::imap_client::read_emails(&config, count).await {
                    Ok(emails) if emails.is_empty() => Ok("No emails found in INBOX.".to_string()),
                    Ok(emails) => Ok(format_email_list(&emails)),
                    Err(e) => Err(anyhow::anyhow!("Error reading emails: {e:#}")),
                }
            }

            "EmailSend" => {
                let config = match koda_email::config::EmailConfig::from_env() {
                    Ok(c) => c,
                    Err(e) => {
                        return ToolResult {
                            output: format!(
                                "Email not configured: {e:#}\n\n{}",
                                koda_email::config::EmailConfig::setup_instructions()
                            ),
                        };
                    }
                };
                let to = args["to"].as_str().unwrap_or("");
                let subject = args["subject"].as_str().unwrap_or("");
                let body = args["body"].as_str().unwrap_or("");
                koda_email::smtp_client::send_email(&config, to, subject, body)
                    .await
                    .map_err(|e| anyhow::anyhow!("Error sending email: {e:#}"))
            }

            "EmailSearch" => {
                let config = match koda_email::config::EmailConfig::from_env() {
                    Ok(c) => c,
                    Err(e) => {
                        return ToolResult {
                            output: format!(
                                "Email not configured: {e:#}\n\n{}",
                                koda_email::config::EmailConfig::setup_instructions()
                            ),
                        };
                    }
                };
                let query = args["query"].as_str().unwrap_or("");
                let max = args["max_results"].as_u64().unwrap_or(10).clamp(1, 50) as u32;
                match koda_email::imap_client::search_emails(&config, query, max).await {
                    Ok(emails) if emails.is_empty() => {
                        Ok(format!("No emails found matching: {query}"))
                    }
                    Ok(emails) => Ok(format!(
                        "Found {} result(s) for \"{query}\":\n\n{}",
                        emails.len(),
                        format_email_list(&emails)
                    )),
                    Err(e) => Err(anyhow::anyhow!("Error searching emails: {e:#}")),
                }
            }

            "InvokeAgent" => {
                // Handled by tool_dispatch.rs before reaching here.
                // This branch should not be reached in normal flow.
                return ToolResult {
                    output: "InvokeAgent is handled by the inference loop.".to_string(),
                };
            }

            other => {
                // Auto-provision: check capability registry for MCP servers
                // that provide this tool
                if let Some(entry) = crate::mcp::capability_registry::find_server_for_tool(other) {
                    // Try to auto-connect the MCP server
                    if let Some(ref mcp) = self.mcp_registry {
                        if crate::mcp::capability_registry::binary_exists(entry.command) {
                            let config = crate::mcp::capability_registry::to_mcp_config(entry);
                            let mut registry = mcp.write().await;
                            match registry
                                .add_server(entry.server_name.to_string(), config)
                                .await
                            {
                                Ok(()) => {
                                    tracing::info!(
                                        "Auto-provisioned MCP server '{}' for tool '{}'",
                                        entry.server_name,
                                        other
                                    );
                                    // Retry the tool call through the now-connected MCP server
                                    let namespaced = format!("{}.{}", entry.server_name, other);
                                    drop(registry);
                                    let registry = mcp.read().await;
                                    return match registry.call_tool(&namespaced, arguments).await {
                                        Ok(output) => ToolResult { output },
                                        Err(e) => ToolResult {
                                            output: format!("MCP Error: {e}"),
                                        },
                                    };
                                }
                                Err(e) => {
                                    return ToolResult {
                                        output: format!(
                                            "Failed to start MCP server '{}': {e}\n\
                                             Install: {}",
                                            entry.server_name, entry.install_hint
                                        ),
                                    };
                                }
                            }
                        } else {
                            return ToolResult {
                                output: format!(
                                    "Tool '{}' is available via the '{}' MCP server, \
                                     but '{}' is not installed.\n\
                                     Description: {}\n\
                                     Install: {}\n\
                                     Then restart koda to use it.",
                                    other,
                                    entry.server_name,
                                    entry.command,
                                    entry.description,
                                    entry.install_hint
                                ),
                            };
                        }
                    } else {
                        // MCP registry not available (e.g., test mode)
                        return ToolResult {
                            output: format!(
                                "Tool '{}' is available via the '{}' MCP server.\n\
                                 Install: {}",
                                other, entry.server_name, entry.install_hint
                            ),
                        };
                    }
                }
                Err(anyhow::anyhow!("Unknown tool: {other}"))
            }
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

/// Format email summaries for LLM-friendly output.
fn format_email_list(emails: &[koda_email::imap_client::EmailSummary]) -> String {
    emails
        .iter()
        .enumerate()
        .map(|(i, e)| {
            format!(
                "{}. [{}] {}\n   From: {}\n   Date: {}\n   {}\n",
                i + 1,
                e.uid,
                e.subject,
                e.from,
                e.date,
                if e.snippet.is_empty() {
                    "(no preview)"
                } else {
                    &e.snippet
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
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
        "AstAnalysis" => {
            let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("?");
            let file = args
                .get("file_path")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            format!("AST {action}: {file}")
        }
        "EmailSend" => {
            let to = args.get("to").and_then(|v| v.as_str()).unwrap_or("?");
            let subject = args.get("subject").and_then(|v| v.as_str()).unwrap_or("?");
            format!("Send email to {to}: {subject}")
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
