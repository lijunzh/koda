//! File system tools: Read, Write, Edit, Delete, and List.
//!
//! **Path policy** (see `docs/src/sandbox.md` for full rationale):
//! - Read / List use `resolve_path_unrestricted` — any path on the filesystem.
//! - Write / Edit / Delete use `safe_resolve_path` — restricted to project root.
//!
//! ## Tools
//!
//! | Tool | Description | Effect |
//! |---|---|---|
//! | **Read** | Read file contents with line numbers. Supports `start_line`/`num_lines` for large files. | ReadOnly |
//! | **Write** | Create a new file or overwrite an existing one. Use `overwrite: true` to replace. | LocalMutation |
//! | **Edit** | Find-and-replace in an existing file. Matches `old_str` exactly and replaces with `new_str`. Use `replace_all: true` to replace all occurrences. | LocalMutation |
//! | **Delete** | Delete a file. Always requires confirmation (Destructive effect). | Destructive |
//! | **List** | List files and directories. Respects `.gitignore`. | ReadOnly |
//!
//! ## Path safety
//!
//! All file paths resolve relative to the project root for relative inputs, or
//! as-is for absolute inputs.  Read / List accept any reachable path;
//! Write / Edit / Delete are restricted to the project root (see sandbox.md).

use sha2::{Digest, Sha256};

use super::resolve_read_path;
use super::safe_resolve_path;
use crate::providers::ToolDefinition;
use anyhow::Result;
use serde_json::{Value, json};
use std::path::Path;
use std::time::SystemTime;

/// Return tool definitions for the LLM.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "Read".to_string(),
            description: "Read the contents of a file. The output includes line numbers. \
                For large files (>500 lines), use start_line and num_lines to read specific \
                portions instead of the whole file. ALWAYS read a file before editing it — \
                never guess at file contents. Re-read after editing to verify changes."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Relative or absolute path to the file"
                    },
                    "start_line": {
                        "type": "integer",
                        "description": "Optional 1-based start line for partial reads"
                    },
                    "num_lines": {
                        "type": "integer",
                        "description": "Number of lines to read from start_line"
                    }
                },
                "required": ["file_path"]
            }),
        },
        ToolDefinition {
            name: "Write".to_string(),
            description: "Create a new file or overwrite an existing one. \
                Set overwrite=true to replace an existing file. \
                For targeted edits to existing files, prefer Edit instead."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Relative or absolute path to the file"
                    },
                    "content": {
                        "type": "string",
                        "description": "The full content to write"
                    },
                    "overwrite": {
                        "type": "boolean",
                        "description": "Must be true to overwrite an existing file (default: false)"
                    }
                },
                "required": ["file_path", "content"]
            }),
        },
        ToolDefinition {
            name: "Edit".to_string(),
            description: "Targeted find-and-replace in an existing file. \
                Each replacement matches exact 'old_str' and replaces with 'new_str'. \
                ALWAYS Read the file first to get exact text. \
                Keep each diff small — target only the minimal snippet you want changed. \
                Apply multiple sequential Edit calls for large refactors. \
                Never paste an entire file inside old_str."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Path to the file to edit"
                    },
                    "replacements": {
                        "type": "array",
                        "description": "List of find-and-replace operations",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_str": {
                                    "type": "string",
                                    "description": "Exact text to find in the file"
                                },
                                "new_str": {
                                    "type": "string",
                                    "description": "Text to replace it with"
                                },
                                "replace_all": {
                                    "type": "boolean",
                                    "description": "Replace all occurrences instead of just the first (default: false)"
                                }
                            },
                            "required": ["old_str", "new_str"]
                        }
                    }
                },
                "required": ["file_path", "replacements"]
            }),
        },
        ToolDefinition {
            name: "Delete".to_string(),
            description: "Delete a file or directory. For directories, set recursive to true. \
                Returns what was removed and the count of deleted items."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Path to the file or directory to delete"
                    },
                    "recursive": {
                        "type": "boolean",
                        "description": "Required for deleting non-empty directories (default: false)"
                    }
                },
                "required": ["file_path"]
            }),
        },
        ToolDefinition {
            name: "List".to_string(),
            description: "List files and directories in a given path. Respects .gitignore \
                and skips common noise (node_modules, __pycache__, .git). \
                Use with recursive=false (default) to explore project structure one level \
                at a time. Use with recursive=true for a full tree view. \
                For finding files by pattern (e.g. all *.rs files), prefer Glob instead."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Directory to list (default: project root)"
                    },
                    "recursive": {
                        "type": "boolean",
                        "description": "Whether to recurse into subdirectories (default: false)"
                    }
                }
            }),
        },
    ]
}

