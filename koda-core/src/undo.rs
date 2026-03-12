//! Undo stack for file mutations.
//!
//! Snapshots file contents before Write/Edit/Delete tool execution.
//! Each turn's mutations are grouped into a single undo entry.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A snapshot of file states before a turn's mutations.
#[derive(Debug, Clone)]
pub struct UndoEntry {
    /// Map of absolute path → previous content (None = file didn't exist).
    files: HashMap<PathBuf, Option<Vec<u8>>>,
}

/// Stack of undo entries, one per turn.
#[derive(Debug, Default)]
pub struct UndoStack {
    entries: Vec<UndoEntry>,
    /// Accumulates snapshots for the current (in-progress) turn.
    pending: HashMap<PathBuf, Option<Vec<u8>>>,
}

impl UndoStack {
    /// Create an empty undo stack.
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot a file before mutation. Call before Write/Edit/Delete.
    ///
    /// Only snapshots the first time per file per turn (preserves original state).
    pub fn snapshot(&mut self, path: &Path) {
        let abs = match std::fs::canonicalize(path) {
            Ok(p) => p,
            Err(_) => path.to_path_buf(), // File doesn't exist yet
        };

        // Only snapshot the first mutation per file per turn
        if self.pending.contains_key(&abs) {
            return;
        }

        let content = std::fs::read(&abs).ok();
        self.pending.insert(abs, content);
    }

    /// Finalize the current turn's snapshots into an undo entry.
    ///
    /// Call at the end of each inference turn (after all tool calls complete).
    /// Does nothing if no mutations were snapshotted.
    pub fn commit_turn(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        self.entries.push(UndoEntry {
            files: std::mem::take(&mut self.pending),
        });
    }

    /// Undo the last turn's file mutations.
    ///
    /// Returns a summary of what was restored, or None if nothing to undo.
    pub fn undo(&mut self) -> Option<String> {
        let entry = self.entries.pop()?;
        let mut restored = Vec::new();

        for (path, original) in &entry.files {
            match original {
                Some(content) => {
                    if let Err(e) = std::fs::write(path, content) {
                        restored.push(format!("  ❌ {} (write failed: {e})", path.display()));
                    } else {
                        restored.push(format!("  ↩ {} (restored)", path.display()));
                    }
                }
                None => {
                    // File didn't exist before — delete it
                    if let Err(e) = std::fs::remove_file(path) {
                        restored.push(format!("  ❌ {} (delete failed: {e})", path.display()));
                    } else {
                        restored.push(format!(
                            "  ↩ {} (removed — was newly created)",
                            path.display()
                        ));
                    }
                }
            }
        }

        restored.sort();
        Some(format!(
            "Undid {} file(s) from last turn:\n{}",
            entry.files.len(),
            restored.join("\n")
        ))
    }

    /// How many turns can be undone.
    pub fn depth(&self) -> usize {
        self.entries.len()
    }
}

/// Check if a tool name is a file-mutating tool that should be snapshotted.
pub fn is_mutating_tool(name: &str) -> bool {
    matches!(name, "Write" | "Edit" | "Delete" | "Overwrite")
}

/// Extract the target file path from tool arguments.
pub fn extract_file_path(name: &str, args: &serde_json::Value) -> Option<String> {
    match name {
        "Write" | "Edit" | "Delete" | "Overwrite" => args
            .get("file_path")
            .or_else(|| args.get("path"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (UndoStack, TempDir) {
        (UndoStack::new(), TempDir::new().unwrap())
    }

    #[test]
    fn test_undo_restores_overwritten_file() {
        let (mut stack, tmp) = setup();
        let path = tmp.path().join("test.txt");
        std::fs::write(&path, "original").unwrap();

        // Snapshot before mutation
        stack.snapshot(&path);
        std::fs::write(&path, "modified").unwrap();
        stack.commit_turn();

        // Undo
        let result = stack.undo();
        assert!(result.is_some());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
    }

    #[test]
    fn test_undo_removes_newly_created_file() {
        let (mut stack, tmp) = setup();
        let path = tmp.path().join("new.txt");

        // Snapshot before creation (file doesn't exist)
        stack.snapshot(&path);
        std::fs::write(&path, "created").unwrap();
        stack.commit_turn();

        // Undo
        stack.undo();
        assert!(!path.exists());
    }

    #[test]
    fn test_undo_empty_stack() {
        let mut stack = UndoStack::new();
        assert!(stack.undo().is_none());
    }

    #[test]
    fn test_multiple_files_per_turn() {
        let (mut stack, tmp) = setup();
        let a = tmp.path().join("a.txt");
        let b = tmp.path().join("b.txt");
        std::fs::write(&a, "aaa").unwrap();
        std::fs::write(&b, "bbb").unwrap();

        stack.snapshot(&a);
        stack.snapshot(&b);
        std::fs::write(&a, "AAA").unwrap();
        std::fs::write(&b, "BBB").unwrap();
        stack.commit_turn();

        stack.undo();
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "aaa");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "bbb");
    }

    #[test]
    fn test_only_first_snapshot_per_file_per_turn() {
        let (mut stack, tmp) = setup();
        let path = tmp.path().join("test.txt");
        std::fs::write(&path, "v1").unwrap();

        stack.snapshot(&path); // Captures "v1"
        std::fs::write(&path, "v2").unwrap();
        stack.snapshot(&path); // Should NOT overwrite snapshot
        std::fs::write(&path, "v3").unwrap();
        stack.commit_turn();

        stack.undo();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v1");
    }

    #[test]
    fn test_multi_turn_undo() {
        let (mut stack, tmp) = setup();
        let path = tmp.path().join("test.txt");
        std::fs::write(&path, "v1").unwrap();

        // Turn 1
        stack.snapshot(&path);
        std::fs::write(&path, "v2").unwrap();
        stack.commit_turn();

        // Turn 2
        stack.snapshot(&path);
        std::fs::write(&path, "v3").unwrap();
        stack.commit_turn();

        assert_eq!(stack.depth(), 2);

        // Undo turn 2
        stack.undo();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v2");

        // Undo turn 1
        stack.undo();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v1");
    }

    #[test]
    fn test_is_mutating_tool() {
        assert!(is_mutating_tool("Write"));
        assert!(is_mutating_tool("Edit"));
        assert!(is_mutating_tool("Delete"));
        assert!(!is_mutating_tool("Read"));
        assert!(!is_mutating_tool("Grep"));
        assert!(!is_mutating_tool("Bash"));
    }

    #[test]
    fn test_extract_file_path() {
        let args = serde_json::json!({"file_path": "src/main.rs"});
        assert_eq!(
            extract_file_path("Write", &args),
            Some("src/main.rs".into())
        );
        assert_eq!(extract_file_path("Read", &args), None);
    }

    #[test]
    fn test_no_commit_if_no_snapshots() {
        let mut stack = UndoStack::new();
        stack.commit_turn(); // Nothing pending
        assert_eq!(stack.depth(), 0);
    }
}
