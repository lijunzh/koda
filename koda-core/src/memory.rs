//! Semantic memory: project context injected into the system prompt.
//!
//! Memory is stored as human-readable Markdown, loaded from two tiers:
//!
//! **Global** (`~/.config/koda/memory.md`):
//!   User-wide preferences and conventions that apply to all projects.
//!
//! **Project-local** (first match wins):
//!   1. `MEMORY.md`  — Koda native
//!   2. `CLAUDE.md`  — Claude Code compatibility
//!   3. `AGENTS.md`  — Code Puppy compatibility
//!
//! Both tiers are concatenated and injected into the system prompt.
//! When Koda writes (auto-memory), it always writes to `MEMORY.md`.

use anyhow::Result;
use std::path::{Path, PathBuf};

/// Project-local memory files, checked in priority order.
const PROJECT_MEMORY_FILES: &[&str] = &["MEMORY.md", "CLAUDE.md", "AGENTS.md"];

/// Global memory filename inside `~/.config/koda/`.
const GLOBAL_MEMORY_FILE: &str = "memory.md";

/// Koda's native project memory filename (used for writes).
const KODA_MEMORY_FILE: &str = "MEMORY.md";

/// Load memory from both global and project-local sources.
///
/// Returns the combined content (global first, then project-local).
/// Returns an empty string if no memory files exist.
pub fn load(project_root: &Path) -> Result<String> {
    let mut parts: Vec<String> = Vec::new();

    // 1. Global memory (~/.config/koda/memory.md)
    if let Some(global) = load_global()? {
        tracing::info!("Loaded global memory ({} bytes)", global.len());
        parts.push(global);
    }

    // 2. Project-local memory (first match wins)
    if let Some((filename, content)) = load_project(project_root)? {
        tracing::info!(
            "Loaded project memory from {filename} ({} bytes)",
            content.len()
        );
        parts.push(content);
    } else {
        tracing::info!("No project memory file found");
    }

    Ok(parts.join("\n\n"))
}

/// Write an entry to the project's memory file.
///
/// If the entry starts with a `## Heading`, and a section with that
/// heading already exists in the file, the existing section is
/// **replaced** (updated in place). Otherwise the entry is appended.
///
/// Always targets the active memory file (or `MEMORY.md` if none exists).
pub fn append(project_root: &Path, entry: &str) -> Result<()> {
    let target_filename =
        active_project_file(project_root).unwrap_or_else(|| KODA_MEMORY_FILE.to_string());
    let path = project_root.join(&target_filename);
    write_or_replace_section(&path, entry)?;
    tracing::info!("Wrote to {target_filename}: {entry}");
    Ok(())
}

/// Return which project memory file is active (for display purposes).
pub fn active_project_file(project_root: &Path) -> Option<String> {
    for filename in PROJECT_MEMORY_FILES {
        if project_root.join(filename).exists() {
            return Some(filename.to_string());
        }
    }
    None
}

/// Write an entry to the global memory file (~/.config/koda/memory.md).
///
/// If the entry starts with a `## Heading` that already exists, the
/// section is replaced. Otherwise the entry is appended.
pub fn append_global(entry: &str) -> Result<()> {
    let path = global_memory_path()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory for global memory"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_or_replace_section(&path, entry)?;
    tracing::info!("Wrote to global memory: {entry}");
    Ok(())
}

// ── Internal helpers ──────────────────────────────────────────────────────

/// Write an entry to a memory file, merging by `## Heading` if possible.
///
/// If `entry` starts with `## <heading>`, we look for an existing section
/// with the same heading. If found, the old section (heading through to
/// the next `##` heading or EOF) is replaced with `entry`. If not found
/// (or `entry` has no heading), the entry is appended.
fn write_or_replace_section(path: &Path, entry: &str) -> Result<()> {
    let heading = extract_heading(entry);
    let existing = if path.exists() {
        std::fs::read_to_string(path)?
    } else {
        String::new()
    };

    let new_content = match heading {
        Some(ref h) if section_exists(&existing, h) => replace_section(&existing, h, entry),
        _ => {
            // No heading or heading not found → append
            let mut buf = existing;
            if !buf.is_empty() && !buf.ends_with('\n') {
                buf.push('\n');
            }
            buf.push_str(&format!("\n- {entry}"));
            buf.push('\n');
            buf
        }
    };

    std::fs::write(path, new_content)?;
    Ok(())
}

