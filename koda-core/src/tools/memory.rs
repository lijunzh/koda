//! Memory tools — read and write semantic memory.
//!
//! Exposes `MemoryRead` and `MemoryWrite` as tools the LLM can call
//! to inspect and persist project/global context.
//!
//! ## MemoryRead
//!
//! Returns the current contents of project and global memory files.
//! No parameters required.
//!
//! ## MemoryWrite
//!
//! Appends a fact to `MEMORY.md` (project) or `~/.config/koda/memory.md` (global).
//!
//! - **`content`** (required) — The fact or convention to remember
//! - **`scope`** (optional, default `"project"`) — `"project"` or `"global"`
//!
//! Memory is injected into the system prompt on every turn, so saved facts
//! persist across sessions and compactions. See [`crate::memory`] for the
//! file format and loading logic.

use crate::memory;
use crate::providers::ToolDefinition;
use anyhow::Result;
use serde_json::{Value, json};
use std::path::Path;

/// Return tool definitions for the LLM.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "MemoryRead".to_string(),
            description: "Read the project's instruction files (AGENTS.md, or CLAUDE.md) and \
                the notes saved with MemoryWrite."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDefinition {
            name: "MemoryWrite".to_string(),
            description: "Save a project insight or rule to Koda's notes, kept outside the \
                project and loaded in later sessions. Set scope='global' for user-wide \
                preferences."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "The insight or rule to remember"
                    },
                    "scope": {
                        "type": "string",
                        "description": "'project' (default) or 'global'"
                    }
                },
                "required": ["content"]
            }),
        },
    ]
}

/// Read all loaded memory.
pub async fn memory_read(project_root: &Path) -> Result<String> {
    memory_read_in(crate::db::config_dir().ok().as_deref(), project_root)
}

fn memory_read_in(config: Option<&Path>, project_root: &Path) -> Result<String> {
    let content = memory::load_in(project_root, config)?;
    if content.is_empty() {
        return Ok(
            "No memory stored yet. Use MemoryWrite to save project context or preferences."
                .to_string(),
        );
    }
    let notes = config
        .map(|c| {
            memory::project_notes_path_in(c, project_root)
                .display()
                .to_string()
        })
        .unwrap_or_else(|| "(no config directory)".to_string());
    let header = match memory::active_project_file(project_root) {
        Some(f) => format!("Project instructions: {f}. Koda's notes: {notes}"),
        None => format!("No project instruction file. Koda's notes: {notes}"),
    };
    Ok(format!("{header}\n\n{content}"))
}

/// Write a memory entry to Koda's notes, outside the project.
pub async fn memory_write(project_root: &Path, args: &Value) -> Result<String> {
    memory_write_in(&crate::db::config_dir()?, project_root, args)
}

fn memory_write_in(config: &Path, project_root: &Path, args: &Value) -> Result<String> {
    let content = args["content"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'content' argument"))?;
    match args["scope"].as_str().unwrap_or("project") {
        "global" => {
            memory::append_global_in(config, content)?;
            Ok(format!("Saved to Koda's global notes: {content}"))
        }
        _ => {
            memory::append_in(config, project_root, content)?;
            Ok(format!("Saved to Koda's notes for this project: {content}"))
        }
    }
}

// =============================================================
// Tool trait implementations (#1265 item 5, PR-7/N).
//
// `MemoryRead` is read-only; `MemoryWrite` is `LocalMutation`.
// =============================================================

use crate::tools::{Tool, ToolEffect, ToolExecCtx, ToolResult};
use async_trait::async_trait;

/// `MemoryRead` — return the project memory file contents.
pub struct MemoryReadTool;

#[async_trait]
impl Tool for MemoryReadTool {
    fn name(&self) -> &'static str {
        "MemoryRead"
    }
    fn definition(&self) -> ToolDefinition {
        definitions()
            .into_iter()
            .find(|d| d.name == "MemoryRead")
            .expect("memory::definitions() must contain MemoryRead")
    }
    fn classify(&self, _args: &serde_json::Value) -> ToolEffect {
        ToolEffect::ReadOnly
    }
    async fn execute(&self, ctx: &ToolExecCtx<'_>, _args: &serde_json::Value) -> ToolResult {
        let r = memory_read(ctx.project_root).await;
        crate::tools::wrap_result(r)
    }
}

