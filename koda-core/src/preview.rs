//! Pre-confirmation diff previews for destructive tool operations.
//!
//! Computes **structured** preview data before the user confirms an Edit,
//! Write, or Delete.  The actual rendering (colors, syntax highlighting)
//! is the client's responsibility — koda-core never emits ANSI codes.

use crate::tools::safe_resolve_path;
use std::path::Path;

/// Maximum diff lines before truncation.
const MAX_PREVIEW_LINES: usize = 40;

/// Maximum first-lines in Write previews.
const MAX_WRITE_PREVIEW_LINES: usize = 8;

// ── Data types ────────────────────────────────────────────────

/// Structured diff preview produced by the engine.
///
/// Clients render this however they want (ANSI, HTML, plain text, …).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind")]
pub enum DiffPreview {
    /// Replacement-based edit with context lines.
    Edit(EditPreview),
    /// New file creation.
    WriteNew(WritePreview),
    /// Overwrite of an existing file.
    WriteOverwrite(WriteOverwritePreview),
    /// Single file deletion.
    DeleteFile(DeleteFilePreview),
    /// Directory deletion.
    DeleteDir(DeleteDirPreview),
    /// Target file doesn't exist yet (Write will create it).
    FileNotYetExists,
    /// Target path not found.
    PathNotFound,
}

/// Preview of an Edit (replacement-based) operation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EditPreview {
    /// File path (as given in the tool args).
    pub path: String,
    /// Per-replacement data.
    pub replacements: Vec<ReplacementPreview>,
    /// Number of replacements omitted due to truncation.
    pub truncated_count: usize,
}

/// A single replacement within an Edit operation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReplacementPreview {
    /// 0-based index of this replacement.
    pub index: usize,
    /// Total number of replacements in the Edit call.
    pub total: usize,
    /// 1-based line number where `old_str` starts in the file.
    pub start_line: usize,
    /// Lines of the old (removed) text.
    pub old_lines: Vec<String>,
    /// Lines of the new (added) text.
    pub new_lines: Vec<String>,
}

/// Preview of a Write (new file) operation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WritePreview {
    /// Total line count of the new file.
    pub line_count: usize,
    /// Total byte count.
    pub byte_count: usize,
    /// First few lines (for preview display).
    pub first_lines: Vec<String>,
    /// Whether `first_lines` was truncated.
    pub truncated: bool,
}

/// Preview of a Write (overwrite existing file) operation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WriteOverwritePreview {
    /// Line count of the existing file.
    pub old_line_count: usize,
    /// Byte count of the existing file.
    pub old_byte_count: usize,
    /// Line count of the new content.
    pub new_line_count: usize,
    /// Byte count of the new content.
    pub new_byte_count: usize,
    /// First few lines of the new content.
    pub first_lines: Vec<String>,
    /// Whether `first_lines` was truncated.
    pub truncated: bool,
}

/// Preview of a single file deletion.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DeleteFilePreview {
    /// Line count of the file being deleted.
    pub line_count: usize,
    /// Byte count of the file being deleted.
    pub byte_count: u64,
}

/// Preview of a directory deletion.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DeleteDirPreview {
    /// Whether the deletion is recursive.
    pub recursive: bool,
}

// ── Compute ───────────────────────────────────────────────────

/// Compute a structured diff preview for a tool action.
///
/// Returns `None` for tools that don't need a preview.
pub async fn compute(
    tool_name: &str,
    args: &serde_json::Value,
    project_root: &Path,
) -> Option<DiffPreview> {
    match tool_name {
        "Edit" => preview_edit(args, project_root).await,
        "Write" => preview_write(args, project_root).await,
        "Delete" => preview_delete(args, project_root).await,
        _ => None,
    }
}

async fn preview_edit(args: &serde_json::Value, project_root: &Path) -> Option<DiffPreview> {
    let inner = args.get("payload").unwrap_or(args);
    let path_str = inner
        .get("path")
        .or(inner.get("file_path"))
        .and_then(|v| v.as_str())?;
    let replacements = inner.get("replacements")?.as_array()?;

    let resolved = safe_resolve_path(project_root, path_str).ok()?;
    if !resolved.exists() {
        return Some(DiffPreview::FileNotYetExists);
    }
    let file_content = tokio::fs::read_to_string(&resolved).await.ok()?;

    let mut previews = Vec::new();
    let mut total_lines = 0usize;
    let mut truncated_count = 0usize;

    for (i, replacement) in replacements.iter().enumerate() {
        let old_str = replacement.get("old_str")?.as_str()?;
        let new_str = replacement
            .get("new_str")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let start_line = file_content
            .find(old_str)
            .map(|byte_pos| {
                file_content[..byte_pos]
                    .bytes()
                    .filter(|&b| b == b'\n')
                    .count()
                    + 1
            })
            .unwrap_or(1);

        let old_lines: Vec<String> = old_str.lines().map(String::from).collect();
        let new_lines: Vec<String> = new_str.lines().map(String::from).collect();
        total_lines += old_lines.len() + new_lines.len();

        previews.push(ReplacementPreview {
            index: i,
            total: replacements.len(),
            start_line,
            old_lines,
            new_lines,
        });

        if total_lines > MAX_PREVIEW_LINES {
            truncated_count = replacements.len() - i - 1;
            break;
        }
    }

    Some(DiffPreview::Edit(EditPreview {
        path: path_str.to_string(),
        replacements: previews,
        truncated_count,
    }))
}

