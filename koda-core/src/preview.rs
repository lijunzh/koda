//! Pre-confirmation diff previews for destructive tool operations.
//!
//! Computes a colored diff preview before the user confirms an Edit or Write,
//! so they can make an informed decision instead of approving blind.

use crate::tools::safe_resolve_path;
use std::path::Path;

const LINE_RED: &str = "\x1b[48;5;52m"; // dark red background — removed line
const LINE_GREEN: &str = "\x1b[48;5;22m"; // dark green background — added line
const WORD_RED: &str = "\x1b[48;5;88m"; // brighter red — changed words in removed line
const WORD_GREEN: &str = "\x1b[48;5;28m"; // brighter green — changed words in added line
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

/// Preview for Edit tool: show each replacement as old (red) → new (green) with line numbers
/// and intra-line word highlighting for the changed region.
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

        let old_line_vec: Vec<&str> = old_str.lines().collect();
        let new_line_vec: Vec<&str> = new_str.lines().collect();
        let pair_count = old_line_vec.len().min(new_line_vec.len());

        // Paired lines: render with intra-line word highlighting
        for j in 0..pair_count {
            let (old_content, new_content) = intra_line_diff(old_line_vec[j], new_line_vec[j]);
            lines.push(format!(
                "{LINE_RED}{:>4} -{old_content}{RESET}",
                start_line + j
            ));
            lines.push(format!(
                "{LINE_GREEN}{:>4} +{new_content}{RESET}",
                start_line + j
            ));
            total_lines += 2;
        }

        // Remaining old lines (pure deletions without a new counterpart)
        for j in pair_count..old_line_vec.len() {
            lines.push(format!(
                "{LINE_RED}{:>4} -{}{RESET}",
                start_line + j,
                old_line_vec[j]
            ));
            total_lines += 1;
        }

        // Remaining new lines (pure additions without an old counterpart)
        for j in pair_count..new_line_vec.len() {
            lines.push(format!(
                "{LINE_GREEN}{:>4} +{}{RESET}",
                start_line + j,
                new_line_vec[j]
            ));
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

/// Compute intra-line diff content for a pair of lines.
///
/// Returns `(old_content, new_content)` with word-level highlight escapes
/// embedded. The caller wraps each in the line-level background + RESET.
fn intra_line_diff(old_line: &str, new_line: &str) -> (String, String) {
    let (pre_bytes, suf_bytes_old, suf_bytes_new) = common_prefix_suffix(old_line, new_line);

    let old_prefix = &old_line[..pre_bytes];
    let old_changed = &old_line[pre_bytes..old_line.len() - suf_bytes_old];
    let old_suffix = &old_line[old_line.len() - suf_bytes_old..];

    let new_prefix = &new_line[..pre_bytes];
    let new_changed = &new_line[pre_bytes..new_line.len() - suf_bytes_new];
    let new_suffix = &new_line[new_line.len() - suf_bytes_new..];

    let old_content = if old_changed.is_empty() {
        format!("{old_prefix}{old_suffix}")
    } else {
        format!("{old_prefix}{WORD_RED}{old_changed}{LINE_RED}{old_suffix}")
    };

    let new_content = if new_changed.is_empty() {
        format!("{new_prefix}{new_suffix}")
    } else {
        format!("{new_prefix}{WORD_GREEN}{new_changed}{LINE_GREEN}{new_suffix}")
    };

    (old_content, new_content)
}

/// Find the byte lengths of the common prefix and suffix shared by `old` and `new`.
///
/// Returns `(prefix_bytes, suffix_bytes_old, suffix_bytes_new)`.
/// Because the prefix chars are identical in both strings, `prefix_bytes` is
/// the same for both. The suffix may differ in byte length for multi-byte chars.
fn common_prefix_suffix(old: &str, new: &str) -> (usize, usize, usize) {
    let prefix_chars = old
        .chars()
        .zip(new.chars())
        .take_while(|(a, b)| a == b)
        .count();

    let prefix_bytes = old
        .char_indices()
        .nth(prefix_chars)
        .map(|(i, _)| i)
        .unwrap_or(old.len());

    // Compute suffix only within the remaining part after the prefix
    let old_rest: Vec<char> = old[prefix_bytes..].chars().collect();
    let new_rest: Vec<char> = new[prefix_bytes..].chars().collect();

    let suffix_chars = old_rest
        .iter()
        .rev()
        .zip(new_rest.iter().rev())
        .take_while(|(a, b)| a == b)
        .count();

    let suffix_bytes_old: usize = old_rest
        .iter()
        .rev()
        .take(suffix_chars)
        .map(|c| c.len_utf8())
        .sum();
    let suffix_bytes_new: usize = new_rest
        .iter()
        .rev()
        .take(suffix_chars)
        .map(|c| c.len_utf8())
        .sum();

    (prefix_bytes, suffix_bytes_old, suffix_bytes_new)
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
            lines.push(format!("{LINE_GREEN}+{line}{RESET}"));
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
            lines.push(format!("{LINE_GREEN}+{line}{RESET}"));
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
            "{LINE_RED}Removing {line_count} lines ({size} bytes){RESET}"
        ))
    } else if meta.is_dir() {
        let recursive = args
            .get("recursive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if recursive {
            Some(format!(
                "{LINE_RED}Removing directory and all contents{RESET}"
            ))
        } else {
            Some(format!("{LINE_RED}Removing empty directory{RESET}"))
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
        // Line-level dark background colors
        assert!(
            text.contains(LINE_RED),
            "removed lines should use dark red background"
        );
        assert!(
            text.contains(LINE_GREEN),
            "added lines should use dark green background"
        );
        // Intra-line word highlights: "hello" vs "world" are the changed words
        assert!(
            text.contains(WORD_RED),
            "changed word in removed line should be highlighted"
        );
        assert!(
            text.contains(WORD_GREEN),
            "changed word in added line should be highlighted"
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
