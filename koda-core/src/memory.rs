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

/// Append a new entry to the project's MEMORY.md.
///
/// Always writes to `MEMORY.md` (Koda native), even if the project
/// currently uses `CLAUDE.md` or `AGENTS.md` for reading.
pub fn append(project_root: &Path, entry: &str) -> Result<()> {
    use std::io::Write;

    // Determine the active file, or default to MEMORY.md
    let target_filename =
        active_project_file(project_root).unwrap_or_else(|| KODA_MEMORY_FILE.to_string());

    let path = project_root.join(&target_filename);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(file, "\n- {entry}")?;
    tracing::info!("Appended to {target_filename}: {entry}");
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

/// Append a new entry to the global memory file (~/.config/koda/memory.md).
pub fn append_global(entry: &str) -> Result<()> {
    use std::io::Write;
    let path = global_memory_path()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory for global memory"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(file, "\n- {entry}")?;
    tracing::info!("Appended to global memory: {entry}");
    Ok(())
}

// ── Internal helpers ──────────────────────────────────────────────────────

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
}
