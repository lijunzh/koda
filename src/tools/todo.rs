//! Todo tool: task tracking persisted in `~/.config/koda/todo.md`.
//!
//! Tasks can be **project-scoped** (tied to a project root) or **global**.
//! All tasks live in a single file, organized by section headers.

use crate::providers::ToolDefinition;
use anyhow::Result;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Return the path to the shared todo file: `~/.config/koda/todo.md`.
fn todo_file_path() -> Result<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| anyhow::anyhow!("Cannot determine home directory"))?;
    let path = PathBuf::from(home)
        .join(".config")
        .join("koda")
        .join("todo.md");
    Ok(path)
}

/// Derive a short section key from the project root (e.g. "~/repo/my-app" → "my-app").
fn project_key(project_root: &Path) -> String {
    project_root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Return tool definitions for the LLM.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "TodoRead".to_string(),
            description: "Read the current task list from ~/.config/koda/todo.md.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDefinition {
            name: "TodoWrite".to_string(),
            description: "Create or update tasks. Use TodoRead first to see existing tasks."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "tasks": {
                        "type": "array",
                        "description": "List of tasks to write",
                        "items": {
                            "type": "object",
                            "properties": {
                                "description": {
                                    "type": "string",
                                    "description": "Task description"
                                },
                                "done": {
                                    "type": "boolean",
                                    "description": "Whether the task is completed (default: false)"
                                },
                                "scope": {
                                    "type": "string",
                                    "description": "'project' (default) or 'global'"
                                }
                            },
                            "required": ["description"]
                        }
                    }
                },
                "required": ["tasks"]
            }),
        },
    ]
}

/// Read the entire todo file.
pub async fn todo_read(project_root: &Path) -> Result<String> {
    let path = todo_file_path()?;

    if !path.exists() {
        return Ok("No tasks yet. Use TodoWrite to create tasks.".to_string());
    }

    let content = tokio::fs::read_to_string(&path).await?;
    if content.trim().is_empty() {
        return Ok("Task list is empty.".to_string());
    }

    // Add context about which project section is "ours"
    let key = project_key(project_root);
    Ok(format!("Current project: {key}\n\n{content}"))
}

/// Write/update tasks, merging into the existing todo file by section.
pub async fn todo_write(project_root: &Path, args: &Value) -> Result<String> {
    let tasks = args["tasks"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Missing 'tasks' argument"))?;

    let path = todo_file_path()?;
    let key = project_key(project_root);

    // Parse existing file into sections
    let existing = if path.exists() {
        tokio::fs::read_to_string(&path).await.unwrap_or_default()
    } else {
        String::new()
    };
    let mut sections = parse_sections(&existing);

    // Bucket new tasks by scope
    let mut project_tasks: Vec<(String, bool)> = Vec::new();
    let mut global_tasks: Vec<(String, bool)> = Vec::new();

    for task in tasks {
        let desc = task["description"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Task missing 'description'"))?;
        let done = task["done"].as_bool().unwrap_or(false);
        let scope = task["scope"].as_str().unwrap_or("project");

        match scope {
            "global" => global_tasks.push((desc.to_string(), done)),
            _ => project_tasks.push((desc.to_string(), done)),
        }
    }

    // Update sections
    if !project_tasks.is_empty() {
        sections.insert(key.clone(), format_tasks(&project_tasks));
    }
    if !global_tasks.is_empty() {
        sections.insert("Global".to_string(), format_tasks(&global_tasks));
    }

    // Render and write
    let content = render_sections(&sections);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&path, &content).await?;

    let total = project_tasks.len() + global_tasks.len();
    let pending = project_tasks
        .iter()
        .chain(&global_tasks)
        .filter(|(_, d)| !d)
        .count();
    let completed = total - pending;
    Ok(format!(
        "Updated todo: {pending} pending, {completed} completed ({total} total)"
    ))
}

// ── Internal helpers ──────────────────────────────────────────

/// Parse a todo.md into a map of section_name → raw markdown body.
fn parse_sections(content: &str) -> std::collections::BTreeMap<String, String> {
    let mut sections = std::collections::BTreeMap::new();
    let mut current_section = String::new();
    let mut current_body = String::new();

    for line in content.lines() {
        if let Some(header) = line.strip_prefix("## ") {
            if !current_section.is_empty() {
                sections.insert(current_section.clone(), current_body.trim().to_string());
            }
            current_section = header.trim().to_string();
            current_body = String::new();
        } else if !current_section.is_empty() {
            current_body.push_str(line);
            current_body.push('\n');
        }
    }
    if !current_section.is_empty() {
        sections.insert(current_section, current_body.trim().to_string());
    }
    sections
}

/// Format a list of (description, done) into markdown checklist lines.
fn format_tasks(tasks: &[(String, bool)]) -> String {
    tasks
        .iter()
        .map(|(desc, done)| {
            if *done {
                format!("- [x] {desc}")
            } else {
                format!("- [ ] {desc}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render all sections back into a single markdown file.
fn render_sections(sections: &std::collections::BTreeMap<String, String>) -> String {
    let mut out = String::from("# Tasks\n\n");
    for (name, body) in sections {
        out.push_str(&format!("## {name}\n\n{body}\n\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_sections() {
        let md = "# Tasks\n\n## my-app\n\n- [ ] Do thing\n\n## Global\n\n- [x] Done\n";
        let sections = parse_sections(md);
        assert_eq!(sections.len(), 2);
        assert!(sections.get("my-app").unwrap().contains("Do thing"));
        assert!(sections.get("Global").unwrap().contains("Done"));
    }

    #[test]
    fn test_parse_sections_empty() {
        let sections = parse_sections("");
        assert!(sections.is_empty());
    }

    #[test]
    fn test_project_key() {
        assert_eq!(project_key(Path::new("/home/user/repo/my-app")), "my-app");
        assert_eq!(project_key(Path::new("/tmp")), "tmp");
    }

    #[test]
    fn test_format_tasks() {
        let tasks = vec![
            ("Done task".to_string(), true),
            ("Pending task".to_string(), false),
        ];
        let result = format_tasks(&tasks);
        assert_eq!(result, "- [x] Done task\n- [ ] Pending task");
    }

    #[test]
    fn test_render_sections_roundtrip() {
        let mut sections = std::collections::BTreeMap::new();
        sections.insert("Global".to_string(), "- [ ] Ship it".to_string());
        sections.insert("my-app".to_string(), "- [x] Fix bug".to_string());

        let rendered = render_sections(&sections);
        let reparsed = parse_sections(&rendered);

        assert_eq!(reparsed.len(), 2);
        assert!(reparsed.get("Global").unwrap().contains("Ship it"));
        assert!(reparsed.get("my-app").unwrap().contains("Fix bug"));
    }
}
