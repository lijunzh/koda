//! Tool registry and execution engine.
//!
//! Each tool is a function that takes JSON arguments and returns a string result.
//! Path validation is enforced here to prevent directory traversal.
//!
//! ## Available tools
//!
//! | Tool | Module | Effect | Description |
//! |---|---|---|---|
//! | **Read** | `file_tools` | ReadOnly | Read file contents with line numbers |
//! | **Write** | `file_tools` | LocalMutation | Create or overwrite a file |
//! | **Edit** | `file_tools` | LocalMutation | Find-and-replace in an existing file |
//! | **Delete** | `file_tools` | Destructive | Delete a file |
//! | **List** | `file_tools` | ReadOnly | List files and directories |
//! | **Bash** | `shell` | LocalMutation | Execute shell commands (with background mode) |
//! | **Grep** | `grep` | ReadOnly | Recursive text search (respects .gitignore) |
//! | **Glob** | `glob_tool` | ReadOnly | Find files by glob pattern |
//! | **WebFetch** | `web_fetch` | RemoteAction | Fetch URL content (HTML→text) |
//! | **WebSearch** | `web_search` | RemoteAction | Web search via DuckDuckGo |
//! | **InvokeAgent** | `agent` | LocalMutation | Delegate task to a sub-agent |
//! | **ListAgents** | `agent` | ReadOnly | List available sub-agents |
//! | **MemoryRead** | `memory` | ReadOnly | Read project/global memory |
//! | **MemoryWrite** | `memory` | LocalMutation | Save facts to memory |
//! | **TodoRead** | `todo` | ReadOnly | Read task list |
//! | **TodoWrite** | `todo` | LocalMutation | Update task list |
//! | **AskUser** | `ask_user` | ReadOnly | Ask the user a question |
//! | **ActivateSkill** | `skills` | ReadOnly | Load a skill's instructions |
//! | **ListSkills** | `skills` | ReadOnly | List available skills |
//!
//! ## Safety model
//!
//! Every tool call is classified by `ToolEffect` and checked against the
//! current approval mode before execution. See
//! `classify_tool` for the effect of each tool.

/// Effect classification for tool calls.
///
/// Two-axis model: what does the tool touch (local vs. remote)
/// and how severe are its effects (read vs. mutate vs. destroy)?
///
/// # Examples
///
/// ```
/// use koda_core::tools::{ToolEffect, classify_tool};
///
/// assert_eq!(classify_tool("Read"), ToolEffect::ReadOnly);
/// assert_eq!(classify_tool("Write"), ToolEffect::LocalMutation);
/// assert_eq!(classify_tool("Delete"), ToolEffect::Destructive);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum ToolEffect {
    /// No side-effects: file reads, grep, git status.
    ReadOnly,
    /// Side-effects on remote services only: GitHub API, WebFetch POST.
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
///
/// Unknown tools default to `LocalMutation` (conservative — always asks).
pub fn classify_tool(name: &str) -> ToolEffect {
    match name {
        // Pure reads — zero side-effects
        "Read" | "List" | "Grep" | "Glob" | "MemoryRead" | "ListAgents" | "ListSkills"
        | "ActivateSkill" | "RecallContext" | "AskUser" => ToolEffect::ReadOnly,

        // Remote actions — side-effects on remote services only
        "WebFetch" => ToolEffect::ReadOnly,    // GET-only fetch
        "WebSearch" => ToolEffect::ReadOnly,   // read-only search
        "InvokeAgent" => ToolEffect::ReadOnly, // sub-agents inherit parent's mode

        // Local mutations — write to filesystem or local state
        "Write" | "Edit" | "MemoryWrite" | "TodoWrite" => ToolEffect::LocalMutation,

        // Bash — default to LocalMutation; refined by classify_bash_command()
        "Bash" => ToolEffect::LocalMutation,

        // Delete is destructive (irreversible without undo)
        "Delete" => ToolEffect::Destructive,

        // Email tools
        "EmailRead" | "EmailSearch" => ToolEffect::ReadOnly,
        "EmailSend" => ToolEffect::LocalMutation,

        // Unknown tools — default to LocalMutation (conservative)
        _ => ToolEffect::LocalMutation,
    }
}