async fn preview_write(args: &serde_json::Value, project_root: &Path) -> Option<DiffPreview> {
    let inner = args.get("payload").unwrap_or(args);
    let path_str = inner
        .get("path")
        .or(inner.get("file_path"))
        .and_then(|v| v.as_str())?;
    let content = inner.get("content").and_then(|v| v.as_str())?;
    let resolved = safe_resolve_path(project_root, path_str).ok()?;

    let content_lines: Vec<&str> = content.lines().collect();
    let line_count = content_lines.len();
    let preview_count = line_count.min(MAX_WRITE_PREVIEW_LINES);
    let first_lines: Vec<String> = content_lines[..preview_count]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let truncated = line_count > MAX_WRITE_PREVIEW_LINES;

    if resolved.exists() {
        let existing = tokio::fs::read_to_string(&resolved).await.ok()?;
        Some(DiffPreview::WriteOverwrite(WriteOverwritePreview {
            old_line_count: existing.lines().count(),
            old_byte_count: existing.len(),
            new_line_count: line_count,
            new_byte_count: content.len(),
            first_lines,
            truncated,
        }))
    } else {
        Some(DiffPreview::WriteNew(WritePreview {
            line_count,
            byte_count: content.len(),
            first_lines,
            truncated,
        }))
    }
}

async fn preview_delete(args: &serde_json::Value, project_root: &Path) -> Option<DiffPreview> {
    let inner = args.get("payload").unwrap_or(args);
    let path_str = inner
        .get("path")
        .or(inner.get("file_path"))
        .and_then(|v| v.as_str())?;
    let resolved = safe_resolve_path(project_root, path_str).ok()?;

    if !resolved.exists() {
        return Some(DiffPreview::PathNotFound);
    }

    let meta = tokio::fs::metadata(&resolved).await.ok()?;
    if meta.is_file() {
        let line_count = tokio::fs::read_to_string(&resolved)
            .await
            .map(|c| c.lines().count())
            .unwrap_or(0);
        Some(DiffPreview::DeleteFile(DeleteFilePreview {
            line_count,
            byte_count: meta.len(),
        }))
    } else if meta.is_dir() {
        let recursive = args
            .get("recursive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        Some(DiffPreview::DeleteDir(DeleteDirPreview { recursive }))
    } else {
        None
    }
}

// ── Tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_preview_edit_replacements() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("test.rs");
        std::fs::write(&file, "fn main() {\n    println!(\"hello\");\n}\n").unwrap();

        let args = json!({
            "path": file.to_str().unwrap(),
            "replacements": [{
                "old_str": "println!(\"hello\");",
                "new_str": "println!(\"world\");"
            }]
        });

        let preview = compute("Edit", &args, tmp.path()).await;
        let preview = preview.expect("should produce a preview");
        match preview {
            DiffPreview::Edit(edit) => {
                assert_eq!(edit.replacements.len(), 1);
                let r = &edit.replacements[0];
                assert_eq!(r.start_line, 2);
                assert_eq!(r.old_lines, vec!["println!(\"hello\");"]);
                assert_eq!(r.new_lines, vec!["println!(\"world\");"]);
            }
            other => panic!("expected Edit preview, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_preview_write_new_file() {
        let tmp = TempDir::new().unwrap();
        let args = json!({
            "path": "new_file.rs",
            "content": "fn main() {}\n"
        });

        let preview = compute("Write", &args, tmp.path()).await;
        assert!(matches!(preview, Some(DiffPreview::WriteNew(_))));
    }

    #[tokio::test]
    async fn test_preview_write_overwrite() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("existing.rs");
        std::fs::write(&file, "old content\n").unwrap();

        let args = json!({
            "path": file.to_str().unwrap(),
            "content": "new content\nline 2\n"
        });

        let preview = compute("Write", &args, tmp.path()).await;
        assert!(matches!(preview, Some(DiffPreview::WriteOverwrite(_))));
    }

    #[tokio::test]
    async fn test_preview_delete_file() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("doomed.rs");
        std::fs::write(&file, "goodbye\n").unwrap();

        let args = json!({
            "path": file.to_str().unwrap()
        });

        let preview = compute("Delete", &args, tmp.path()).await;
        assert!(matches!(preview, Some(DiffPreview::DeleteFile(_))));
    }

    #[tokio::test]
    async fn test_preview_read_returns_none() {
        let tmp = TempDir::new().unwrap();
        let args = json!({"path": "anything.rs"});
        let preview = compute("Read", &args, tmp.path()).await;
        assert!(preview.is_none());
    }
}