/// Extract a `## Heading` from the first line of an entry.
fn extract_heading(entry: &str) -> Option<String> {
    let first_line = entry.lines().next()?.trim();
    if first_line.starts_with("## ") {
        Some(first_line.to_string())
    } else {
        None
    }
}

/// Check if a `## Heading` section already exists in the content.
fn section_exists(content: &str, heading: &str) -> bool {
    content.lines().any(|line| line.trim() == heading)
}

/// Replace a `## Heading` section with new content.
///
/// The section spans from the heading line to (but not including)
/// the next `## ` heading or EOF.
fn replace_section(content: &str, heading: &str, replacement: &str) -> String {
    let mut result = String::new();
    let mut in_target_section = false;
    let mut replaced = false;

    for line in content.lines() {
        let trimmed = line.trim();

        if trimmed == heading && !replaced {
            // Start of the section we want to replace
            in_target_section = true;
            // Emit the replacement content
            result.push_str(replacement);
            if !replacement.ends_with('\n') {
                result.push('\n');
            }
            replaced = true;
            continue;
        }

        if in_target_section {
            // Check if we've hit the next section heading
            if trimmed.starts_with("## ") {
                in_target_section = false;
                result.push_str(line);
                result.push('\n');
            }
            // else: skip old section content
            continue;
        }

        result.push_str(line);
        result.push('\n');
    }

    result
}

/// Load global memory from `~/.config/koda/memory.md`.
fn load_global() -> Result<Option<String>> {
    let path = global_memory_path();
    match path {
        Some(p) if p.exists() => {
            let content = std::fs::read_to_string(&p)?;
            if content.trim().is_empty() {
                Ok(None)
            } else {
                Ok(Some(content))
            }
        }
        _ => Ok(None),
    }
}

/// Load project-local memory (first matching file wins).
fn load_project(project_root: &Path) -> Result<Option<(String, String)>> {
    for filename in PROJECT_MEMORY_FILES {
        let path = project_root.join(filename);
        if path.exists() {
            let content = std::fs::read_to_string(&path)?;
            if !content.trim().is_empty() {
                return Ok(Some((filename.to_string(), content)));
            }
        }
    }
    Ok(None)
}