/// Returns true if the tool performs a mutating operation.
///
/// Convenience wrapper over [`classify_tool`] for call sites that only
/// need a bool (e.g., loop guard).
///
/// ```
/// use koda_core::tools::is_mutating_tool;
///
/// assert!(!is_mutating_tool("Read"));
/// assert!(is_mutating_tool("Write"));
/// assert!(is_mutating_tool("Delete"));
/// ```
pub fn is_mutating_tool(name: &str) -> bool {
    !matches!(classify_tool(name), ToolEffect::ReadOnly)
}

/// Sub-agent invocation tool (`InvokeAgent`, `ListAgents`).
pub mod agent;
pub mod ask_user;
pub mod bg_process;
/// File CRUD tools (`Read`, `Write`, `Edit`, `Delete`, `List`).
pub mod file_tools;
pub mod fuzzy;
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
/// Session-scoped task list tool (`TodoWrite`).
pub mod todo;
/// Pre-flight validation for tool calls (runs before approval).
pub mod validate;
/// HTTP fetch tool (`WebFetch`).
pub mod web_fetch;
/// Web search tool (`WebSearch`).
pub mod web_search;

use anyhow::Result;
use path_clean::PathClean;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use crate::output_caps::OutputCaps;

/// Load email config from env, returning a `ToolResult` error if not configured.
///
/// Used by EmailRead, EmailSend, EmailSearch to avoid repeating the same
/// match-and-return-error boilerplate three times.
macro_rules! require_email_config {
    ($self:ident) => {
        match koda_email::config::EmailConfig::from_env() {
            Ok(c) => c,
            Err(e) => {
                return ToolResult {
                    output: format!(
                        "Email not configured: {e:#}\n\n{}",
                        koda_email::config::EmailConfig::setup_instructions()
                    ),
                    success: false,
                    full_output: None,
                };
            }
        }
    };
}
use crate::providers::ToolDefinition;

/// Shared file-read cache: tracks (size, mtime) per cache key so we can
/// detect stale reads and avoid re-streaming unchanged files.
///
/// Wrapped in `Arc` so parent and sub-agent `ToolRegistry` instances
/// share the same cache — reads by one agent benefit all others.
pub type FileReadCache = Arc<std::sync::Mutex<HashMap<String, (u64, SystemTime)>>>;

/// Result of executing a tool.
///
/// The `success` field is set automatically by `ToolRegistry::execute()` —
/// `Ok(…)` → `true`, `Err(…)` → `false`. Individual tool functions just
/// return `Result<String>`.
///
/// ```
/// use koda_core::tools::ToolResult;
///
/// let ok = ToolResult { output: "done".into(), success: true, full_output: None };
/// assert!(ok.success);
/// ```
#[derive(Debug, Clone)]
pub struct ToolResult {
    /// The tool's output string (model-facing; may be a summary for Bash).
    pub output: String,
    /// Whether the tool executed successfully.
    ///
    /// Set automatically by `ToolRegistry::execute()` — `Ok(…)` → `true`,
    /// `Err(…)` → `false`. Individual tools never set this directly;
    /// they just return `Result<String>`.
    pub success: bool,
    /// Full untruncated output, stored separately in DB for later retrieval.
    ///
    /// Only populated by Bash when output exceeds the summary threshold.
    /// `RecallContext` can search this to retrieve details the model didn't
    /// see in its context window.
    pub full_output: Option<String>,
}

