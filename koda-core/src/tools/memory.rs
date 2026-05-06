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
            description: "Read project and global memory (MEMORY.md + ~/.config/koda/memory.md)."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDefinition {
            name: "MemoryWrite".to_string(),
            description: "Save a project insight or rule to persistent memory (MEMORY.md). \
                Set scope='global' for user-wide preferences (~/.config/koda/memory.md)."
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
    let content = memory::load(project_root)?;
    if content.is_empty() {
        return Ok(
            "No memory stored yet. Use MemoryWrite to save project context or preferences."
                .to_string(),
        );
    }

    let active = memory::active_project_file(project_root);
    let header = match active {
        Some(f) => format!("Active project memory file: {f}"),
        None => "No project memory file (will create MEMORY.md on first write)".to_string(),
    };

    Ok(format!("{header}\n\n{content}"))
}

/// Write a memory entry.
pub async fn memory_write(project_root: &Path, args: &Value) -> Result<String> {
    let content = args["content"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'content' argument"))?;
    let scope = args["scope"].as_str().unwrap_or("project");

    match scope {
        "global" => {
            memory::append_global(content)?;
            Ok(format!("Saved to global memory: {content}"))
        }
        _ => {
            memory::append(project_root, content)?;
            Ok(format!("Saved to project memory: {content}"))
        }
    }
}

// =============================================================
// Tool trait implementations (#1265 item 5, PR-7/N).
//
// `MemoryRead` is read-only; `MemoryWrite` is `LocalMutation`. The
// memory file (`.koda/memory.md`) is its own concern — we don't
// integrate it into the file-undo stack (matches pre-#1265 behavior).
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
    /// `extract_undo_path` returns `None` because the memory file
    /// is intentionally outside the file-undo stack — same as
    /// pre-#1265 (`MemoryWrite` was not in `undo::is_mutating_tool`).
    fn extract_undo_path(&self, _args: &serde_json::Value) -> Option<std::path::PathBuf> {
        None
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

    #[tokio::test]
    async fn test_memory_read_empty() {
        let tmp = TempDir::new().unwrap();
        let result = memory_read(tmp.path()).await.unwrap();
        assert!(result.contains("No memory stored"));
    }

    #[tokio::test]
    async fn test_memory_read_with_content() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("MEMORY.md"), "# Notes\n- Uses Rust").unwrap();
        let result = memory_read(tmp.path()).await.unwrap();
        assert!(result.contains("Uses Rust"));
        assert!(result.contains("MEMORY.md"));
    }

    #[tokio::test]
    async fn test_memory_write_project() {
        let tmp = TempDir::new().unwrap();
        let args = json!({ "content": "This project uses SQLite" });
        let result = memory_write(tmp.path(), &args).await.unwrap();
        assert!(result.contains("project memory"));

        let content = std::fs::read_to_string(tmp.path().join("MEMORY.md")).unwrap();
        assert!(content.contains("This project uses SQLite"));
    }

    #[tokio::test]
    async fn test_memory_write_defaults_to_project() {
        let tmp = TempDir::new().unwrap();
        let args = json!({ "content": "no scope specified" });
        memory_write(tmp.path(), &args).await.unwrap();
        assert!(tmp.path().join("MEMORY.md").exists());
    }
}
