//! Pre-confirmation diff previews for destructive tool operations.
//!
//! Computes a colored diff preview before the user confirms an Edit or Write,
//! so they can make an informed decision instead of approving blind.

use crate::tools::safe_resolve_path;
use std::path::Path;

const GREEN: &str = "\x1b[42m"; // green background
const RED: &str = "\x1b[41m"; // red background
const DIM: &str = "\x1b[90m";
const RESET: &str = "\x1b[0m";

/// Maximum diff lines to show before truncating.
const MAX_PREVIEW_LINES: usize = 40;

/// Compute a diff preview for a tool action, if applicable.
///
/// Returns `None` for tools that don't need a preview (Read, List, Grep, etc.).
pub async fn compute(
    tool_name: &str,
    args: &serde_json::Value,
    project_root: &Path,
) -> Option<String> {
    match tool_name {
        "Edit" => preview_edit(args, project_root).await,
        "Write" => preview_write(args, project_root).await,
        "Delete" => preview_delete(args, project_root).await,
        _ => None,
    }
}

/// Preview for Edit tool: show each replacement as old (red) → new (green) with line numbers.
async fn preview_edit(args: &serde_json::Value, project_root: &Path) -> Option<String> {
    // Handle both flat args {"path", "replacements"} and nested {"payload": {"file_path", "replacements"}}
    let inner = args.get("payload").unwrap_or(args);

    let path_str = inner
        .get("path")
        .or(inner.get("file_path"))
        .and_then(|v| v.as_str())?;
    let replacements = inner.get("replacements")?.as_array()?;

    // Verify file exists and read content for line number computation
    let resolved = safe_resolve_path(project_root, path_str).ok()?;
    if !resolved.exists() {
        return Some(format!("{DIM}(file does not exist yet){RESET}"));
    }
    let file_content = tokio::fs::read_to_string(&resolved).await.ok()?;

    let mut lines = Vec::new();
    let mut total_lines = 0usize;

    for (i, replacement) in replacements.iter().enumerate() {
        let old_str = replacement.get("old_str")?.as_str()?;
        let new_str = replacement
            .get("new_str")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        // Find the 1-based starting line number of old_str in the file.
        // Count newlines before the match position (not lines(), which
        // includes the partial line at byte_pos and over-counts by 1).
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

        if replacements.len() > 1 {
            lines.push(format!(
                "{DIM}── replacement {}/{} ──{RESET}",
                i + 1,
                replacements.len()
            ));
        }

        for (j, line) in old_str.lines().enumerate() {
            lines.push(format!("{RED}{:>4} -{line}{RESET}", start_line + j));
            total_lines += 1;
        }
        for (j, line) in new_str.lines().enumerate() {
            lines.push(format!("{GREEN}{:>4} +{line}{RESET}", start_line + j));
            total_lines += 1;
        }

        if total_lines > MAX_PREVIEW_LINES {
            let remaining = replacements.len() - i - 1;
            if remaining > 0 {
                lines.push(format!(
                    "{DIM}... and {remaining} more replacement(s){RESET}"
                ));
            }
            break;
        }
    }

    // Truncate if too many lines
    if lines.len() > MAX_PREVIEW_LINES {
        lines.truncate(MAX_PREVIEW_LINES);
        let hidden = total_lines - MAX_PREVIEW_LINES;
        lines.push(format!("{DIM}... +{hidden} more lines{RESET}"));
    }

    Some(lines.join("\n"))
}

