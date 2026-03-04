//! Memory tools: read and write semantic memory.
//!
//! Exposes `MemoryRead` and `MemoryWrite` as tools the LLM can call
//! to inspect and persist project/global context.

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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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