/// The tool registry: maps tool names to their definitions and handlers.
pub struct ToolRegistry {
    project_root: PathBuf,
    definitions: HashMap<String, ToolDefinition>,
    read_cache: FileReadCache,
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
    /// Background process registry — tracks processes spawned with `background: true`.
    /// Dropped (SIGTERM all) when the session ends.
    pub bg_registry: bg_process::BgRegistry,
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
        for def in ask_user::definitions() {
            definitions.insert(def.name.clone(), def);
        }
        for def in glob_tool::definitions() {
            definitions.insert(def.name.clone(), def);
        }
        for def in web_fetch::definitions() {
            definitions.insert(def.name.clone(), def);
        }
        for def in web_search::definitions() {
            definitions.insert(def.name.clone(), def);
        }
        for def in todo::definitions() {
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
        // First-party library tools (direct calls)
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

        let skill_registry = crate::skills::SkillRegistry::discover(&project_root);

        Self {
            project_root,
            definitions,
            read_cache: Arc::new(std::sync::Mutex::new(HashMap::new())),
            undo: std::sync::Mutex::new(crate::undo::UndoStack::new()),
            skill_registry,
            db: std::sync::RwLock::new(None),
            session_id: std::sync::RwLock::new(None),
            caps: OutputCaps::for_context(max_context_tokens),
            bg_registry: bg_process::BgRegistry::new(),
        }
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

    /// Get all built-in tool names.
    /// Used by wiring tests to verify every tool is properly integrated.
    pub fn all_builtin_tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.definitions.keys().cloned().collect();
        names.sort();
        names
    }

    /// Check whether a tool name is known.
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

    /// Get tool definitions, optionally filtered by allow/deny lists.
    ///
    /// - `allowed` non-empty → only those tools (allowlist).
    /// - `denied` non-empty → all tools except those (denylist).
    /// - Both empty → all tools.
    /// - If both are specified, allowlist wins (deny is ignored).
    pub fn get_definitions(&self, allowed: &[String], denied: &[String]) -> Vec<ToolDefinition> {
        if !allowed.is_empty() {
            allowed
                .iter()
                .filter_map(|name| self.definitions.get(name).cloned())
                .collect()
        } else if !denied.is_empty() {
            self.definitions
                .values()
                .filter(|d| !denied.contains(&d.name))
                .cloned()
                .collect()
        } else {
            self.definitions.values().cloned().collect()
        }
    }

    /// Execute a tool by name with the given JSON arguments.
    ///
    /// Empty or whitespace-only `arguments` are treated as `{}` (no args)
    /// so that tools can fall through to their own defaults instead of
    /// surfacing a raw JSON parse error.  See #513.
    ///
    /// `sink_for_streaming` is an optional `(sink, call_id)` pair. When
    /// provided, the Bash tool streams each output line as a
    /// `ToolOutputLine` event in real-time.
    pub async fn execute(
        &self,
        name: &str,
        arguments: &str,
        sink_for_streaming: Option<(&dyn crate::engine::EngineSink, &str)>,
    ) -> ToolResult {
        let raw = arguments.trim();
        let raw = if raw.is_empty() { "{}" } else { raw };
        let args: Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(e) => {
                return ToolResult {
                    output: format!("Invalid JSON arguments: {e}"),
                    success: false,
                    full_output: None,
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
            // Shell — returns ShellOutput with summary + full output.
            "Bash" => {
                let shell_result = shell::run_shell_command(
                    &self.project_root,
                    &args,
                    self.caps.shell_output_lines,
                    &self.bg_registry,
                    sink_for_streaming,
                )
                .await;
                return match shell_result {
                    Ok(so) => ToolResult {
                        output: so.summary,
                        success: true,
                        full_output: so.full_output,
                    },
                    Err(e) => ToolResult {
                        output: format!("Error: {e}"),
                        success: false,
                        full_output: None,
                    },
                };
            }

            // Web
            "WebFetch" => web_fetch::web_fetch(&args, self.caps.web_body_chars).await,
            "WebSearch" => web_search::web_search(&args).await,
            "TodoWrite" => {
                let db_opt = self.db.read().ok().and_then(|g| g.clone());
                let sid_opt = self.session_id.read().ok().and_then(|g| g.clone());
                match (db_opt, sid_opt) {
                    (Some(db), Some(sid)) => todo::todo_write(&db, &sid, &args).await,
                    _ => Ok("TodoWrite requires an active session.".to_string()),
                }
            }

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

            // First-party library tools — direct calls
            "EmailRead" => {
                let config = require_email_config!(self);
                let count = args["count"].as_u64().unwrap_or(5).clamp(1, 20) as u32;
                match koda_email::imap_client::read_emails(&config, count).await {
                    Ok(emails) if emails.is_empty() => Ok("No emails found in INBOX.".to_string()),
                    Ok(emails) => Ok(format_email_list(&emails)),
                    Err(e) => Err(anyhow::anyhow!("Error reading emails: {e:#}")),
                }
            }

            "EmailSend" => {
                let config = require_email_config!(self);
                let to = args["to"].as_str().unwrap_or("");
                let subject = args["subject"].as_str().unwrap_or("");
                let body = args["body"].as_str().unwrap_or("");
                koda_email::smtp_client::send_email(&config, to, subject, body)
                    .await
                    .map_err(|e| anyhow::anyhow!("Error sending email: {e:#}"))
            }

            "EmailSearch" => {
                let config = require_email_config!(self);
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
                    success: false,
                    full_output: None,
                };
            }

            "AskUser" => {
                // Handled by execute_tools_sequential (needs sink + cmd_rx).
                // This branch should not be reached in normal flow.
                return ToolResult {
                    output: "AskUser is handled by the inference loop.".to_string(),
                    success: false,
                    full_output: None,
                };
            }

            other => {
                // Detect garbled tool names (JSON blobs, very long strings)
                // — a sign the model can't do structured tool calling.
                let warning = if other.contains('{') || other.len() > 64 {
                    format!(
                        "Unknown tool: {other}. \
                         This model appears to struggle with tool calling. \
                         Consider switching to a model with native function-call support."
                    )
                } else {
                    format!("Unknown tool: {other}")
                };
                Err(anyhow::anyhow!(warning))
            }
        };

        match result {
            Ok(output) => ToolResult {
                output,
                success: true,
                full_output: None,
            },
            Err(e) => ToolResult {
                output: format!("Error: {e}"),
                success: false,
                full_output: None,
            },
        }
    }
}