/// `MemoryWrite` — append a line to the project memory file.
pub struct MemoryWriteTool;

#[async_trait]
impl Tool for MemoryWriteTool {
    fn name(&self) -> &'static str {
        "MemoryWrite"
    }
    fn definition(&self) -> ToolDefinition {
        definitions()
            .into_iter()
            .find(|d| d.name == "MemoryWrite")
            .expect("memory::definitions() must contain MemoryWrite")
    }
    fn classify(&self, _args: &serde_json::Value) -> ToolEffect {
        ToolEffect::LocalMutation
    }
    async fn execute(&self, ctx: &ToolExecCtx<'_>, args: &serde_json::Value) -> ToolResult {
        let r = memory_write(ctx.project_root, args).await;
        crate::tools::wrap_result(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── definitions ──────────────────────────────────────────────────────

    #[test]
    fn test_definitions_returns_two_tools() {
        let defs = definitions();
        assert_eq!(defs.len(), 2);
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"MemoryRead"));
        assert!(names.contains(&"MemoryWrite"));
    }

    #[test]
    fn test_memory_write_requires_content() {
        let write_def = definitions()
            .into_iter()
            .find(|d| d.name == "MemoryWrite")
            .unwrap();
        let required: Vec<&str> = write_def.parameters["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(required.contains(&"content"));
        assert!(!required.contains(&"scope"), "scope should be optional");
    }

    #[test]
    fn test_memory_read_has_no_required_params() {
        let read_def = definitions()
            .into_iter()
            .find(|d| d.name == "MemoryRead")
            .unwrap();
        // MemoryRead takes no parameters at all.
        let props = &read_def.parameters["properties"];
        assert!(
            props.as_object().map(|o| o.is_empty()).unwrap_or(true),
            "MemoryRead should have no properties"
        );
    }

    // ── memory_read / memory_write ──────────────────────────────────

    #[test]
    fn test_memory_read_empty() {
        let (tmp, config) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let result = memory_read_in(Some(config.path()), tmp.path()).unwrap();
        assert!(result.contains("No memory stored"));
    }

    #[test]
    fn test_memory_read_with_content() {
        let (tmp, config) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        std::fs::write(tmp.path().join("AGENTS.md"), "# Notes\n- Uses Rust").unwrap();
        let result = memory_read_in(Some(config.path()), tmp.path()).unwrap();
        assert!(result.contains("Uses Rust"));
        assert!(
            result.contains("Project instructions: AGENTS.md"),
            "{result}"
        );
    }

    #[test]
    fn test_memory_write_stays_out_of_the_project() {
        let (tmp, config) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        std::fs::write(tmp.path().join("CLAUDE.md"), "rules").unwrap();
        let args = json!({ "content": "This project uses SQLite" });
        let result = memory_write_in(config.path(), tmp.path(), &args).unwrap();
        assert!(result.contains("notes for this project"), "{result}");

        assert_eq!(
            std::fs::read_to_string(tmp.path().join("CLAUDE.md")).unwrap(),
            "rules"
        );
        assert!(!tmp.path().join("MEMORY.md").exists());
        let notes = memory::project_notes_path_in(config.path(), tmp.path());
        assert!(
            std::fs::read_to_string(notes)
                .unwrap()
                .contains("uses SQLite")
        );
        let read = memory_read_in(Some(config.path()), tmp.path()).unwrap();
        assert!(read.contains("uses SQLite"), "{read}");
    }

    #[test]
    fn test_memory_write_global_goes_to_the_global_notes() {
        let (tmp, config) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let args = json!({ "content": "prefers tabs", "scope": "global" });
        memory_write_in(config.path(), tmp.path(), &args).unwrap();
        let global = config.path().join("memories/MEMORY.md");
        assert!(
            std::fs::read_to_string(global)
                .unwrap()
                .contains("prefers tabs")
        );
    }
}