/// Path to the global memory file.
fn global_memory_path() -> Option<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("koda")
            .join(GLOBAL_MEMORY_FILE),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_load_missing_memory_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let content = load(tmp.path()).unwrap();
        assert!(content.is_empty());
    }

    #[test]
    fn test_load_memory_md() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("MEMORY.md"), "# Project notes\n- Uses Rust").unwrap();
        let content = load(tmp.path()).unwrap();
        assert!(content.contains("Uses Rust"));
    }

    #[test]
    fn test_load_claude_md_compat() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("CLAUDE.md"), "# Claude rules\n- Be concise").unwrap();
        let content = load(tmp.path()).unwrap();
        assert!(content.contains("Be concise"));
    }

    #[test]
    fn test_load_agents_md_compat() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("AGENTS.md"), "# Agent rules\n- DRY").unwrap();
        let content = load(tmp.path()).unwrap();
        assert!(content.contains("DRY"));
    }

    #[test]
    fn test_memory_md_takes_priority_over_claude_md() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("MEMORY.md"), "koda-memory").unwrap();
        std::fs::write(tmp.path().join("CLAUDE.md"), "claude-rules").unwrap();
        let content = load(tmp.path()).unwrap();
        assert!(content.contains("koda-memory"));
        assert!(!content.contains("claude-rules"));
    }

    #[test]
    fn test_claude_md_takes_priority_over_agents_md() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("CLAUDE.md"), "claude-rules").unwrap();
        std::fs::write(tmp.path().join("AGENTS.md"), "puppy-rules").unwrap();
        let content = load(tmp.path()).unwrap();
        assert!(content.contains("claude-rules"));
        assert!(!content.contains("puppy-rules"));
    }

    #[test]
    fn test_append_creates_and_appends() {
        let tmp = TempDir::new().unwrap();
        append(tmp.path(), "first entry").unwrap();
        append(tmp.path(), "second entry").unwrap();

        let content = load(tmp.path()).unwrap();
        assert!(content.contains("first entry"));
        assert!(content.contains("second entry"));
    }

    #[test]
    fn test_append_writes_to_active_file() {
        let tmp = TempDir::new().unwrap();
        // If CLAUDE.md exists, append writes directly to CLAUDE.md
        std::fs::write(tmp.path().join("CLAUDE.md"), "existing claude rules").unwrap();
        append(tmp.path(), "new koda insight").unwrap();

        // It should NOT create MEMORY.md
        assert!(!tmp.path().join("MEMORY.md").exists());

        // It SHOULD append to CLAUDE.md
        let memory = std::fs::read_to_string(tmp.path().join("CLAUDE.md")).unwrap();
        assert!(memory.contains("new koda insight"));
    }

    #[test]
    fn test_active_project_file() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(active_project_file(tmp.path()), None);

        std::fs::write(tmp.path().join("AGENTS.md"), "rules").unwrap();
        assert_eq!(
            active_project_file(tmp.path()),
            Some("AGENTS.md".to_string())
        );

        std::fs::write(tmp.path().join("MEMORY.md"), "memory").unwrap();
        assert_eq!(
            active_project_file(tmp.path()),
            Some("MEMORY.md".to_string())
        );
    }

    // ── Section merge tests (#519) ──

    #[test]
    fn test_extract_heading() {
        assert_eq!(
            extract_heading("## Workflow Preferences\n- item"),
            Some("## Workflow Preferences".to_string())
        );
        assert_eq!(extract_heading("just a plain note"), None);
        assert_eq!(extract_heading("# Top level heading"), None); // only ## is matched
        assert_eq!(extract_heading(""), None);
    }

    #[test]
    fn test_section_exists() {
        let content = "# Title\n## Workflow Preferences\n- item1\n## Other\n- item2";
        assert!(section_exists(content, "## Workflow Preferences"));
        assert!(section_exists(content, "## Other"));
        assert!(!section_exists(content, "## Missing"));
    }

    #[test]
    fn test_replace_section() {
        let content = "# Title\n## Workflow Preferences\n- old item1\n- old item2\n## Other Section\n- keep this\n";
        let replacement = "## Workflow Preferences\n- new item1\n- new item2\n- new item3";
        let result = replace_section(content, "## Workflow Preferences", replacement);
        assert!(result.contains("- new item1"), "Should contain new content");
        assert!(result.contains("- new item3"), "Should contain new content");
        assert!(
            !result.contains("- old item1"),
            "Should not contain old content"
        );
        assert!(
            result.contains("## Other Section"),
            "Should preserve other sections"
        );
        assert!(
            result.contains("- keep this"),
            "Should preserve other section content"
        );
    }

    #[test]
    fn test_replace_section_at_end() {
        let content = "## First\n- a\n## Second\n- old\n";
        let replacement = "## Second\n- new";
        let result = replace_section(content, "## Second", replacement);
        assert!(result.contains("## First"), "Should preserve first section");
        assert!(
            result.contains("- a"),
            "Should preserve first section content"
        );
        assert!(result.contains("- new"), "Should contain replacement");
        assert!(!result.contains("- old"), "Should not contain old content");
    }

    #[test]
    fn test_append_merges_existing_section() {
        let tmp = TempDir::new().unwrap();
        let existing = "## Workflow Preferences\n- old item\n";
        std::fs::write(tmp.path().join("MEMORY.md"), existing).unwrap();

        append(
            tmp.path(),
            "## Workflow Preferences\n- updated item\n- new item",
        )
        .unwrap();

        let content = std::fs::read_to_string(tmp.path().join("MEMORY.md")).unwrap();
        assert!(
            content.contains("- updated item"),
            "Should contain new content"
        );
        assert!(content.contains("- new item"), "Should contain new content");
        assert!(
            !content.contains("- old item"),
            "Should not contain old content"
        );
        // Should only have one copy of the heading
        assert_eq!(
            content.matches("## Workflow Preferences").count(),
            1,
            "Should have exactly one copy of the heading"
        );
    }

    #[test]
    fn test_append_new_section_still_appends() {
        let tmp = TempDir::new().unwrap();
        let existing = "## Existing Section\n- item\n";
        std::fs::write(tmp.path().join("MEMORY.md"), existing).unwrap();

        append(tmp.path(), "## New Section\n- new item").unwrap();

        let content = std::fs::read_to_string(tmp.path().join("MEMORY.md")).unwrap();
        assert!(content.contains("## Existing Section"));
        assert!(content.contains("## New Section"));
        assert!(content.contains("- new item"));
    }

    #[test]
    fn test_append_plain_entry_still_appends() {
        let tmp = TempDir::new().unwrap();
        append(tmp.path(), "just a plain note").unwrap();
        append(tmp.path(), "another plain note").unwrap();

        let content = std::fs::read_to_string(tmp.path().join("MEMORY.md")).unwrap();
        assert!(content.contains("just a plain note"));
        assert!(content.contains("another plain note"));
    }
}