/// Read file contents, with optional line-range selection.
/// When a line range is requested, only reads lines up to the end of the range
/// instead of loading the entire file into memory.
pub async fn read_file(
    project_root: &Path,
    args: &Value,
    cache: &super::FileReadCache,
) -> Result<String> {
    let path_str = args["file_path"]
        .as_str()
        .or_else(|| args["path"].as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'file_path' argument"))?;
    let resolved = resolve_read_path(project_root, path_str)?;

    // No symlink traversal check — reads are unrestricted (docs/src/sandbox.md).

    let start_line = args["start_line"].as_u64();
    let num_lines = args["num_lines"].as_u64();

    // Check if the file exists and get its metadata
    let metadata = tokio::fs::metadata(&resolved)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", resolved.display(), e))?;

    let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let size = metadata.len();

    let cache_key = format!("{}:{:?}:{:?}", resolved.display(), start_line, num_lines);

    // Stale-read optimization: if the file hasn't changed since the last time this session read it,
    // we don't need to re-read and re-stream it to the LLM. It's already in the conversation context.
    {
        let cache_guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((cached_size, cached_mtime, _)) = cache_guard.get(&cache_key)
            && *cached_size == size
            && *cached_mtime == mtime
        {
            return Ok(format!(
                "[File '{}' is unchanged since last read. Full content is already in \
                 your conversation history. To read a specific section, use the \
                 start_line and num_lines parameters instead of re-reading the whole file.]",
                path_str
            ));
        }
    }

    // For full-file reads we compute a SHA-256 content hash to enable
    // staleness detection in edit_file (Gemini CLI strategy).
    let mut content_sha256 = String::new();

    let output = match (start_line, num_lines) {
        (Some(start), Some(count)) => {
            // Line-range read: use BufReader to avoid loading the entire file
            use tokio::io::{AsyncBufReadExt, BufReader};
            let file = tokio::fs::File::open(&resolved).await?;
            let reader = BufReader::new(file);
            let mut lines = reader.lines();

            let start_idx = (start as usize).saturating_sub(1); // 1-based to 0-based
            let mut collected = Vec::with_capacity(count as usize);
            let mut current = 0usize;

            while let Some(line) = lines.next_line().await? {
                if current >= start_idx {
                    collected.push(line);
                    if collected.len() >= count as usize {
                        break;
                    }
                }
                current += 1;
            }
            collected.join("\n")
        }
        _ => {
            // Full read with token safety cap
            let content = tokio::fs::read_to_string(&resolved).await?;

            // Hash the raw (un-truncated) content so edit_file can detect if
            // the file changes between this read and the subsequent write.
            content_sha256 = format!("{:x}", Sha256::digest(content.as_bytes()));

            if content.len() > 20_000 {
                // Snap to char boundary to avoid panic on multi-byte chars
                let mut end = 20_000;
                while !content.is_char_boundary(end) {
                    end -= 1;
                }
                format!(
                    "{}\n\n... [TRUNCATED: file is {} bytes. Use start_line/num_lines for large files]",
                    &content[..end],
                    content.len()
                )
            } else {
                content
            }
        }
    };

    // Update the cache after a successful read
    {
        let mut cache_guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        cache_guard.insert(cache_key, (size, mtime, content_sha256));
    }

    Ok(output)
}

/// Write content to a file, creating parent directories as needed.
/// Refuses to overwrite existing files unless `overwrite=true`.
pub async fn write_file(project_root: &Path, args: &Value) -> Result<String> {
    let path_str = args["file_path"]
        .as_str()
        .or_else(|| args["path"].as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'file_path' argument"))?;
    let content = args["content"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'content' argument"))?;
    let overwrite = args["overwrite"].as_bool().unwrap_or(false);

    let resolved = safe_resolve_path(project_root, path_str)?;

    // Overwrite protection: refuse to clobber existing files without explicit opt-in
    if resolved.exists() && !overwrite {
        anyhow::bail!(
            "File '{}' already exists. Set overwrite=true to replace it, \
             or use Edit for targeted changes.",
            path_str
        );
    }

    // Ensure parent directory exists
    if let Some(parent) = resolved.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    tokio::fs::write(&resolved, content).await?;
    Ok(format!(
        "Written {} bytes to {}",
        content.len(),
        resolved.display()
    ))
}

/// Convert a byte offset in `content` to a 1-based line number.
fn byte_offset_to_line(content: &str, offset: usize) -> usize {
    content[..offset.min(content.len())]
        .chars()
        .filter(|&c| c == '\n')
        .count()
        + 1
}

/// Return the 1-based line numbers of every occurrence of `needle` in `haystack`.
fn match_line_numbers(haystack: &str, needle: &str) -> Vec<usize> {
    let mut line_nos = Vec::new();
    let mut start = 0;
    while let Some(rel) = haystack[start..].find(needle) {
        let abs = start + rel;
        line_nos.push(byte_offset_to_line(haystack, abs));
        start = abs + 1; // advance past this hit to find the next
    }
    line_nos
}

/// Apply targeted find-and-replace edits to an existing file.
///
/// Staleness check: if the model previously read this file (full read) the
/// cached SHA-256 is compared against the current on-disk hash before any
/// write.  A mismatch means an external process (bash, the user, another
/// agent) changed the file since the model last saw it — the edit is
/// rejected so the model can re-read and retry with fresh text.
pub async fn edit_file(
    project_root: &Path,
    args: &Value,
    cache: &super::FileReadCache,
) -> Result<String> {
    let path_str = args["file_path"]
        .as_str()
        .or_else(|| args["path"].as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'file_path' argument"))?;
    let replacements = args["replacements"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Missing 'replacements' argument"))?;

    let resolved = safe_resolve_path(project_root, path_str)?;
    let mut content = tokio::fs::read_to_string(&resolved).await?;

    // ── Staleness check (SHA-256, Gemini CLI strategy) ────────────────────
    //
    // The full-read cache key matches what read_file stores for a whole-file
    // read (start_line=None, num_lines=None).  If we have a cached hash for
    // this file and it doesn't match the on-disk content, an external change
    // (bash, the user, another agent) happened since the model last read it.
    // Reject the edit so the model re-reads instead of clobbering changes.
    let full_key = format!("{}:None:None", resolved.display());
    {
        let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, _, cached_hash)) = guard.get(&full_key)
            && !cached_hash.is_empty()
        {
            let current_hash = format!("{:x}", Sha256::digest(content.as_bytes()));
            if *cached_hash != current_hash {
                anyhow::bail!(
                    "File '{}' has changed on disk since you last read it \
                     (SHA-256 mismatch). Re-read the file to get the current \
                     content before editing.",
                    path_str
                );
            }
        }
    }
    // ──────────────────────────────────────────────────────────────────────

    let mut changes = Vec::new();

    for (i, replacement) in replacements.iter().enumerate() {
        let old_str = replacement["old_str"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Replacement {i}: missing 'old_str'"))?;
        let new_str = replacement["new_str"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Replacement {i}: missing 'new_str'"))?;

        if old_str.is_empty() {
            anyhow::bail!("Replacement {i}: 'old_str' cannot be empty");
        }

        let replace_all = replacement["replace_all"].as_bool().unwrap_or(false);

        // ── Exact match path ─────────────────────────────────────────────────────────────
        if content.contains(old_str) {
            let count = content.matches(old_str).count();

            if replace_all {
                content = content.replace(old_str, new_str);
                for line in old_str.lines() {
                    changes.push(format!("-{line}"));
                }
                for line in new_str.lines() {
                    changes.push(format!("+{line}"));
                }
                if count > 1 {
                    changes.push(format!("({count} occurrences replaced)"));
                }
            } else if count > 1 {
                // Multiple exact matches: report line numbers so the model
                // can tighten the snippet in one shot (Claude Code strategy).
                let lines = match_line_numbers(&content, old_str);
                let line_list = lines
                    .iter()
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                anyhow::bail!(
                    "Replacement {i}: 'old_str' matches {count} times in '{}' \
                     (at lines {line_list}). Set replace_all=true to replace \
                     every occurrence, or expand the snippet to uniquely \
                     identify the one you want.",
                    path_str
                );
            } else {
                content = content.replacen(old_str, new_str, 1);
                for line in old_str.lines() {
                    changes.push(format!("-{line}"));
                }
                for line in new_str.lines() {
                    changes.push(format!("+{line}"));
                }
            }
        } else {
            // ── Fuzzy fallback (trailing-whitespace-normalized) ──────────
            let ranges = super::fuzzy::fuzzy_match_ranges(old_str, &content);
            match ranges.len() {
                0 => anyhow::bail!(
                    "Replacement {i}: 'old_str' not found in '{}'. \
                     Read the file first to get the exact text.",
                    path_str
                ),
                1 => {
                    let r = ranges.into_iter().next().unwrap();
                    for line in old_str.lines() {
                        changes.push(format!("-{line}"));
                    }
                    for line in new_str.lines() {
                        changes.push(format!("+{line}"));
                    }
                    changes.push("(fuzzy match: trailing whitespace ignored)".into());
                    content = format!("{}{}{}", &content[..r.start], new_str, &content[r.end..]);
                }
                n => anyhow::bail!(
                    "Replacement {i}: 'old_str' is ambiguous — {n} fuzzy matches \
                     in '{}' (at lines {}). Use a more specific snippet.",
                    path_str,
                    ranges
                        .iter()
                        .map(|r| byte_offset_to_line(&content, r.start).to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        }

        if replacements.len() > 1 {
            changes.push(String::new()); // separator between replacements
        }
    }

    tokio::fs::write(&resolved, &content).await?;

    Ok(format!(
        "Applied {} edit(s) to {}\n{}",
        replacements.len(),
        resolved.display(),
        changes.join("\n")
    ))
}

/// Delete a file and return confirmation.
pub async fn delete_file(project_root: &Path, args: &Value) -> Result<String> {
    let path_str = args["file_path"]
        .as_str()
        .or_else(|| args["path"].as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'file_path' argument"))?;
    let recursive = args["recursive"].as_bool().unwrap_or(false);
    let resolved = safe_resolve_path(project_root, path_str)?;

    if !resolved.exists() {
        anyhow::bail!("Path not found: {}", resolved.display());
    }

    // Prevent deleting the project root itself
    if resolved == project_root {
        anyhow::bail!("Cannot delete the project root directory");
    }

    if resolved.is_file() {
        let size = tokio::fs::metadata(&resolved).await?.len();
        tokio::fs::remove_file(&resolved).await?;
        Ok(format!(
            "Deleted file {} ({} bytes)",
            resolved.display(),
            size
        ))
    } else if resolved.is_dir() {
        // Check if directory is empty
        let is_empty = resolved.read_dir()?.next().is_none();

        if is_empty {
            tokio::fs::remove_dir(&resolved).await?;
            Ok(format!("Deleted empty directory {}", resolved.display()))
        } else if recursive {
            // Count items for informative output
            let count = count_dir_entries(&resolved);
            tokio::fs::remove_dir_all(&resolved).await?;
            Ok(format!(
                "Deleted directory {} ({} items removed)",
                resolved.display(),
                count
            ))
        } else {
            anyhow::bail!(
                "Directory {} is not empty. Set recursive=true to delete it and all contents.",
                resolved.display()
            )
        }
    } else {
        anyhow::bail!("Path is not a file or directory: {}", resolved.display())
    }
}

/// Count all entries in a directory recursively.
fn count_dir_entries(path: &Path) -> usize {
    let mut count = 0;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            count += 1;
            if entry.path().is_dir() {
                count += count_dir_entries(&entry.path());
            }
        }
    }
    count
}

/// List files in a directory, respecting .gitignore.
/// Entry cap is set by `OutputCaps` (context-scaled).
pub async fn list_files(project_root: &Path, args: &Value, max_entries: usize) -> Result<String> {
    let path_str = args["file_path"]
        .as_str()
        .or_else(|| args["path"].as_str())
        .unwrap_or(".");
    let recursive = args["recursive"].as_bool().unwrap_or(false);
    let resolved = resolve_read_path(project_root, path_str)?;
    let mut entries: Vec<String> = Vec::new();
    let mut total_count: usize = 0;

    if recursive {
        // Use the `ignore` crate to respect .gitignore
        let mut builder = ignore::WalkBuilder::new(&resolved);
        builder
            .hidden(true) // skip hidden files/dirs (dotfiles)
            .git_ignore(true)
            // Always ignore common build/dependency dirs even without .gitignore
            .filter_entry(|entry| {
                let name = entry.file_name().to_string_lossy();
                !matches!(
                    name.as_ref(),
                    "target"
                        | "node_modules"
                        | "__pycache__"
                        | ".git"
                        | "dist"
                        | "build"
                        | ".next"
                        | ".cache"
                )
            });
        let walker = builder.build();

        for entry in walker.flatten() {
            let path = entry.path();
            // Skip the root directory itself
            if path == resolved {
                continue;
            }
            let relative = path.strip_prefix(project_root).unwrap_or(path);
            let prefix = if path.is_dir() { "d " } else { "  " };
            entries.push(format!("{prefix}{}", relative.display()));
            total_count += 1;
            if entries.len() >= max_entries {
                break;
            }
        }
    } else {
        let mut reader = tokio::fs::read_dir(&resolved).await?;
        while let Some(entry) = reader.next_entry().await? {
            let ft = entry.file_type().await?;
            let prefix = if ft.is_dir() { "d " } else { "  " };
            entries.push(format!("{prefix}{}", entry.file_name().to_string_lossy()));
            total_count += 1;
            if entries.len() >= max_entries {
                break;
            }
        }
    }

    if entries.is_empty() {
        Ok("(empty directory)".to_string())
    } else if total_count >= max_entries {
        Ok(format!(
            "{}\n\n... [CAPPED at {max_entries} entries. Use a subdirectory path to narrow results.]",
            entries.join("\n")
        ))
    } else {
        Ok(entries.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    fn cache() -> super::super::FileReadCache {
        std::sync::Arc::new(Mutex::new(HashMap::new()))
    }

    // ── Read ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn read_file_basic() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("hello.txt");
        std::fs::write(&f, "line1\nline2\nline3").unwrap();

        let args = json!({"file_path": f.to_string_lossy()});
        let result = read_file(tmp.path(), &args, &cache()).await.unwrap();
        assert!(result.contains("line1"));
        assert!(result.contains("line3"));
    }

    #[tokio::test]
    async fn read_file_with_line_range() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("lines.txt");
        let content: String = (1..=100).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&f, &content).unwrap();

        let args = json!({"file_path": f.to_string_lossy(), "start_line": 50, "num_lines": 3});
        let result = read_file(tmp.path(), &args, &cache()).await.unwrap();
        assert!(result.contains("line 50"));
        assert!(result.contains("line 52"));
        assert!(!result.contains("line 53"));
    }

    #[tokio::test]
    async fn read_file_nonexistent_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let args = json!({"file_path": "does_not_exist.txt"});
        let result = read_file(tmp.path(), &args, &cache()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn read_file_stale_cache_returns_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("cached.txt");
        std::fs::write(&f, "original content").unwrap();

        let c = cache();
        let args = json!({"file_path": f.to_string_lossy()});

        // First read — populates cache.
        let r1 = read_file(tmp.path(), &args, &c).await.unwrap();
        assert!(r1.contains("original content"));

        // Second read — same file, same mtime → stale-read.
        let r2 = read_file(tmp.path(), &args, &c).await.unwrap();
        assert!(r2.contains("unchanged"), "expected stale-read: {r2}");
    }

    #[tokio::test]
    async fn read_file_missing_path_arg_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let args = json!({});
        let result = read_file(tmp.path(), &args, &cache()).await;
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("file_path"),
            "should mention missing param"
        );
    }

    #[tokio::test]
    async fn read_file_large_file_truncates() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("big.txt");
        let content = "x".repeat(30_000);
        std::fs::write(&f, &content).unwrap();

        let args = json!({"file_path": f.to_string_lossy()});
        let result = read_file(tmp.path(), &args, &cache()).await.unwrap();
        assert!(result.contains("TRUNCATED"));
        assert!(result.len() < 25_000);
    }

    #[tokio::test]
    async fn read_file_can_reach_outside_project_root() {
        // Reads are unrestricted (docs/src/sandbox.md).
        // Create: /tmp/outer/secret.txt and /tmp/outer/project/
        // Then read "../secret.txt" from project/.
        let outer = tempfile::tempdir().unwrap();
        let secret = outer.path().join("secret.txt");
        std::fs::write(&secret, "outer content").unwrap();
        let project = outer.path().join("project");
        std::fs::create_dir_all(&project).unwrap();

        let args = json!({"file_path": "../secret.txt"});
        let result = read_file(&project, &args, &cache()).await;
        assert!(
            result.is_ok(),
            "read outside project root should succeed; got: {:?}",
            result
        );
        assert!(result.unwrap().contains("outer content"));
    }

    // ── Write ────────────────────────────────────────────────────

    #[tokio::test]
    async fn write_file_creates_new() {
        let tmp = tempfile::tempdir().unwrap();
        let args = json!({"file_path": "new_file.txt", "content": "hello world"});
        let result = write_file(tmp.path(), &args).await.unwrap();
        assert!(result.contains("Written"));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("new_file.txt")).unwrap(),
            "hello world"
        );
    }

    #[tokio::test]
    async fn write_file_creates_parent_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let args = json!({"file_path": "a/b/c/deep.txt", "content": "nested"});
        write_file(tmp.path(), &args).await.unwrap();
        assert!(tmp.path().join("a/b/c/deep.txt").exists());
    }

    #[tokio::test]
    async fn write_file_refuses_overwrite_without_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("existing.txt");
        std::fs::write(&f, "original").unwrap();

        let args = json!({"file_path": "existing.txt", "content": "replaced"});
        let result = write_file(tmp.path(), &args).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
        // File should be unchanged.
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "original");
    }

    #[tokio::test]
    async fn write_file_overwrites_with_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("overwrite_me.txt");
        std::fs::write(&f, "old").unwrap();

        let args = json!({"file_path": "overwrite_me.txt", "content": "new", "overwrite": true});
        write_file(tmp.path(), &args).await.unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "new");
    }

    // ── Edit ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn edit_file_single_replacement() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("edit_me.txt");
        std::fs::write(&f, "hello world\nfoo bar").unwrap();

        let args = json!({
            "file_path": "edit_me.txt",
            "replacements": [{"old_str": "foo", "new_str": "baz"}]
        });
        let result = edit_file(tmp.path(), &args, &cache()).await.unwrap();
        assert!(result.contains("Applied 1 edit"));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "hello world\nbaz bar");
    }

    #[tokio::test]
    async fn edit_file_replace_all() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("multi.txt");
        std::fs::write(&f, "aaa bbb aaa ccc aaa").unwrap();

        let args = json!({
            "file_path": "multi.txt",
            "replacements": [{"old_str": "aaa", "new_str": "zzz", "replace_all": true}]
        });
        let result = edit_file(tmp.path(), &args, &cache()).await.unwrap();
        assert!(result.contains("3 occurrences"));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "zzz bbb zzz ccc zzz");
    }

    /// Multi-match without replace_all must now error with line numbers instead
    /// of silently clobbering the first occurrence.
    #[tokio::test]
    async fn edit_file_multi_match_errors_with_line_numbers() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("multi_match.txt");
        std::fs::write(&f, "aaa\nbbb\naaa").unwrap();

        let args = json!({
            "file_path": "multi_match.txt",
            "replacements": [{"old_str": "aaa", "new_str": "zzz"}]
        });
        let err = edit_file(tmp.path(), &args, &cache()).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("matches 2 times"), "expected count: {msg}");
        assert!(
            msg.contains("lines"),
            "expected line numbers mention: {msg}"
        );
        // File must be untouched
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "aaa\nbbb\naaa");
    }

    #[tokio::test]
    async fn edit_file_not_found_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("edit_me.txt");
        std::fs::write(&f, "hello world").unwrap();

        let args = json!({
            "file_path": "edit_me.txt",
            "replacements": [{"old_str": "not here", "new_str": "x"}]
        });
        let result = edit_file(tmp.path(), &args, &cache()).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn edit_file_empty_old_str_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("edit.txt");
        std::fs::write(&f, "content").unwrap();

        let args = json!({
            "file_path": "edit.txt",
            "replacements": [{"old_str": "", "new_str": "x"}]
        });
        let result = edit_file(tmp.path(), &args, &cache()).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty"));
    }

    #[tokio::test]
    async fn edit_file_multiple_replacements() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("multi_edit.txt");
        std::fs::write(&f, "alpha beta gamma").unwrap();

        let args = json!({
            "file_path": "multi_edit.txt",
            "replacements": [
                {"old_str": "alpha", "new_str": "ALPHA"},
                {"old_str": "gamma", "new_str": "GAMMA"}
            ]
        });
        edit_file(tmp.path(), &args, &cache()).await.unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "ALPHA beta GAMMA");
    }

    /// Staleness guard: if the read cache has a SHA-256 for a file and the
    /// file changes on disk, edit_file must reject the write.
    #[tokio::test]
    async fn edit_file_staleness_check_rejects_changed_file() {
        use sha2::{Digest, Sha256};

        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("staleness.txt");
        let original = "line one\nline two\n";
        std::fs::write(&f, original).unwrap();

        // Simulate a prior full read: store the hash of `original` in the cache.
        let hash = format!("{:x}", Sha256::digest(original.as_bytes()));
        let key = format!("{}:None:None", f.display());
        let c = cache();
        {
            let mut g = c.lock().unwrap();
            g.insert(
                key,
                (original.len() as u64, std::time::SystemTime::now(), hash),
            );
        }

        // An external agent now changes the file.
        std::fs::write(&f, "completely different content\n").unwrap();

        // Edit must be rejected with a staleness error.
        let args = json!({
            "file_path": "staleness.txt",
            "replacements": [{"old_str": "line one", "new_str": "LINE ONE"}]
        });
        let err = edit_file(tmp.path(), &args, &c).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("changed on disk") || msg.contains("SHA-256"),
            "expected staleness error: {msg}"
        );
        // File must still have the external-agent content, not the edit.
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "completely different content\n"
        );
    }

    // ── Delete ───────────────────────────────────────────────────

    #[tokio::test]
    async fn delete_file_removes_file() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("doomed.txt");
        std::fs::write(&f, "goodbye").unwrap();

        let args = json!({"file_path": "doomed.txt"});
        let result = delete_file(tmp.path(), &args).await.unwrap();
        assert!(result.contains("Deleted"));
        assert!(!f.exists());
    }

    #[tokio::test]
    async fn delete_file_nonexistent_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let args = json!({"file_path": "nope.txt"});
        let result = delete_file(tmp.path(), &args).await;
        assert!(result.is_err());
    }

    // ── List ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn list_files_basic() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "").unwrap();
        std::fs::create_dir(tmp.path().join("subdir")).unwrap();

        let args = json!({"directory": "."});
        let result = list_files(tmp.path(), &args, 200).await.unwrap();
        assert!(result.contains("a.txt"));
        assert!(result.contains("b.txt"));
        assert!(result.contains("subdir"));
    }

    #[tokio::test]
    async fn list_files_capped() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..20 {
            std::fs::write(tmp.path().join(format!("file_{i}.txt")), "").unwrap();
        }

        let args = json!({"file_path": "."});
        let result = list_files(tmp.path(), &args, 5).await.unwrap();
        assert!(result.contains("CAPPED"), "expected cap message: {result}");
    }
}