/// Preview for Write tool: show new-file summary or overwrite diff.
async fn preview_write(args: &serde_json::Value, project_root: &Path) -> Option<String> {
    let inner = args.get("payload").unwrap_or(args);

    let path_str = inner
        .get("path")
        .or(inner.get("file_path"))
        .and_then(|v| v.as_str())?;
    let content = inner.get("content").and_then(|v| v.as_str())?;
    let resolved = safe_resolve_path(project_root, path_str).ok()?;

    let content_lines: Vec<&str> = content.lines().collect();
    let line_count = content_lines.len();

    if resolved.exists() {
        // Overwrite: show what's being replaced
        let existing = tokio::fs::read_to_string(&resolved).await.ok()?;
        let existing_lines = existing.lines().count();
        let existing_bytes = existing.len();

        let mut lines = vec![format!(
            "{DIM}Overwriting {existing_lines} lines ({existing_bytes} bytes) → {line_count} lines ({} bytes){RESET}",
            content.len()
        )];

        // Show first few lines of new content as preview
        let preview_count = line_count.min(8);
        for line in &content_lines[..preview_count] {
            lines.push(format!("{GREEN}+{line}{RESET}"));
        }
        if line_count > 8 {
            lines.push(format!("{DIM}... +{} more lines{RESET}", line_count - 8));
        }

        Some(lines.join("\n"))
    } else {
        // New file: show first few lines
        let mut lines = vec![format!(
            "{DIM}New file: {line_count} lines ({} bytes){RESET}",
            content.len()
        )];

        let preview_count = line_count.min(8);
        for line in &content_lines[..preview_count] {
            lines.push(format!("{GREEN}+{line}{RESET}"));
        }
        if line_count > 8 {
            lines.push(format!("{DIM}... +{} more lines{RESET}", line_count - 8));
        }

        Some(lines.join("\n"))
    }
}

/// Preview for Delete tool: show file/dir size info.
async fn preview_delete(args: &serde_json::Value, project_root: &Path) -> Option<String> {
    let inner = args.get("payload").unwrap_or(args);

    let path_str = inner
        .get("path")
        .or(inner.get("file_path"))
        .and_then(|v| v.as_str())?;
    let resolved = safe_resolve_path(project_root, path_str).ok()?;

    if !resolved.exists() {
        return Some(format!("{DIM}(path does not exist){RESET}"));
    }

    let meta = tokio::fs::metadata(&resolved).await.ok()?;
    if meta.is_file() {
        let size = meta.len();
        let line_count = tokio::fs::read_to_string(&resolved)
            .await
            .map(|c| c.lines().count())
            .unwrap_or(0);
        Some(format!(
            "{RED}Removing {line_count} lines ({size} bytes){RESET}"
        ))
    } else if meta.is_dir() {
        let recursive = args
            .get("recursive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if recursive {
            Some(format!("{RED}Removing directory and all contents{RESET}"))
        } else {
            Some(format!("{RED}Removing empty directory{RESET}"))
        }
    } else {
        None
    }
}

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
        assert!(preview.is_some());
        let text = preview.unwrap();
        assert!(text.contains("hello"));
        assert!(text.contains("world"));
        // Line number for println! on line 2
        assert!(
            text.contains("2 -"),
            "should show line number for removed line"
        );
        assert!(
            text.contains("2 +"),
            "should show line number for added line"
        );
        // Must use background colors
        assert!(
            text.contains("\x1b[41m"),
            "removed lines should use red background"
        );
        assert!(
            text.contains("\x1b[42m"),
            "added lines should use green background"
        );
    }

    #[tokio::test]
    async fn test_preview_write_new_file() {
        let tmp = TempDir::new().unwrap();
        let args = json!({
            "path": "new_file.rs",
            "content": "fn main() {}\n"
        });

        let preview = compute("Write", &args, tmp.path()).await;
        assert!(preview.is_some());
        let text = preview.unwrap();
        assert!(text.contains("New file"));
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
        assert!(preview.is_some());
        let text = preview.unwrap();
        assert!(text.contains("Overwriting"));
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
        assert!(preview.is_some());
        let text = preview.unwrap();
        assert!(text.contains("Removing"));
    }

    #[tokio::test]
    async fn test_preview_read_returns_none() {
        let tmp = TempDir::new().unwrap();
        let args = json!({"path": "anything.rs"});
        let preview = compute("Read", &args, tmp.path()).await;
        assert!(preview.is_none());
    }
}
