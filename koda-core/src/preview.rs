//! Pre-confirmation diff previews for destructive tool operations.
//!
//! Computes **structured** preview data before the user confirms an Edit,
//! Write, or Delete.  The actual rendering (colors, syntax highlighting)
//! is the client's responsibility — koda-core never emits ANSI codes.
//!
//! Edit and Write-overwrite produce a proper unified diff (via `similar`)
//! with context lines and hunk headers — the same information you'd see
//! in `git diff` output.

use crate::tools::safe_resolve_path;
use similar::{ChangeTag, TextDiff};
use std::path::Path;

/// Number of context lines around each change (like `diff -U3`).
const CONTEXT_LINES: usize = 3;

/// Maximum total diff lines before we truncate.
const MAX_DIFF_LINES: usize = 120;

/// Maximum lines shown for a new-file preview.
const MAX_WRITE_NEW_LINES: usize = 60;

// ── Data types ────────────────────────────────────────────────

/// Structured diff preview produced by the engine.
///
/// Clients render this however they want (ratatui, HTML, plain text, …).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind")]
pub enum DiffPreview {
    /// Unified diff with context lines and hunk headers.
    /// Used for both Edit and Write-overwrite.
    UnifiedDiff(UnifiedDiffPreview),
    /// New file creation.
    WriteNew(WriteNewPreview),
    /// Single file deletion.
    DeleteFile(DeleteFilePreview),
    /// Directory deletion.
    DeleteDir(DeleteDirPreview),
    /// Target file doesn't exist yet (for Edit on missing file).
    FileNotYetExists,
    /// Target path not found.
    PathNotFound,
}

/// A proper unified diff between old and new file content.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UnifiedDiffPreview {
    /// File path (as given in the tool args).
    pub path: String,
    /// Full old file content (for syntax-context highlighting).
    pub old_content: String,
    /// Full new file content (for syntax-context highlighting).
    pub new_content: String,
    /// Diff hunks with context.
    pub hunks: Vec<DiffHunk>,
    /// Whether hunks were truncated due to size limits.
    pub truncated: bool,
}

/// A single hunk in a unified diff.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DiffHunk {
    /// 1-based starting line in the old file.
    pub old_start: usize,
    /// Number of lines from the old file in this hunk.
    pub old_count: usize,
    /// 1-based starting line in the new file.
    pub new_start: usize,
    /// Number of lines from the new file in this hunk.
    pub new_count: usize,
    /// The lines in this hunk (context + insertions + deletions).
    pub lines: Vec<DiffLine>,
}

/// A single line within a diff hunk.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DiffLine {
    /// Whether this line is context, inserted, or deleted.
    pub tag: DiffTag,
    /// The line content (without trailing newline).
    pub content: String,
    /// Line number in the old file (for Context/Delete lines).
    pub old_line: Option<usize>,
    /// Line number in the new file (for Context/Insert lines).
    pub new_line: Option<usize>,
}

/// The type of a diff line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DiffTag {
    /// Unchanged context line.
    Context,
    /// Line was added.
    Insert,
    /// Line was removed.
    Delete,
}

/// Preview of a Write (new file) operation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WriteNewPreview {
    /// File path.
    pub path: String,
    /// Total line count of the new file.
    pub line_count: usize,
    /// Total byte count.
    pub byte_count: usize,
    /// First lines (for preview display).
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