/// Validate and resolve a path, preventing directory traversal.
///
/// Works for both existing and non-existing files (no `canonicalize!`).
/// Relative paths are joined to `project_root`; absolute paths must
/// still be within `project_root`.
///
/// # Examples
///
/// ```
/// use koda_core::tools::safe_resolve_path;
/// use std::path::Path;
///
/// let root = Path::new("/home/user/project");
///
/// // Relative paths resolve within project
/// let p = safe_resolve_path(root, "src/main.rs").unwrap();
/// assert_eq!(p, Path::new("/home/user/project/src/main.rs"));
///
/// // Traversal is blocked
/// assert!(safe_resolve_path(root, "../../etc/passwd").is_err());
/// ```
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
            let bg = args
                .get("background")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if bg {
                format!("[bg] {cmd}")
            } else {
                cmd.to_string()
            }
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
        "WebSearch" => {
            let q = args.get("query").and_then(|v| v.as_str()).unwrap_or("?");
            format!("Web search: {q}")
        }
        "TodoWrite" => {
            let n = args
                .get("todos")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            format!("Update todo list ({n} tasks)")
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

    #[test]
    fn test_get_definitions_deny_list() {
        let registry = ToolRegistry::new(PathBuf::from("/tmp"), 128_000);
        let denied = vec![
            "Write".to_string(),
            "Edit".to_string(),
            "Delete".to_string(),
        ];
        let defs = registry.get_definitions(&[], &denied);
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(!names.contains(&"Write"));
        assert!(!names.contains(&"Edit"));
        assert!(!names.contains(&"Delete"));
        assert!(names.contains(&"Read"));
        assert!(names.contains(&"Grep"));
    }

    #[test]
    fn test_get_definitions_allow_list_wins_over_deny() {
        let registry = ToolRegistry::new(PathBuf::from("/tmp"), 128_000);
        let allowed = vec!["Read".to_string(), "Write".to_string()];
        let denied = vec!["Write".to_string()];
        // allow wins — Write should be present
        let defs = registry.get_definitions(&allowed, &denied);
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"Read"));
        assert!(names.contains(&"Write"));
    }

    #[test]
    fn test_get_definitions_both_empty_returns_all() {
        let registry = ToolRegistry::new(PathBuf::from("/tmp"), 128_000);
        let all = registry.get_definitions(&[], &[]);
        assert!(all.len() > 10, "Should have many tools");
    }
}