/// Build a unified diff from old and new content.
///
/// Shared by Edit and Write-overwrite paths.
fn build_unified_diff(
    path: &str,
    old_content: &str,
    new_content: &str,
) -> UnifiedDiffPreview {
    let diff = TextDiff::from_lines(old_content, new_content);
    let mut hunks = Vec::new();
    let mut total_lines = 0usize;
    let mut truncated = false;

    for group in diff.grouped_ops(CONTEXT_LINES) {
        let mut hunk_lines = Vec::new();
        let mut old_start = 0;
        let mut new_start = 0;
        let mut old_count = 0;
        let mut new_count = 0;
        let mut first = true;

        for op in &group {
            if first {
                old_start = op.old_range().start + 1; // 1-based
                new_start = op.new_range().start + 1;
                first = false;
            }

            let old_lines = diff.old_slices();
            let new_lines = diff.new_slices();

            for change in diff.iter_changes(op) {
                let content = change.value().trim_end_matches('\n').to_string();
                let (tag, old_line, new_line) = match change.tag() {
                    ChangeTag::Equal => {
                        old_count += 1;
                        new_count += 1;
                        (
                            DiffTag::Context,
                            change.old_index().map(|i| i + 1),
                            change.new_index().map(|i| i + 1),
                        )
                    }
                    ChangeTag::Delete => {
                        old_count += 1;
                        (
                            DiffTag::Delete,
                            change.old_index().map(|i| i + 1),
                            None,
                        )
                    }
                    ChangeTag::Insert => {
                        new_count += 1;
                        (
                            DiffTag::Insert,
                            None,
                            change.new_index().map(|i| i + 1),
                        )
                    }
                };

                hunk_lines.push(DiffLine {
                    tag,
                    content,
                    old_line,
                    new_line,
                });
            }

            // Suppress unused-variable warnings — we use iter_changes instead.
            let _ = (old_lines, new_lines);
        }

        total_lines += hunk_lines.len();
        hunks.push(DiffHunk {
            old_start,
            old_count,
            new_start,
            new_count,
            lines: hunk_lines,
        });

        if total_lines > MAX_DIFF_LINES {
            truncated = true;
            break;
        }
    }

    UnifiedDiffPreview {
        path: path.to_string(),
        old_content: old_content.to_string(),
        new_content: new_content.to_string(),
        hunks,
        truncated,
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
    let old_content = tokio::fs::read_to_string(&resolved).await.ok()?;

    // Apply all replacements sequentially to produce new_content
    let mut new_content = old_content.clone();
    for replacement in replacements {
        let old_str = replacement.get("old_str")?.as_str()?;
        let new_str = replacement
            .get("new_str")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        // Replace first occurrence only (matches Edit tool behavior)
        if let Some(pos) = new_content.find(old_str) {
            new_content.replace_range(pos..pos + old_str.len(), new_str);
        }
    }

    let preview = build_unified_diff(path_str, &old_content, &new_content);
    Some(DiffPreview::UnifiedDiff(preview))
}

async fn preview_write(args: &serde_json::Value, project_root: &Path) -> Option<DiffPreview> {
    let inner = args.get("payload").unwrap_or(args);
    let path_str = inner
        .get("path")
        .or(inner.get("file_path"))
        .and_then(|v| v.as_str())?;
    let content = inner.get("content").and_then(|v| v.as_str())?;
    let resolved = safe_resolve_path(project_root, path_str).ok()?;

    if resolved.exists() {
        // Overwrite → produce a real unified diff
        let old_content = tokio::fs::read_to_string(&resolved).await.ok()?;
        let preview = build_unified_diff(path_str, &old_content, content);
        Some(DiffPreview::UnifiedDiff(preview))
    } else {
        // New file → show content preview
        let content_lines: Vec<&str> = content.lines().collect();
        let line_count = content_lines.len();
        let preview_count = line_count.min(MAX_WRITE_NEW_LINES);
        let first_lines: Vec<String> = content_lines[..preview_count]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let truncated = line_count > MAX_WRITE_NEW_LINES;

        Some(DiffPreview::WriteNew(WriteNewPreview {
            path: path_str.to_string(),
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
    async fn test_edit_produces_unified_diff() {
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

        let preview = compute("Edit", &args, tmp.path()).await.unwrap();
        match preview {
            DiffPreview::UnifiedDiff(diff) => {
                assert_eq!(diff.hunks.len(), 1);
                let hunk = &diff.hunks[0];
                // Should have context + delete + insert
                let tags: Vec<_> = hunk.lines.iter().map(|l| l.tag).collect();
                assert!(tags.contains(&DiffTag::Delete));
                assert!(tags.contains(&DiffTag::Insert));
                assert!(tags.contains(&DiffTag::Context));
                // Deleted line should contain "hello"
                let del = hunk.lines.iter().find(|l| l.tag == DiffTag::Delete).unwrap();
                assert!(del.content.contains("hello"));
                // Inserted line should contain "world"
                let ins = hunk.lines.iter().find(|l| l.tag == DiffTag::Insert).unwrap();
                assert!(ins.content.contains("world"));
            }
            other => panic!("expected UnifiedDiff, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_edit_multiple_replacements() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("test.rs");
        // 20 lines — changes at line 2 and 19 are >6 lines apart,
        // so with 3 context lines they produce separate hunks.
        let content: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&file, &content).unwrap();

        let args = json!({
            "path": file.to_str().unwrap(),
            "replacements": [
                { "old_str": "line 2", "new_str": "LINE TWO" },
                { "old_str": "line 19", "new_str": "LINE NINETEEN" }
            ]
        });

        let preview = compute("Edit", &args, tmp.path()).await.unwrap();
        match preview {
            DiffPreview::UnifiedDiff(diff) => {
                assert_eq!(diff.hunks.len(), 2, "expected 2 hunks, got {:?}", diff.hunks);
            }
            other => panic!("expected UnifiedDiff, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_write_new_file() {
        let tmp = TempDir::new().unwrap();
        let args = json!({
            "path": "new_file.rs",
            "content": "fn main() {}\n"
        });

        let preview = compute("Write", &args, tmp.path()).await.unwrap();
        assert!(matches!(preview, DiffPreview::WriteNew(_)));
    }

    #[tokio::test]
    async fn test_write_overwrite_produces_unified_diff() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("existing.rs");
        std::fs::write(&file, "old content\n").unwrap();

        let args = json!({
            "path": file.to_str().unwrap(),
            "content": "new content\nline 2\n"
        });

        let preview = compute("Write", &args, tmp.path()).await.unwrap();
        match preview {
            DiffPreview::UnifiedDiff(diff) => {
                assert!(!diff.hunks.is_empty());
            }
            other => panic!("expected UnifiedDiff for overwrite, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_delete_file() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("doomed.rs");
        std::fs::write(&file, "goodbye\n").unwrap();

        let args = json!({ "path": file.to_str().unwrap() });
        let preview = compute("Delete", &args, tmp.path()).await.unwrap();
        assert!(matches!(preview, DiffPreview::DeleteFile(_)));
    }

    #[tokio::test]
    async fn test_unknown_tool_returns_none() {
        let tmp = TempDir::new().unwrap();
        let args = json!({"path": "anything.rs"});
        let preview = compute("Read", &args, tmp.path()).await;
        assert!(preview.is_none());
    }

    #[tokio::test]
    async fn test_edit_missing_file() {
        let tmp = TempDir::new().unwrap();
        let args = json!({
            "path": "nonexistent.rs",
            "replacements": [{ "old_str": "x", "new_str": "y" }]
        });
        let preview = compute("Edit", &args, tmp.path()).await.unwrap();
        assert!(matches!(preview, DiffPreview::FileNotYetExists));
    }

    #[tokio::test]
    async fn test_unified_diff_has_line_numbers() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("test.txt");
        std::fs::write(&file, "a\nb\nc\nd\ne\n").unwrap();

        let args = json!({
            "path": file.to_str().unwrap(),
            "replacements": [{ "old_str": "c", "new_str": "C" }]
        });

        let preview = compute("Edit", &args, tmp.path()).await.unwrap();
        match preview {
            DiffPreview::UnifiedDiff(diff) => {
                let hunk = &diff.hunks[0];
                // Every line should have at least one line number
                for line in &hunk.lines {
                    assert!(
                        line.old_line.is_some() || line.new_line.is_some(),
                        "line should have a line number: {line:?}"
                    );
                }
            }
            other => panic!("expected UnifiedDiff, got {other:?}"),
        }
    }
}
