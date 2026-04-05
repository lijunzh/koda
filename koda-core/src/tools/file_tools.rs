//! File system tools: Read, Write, Edit, Delete, and List.
//!
//! All paths are validated through `safe_resolve_path` to prevent escapes
//! outside the project root.
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
//! All file paths are resolved relative to the project root. Attempts to
//! access files outside the project (e.g., `../../../etc/passwd`) are blocked
//! with an error. Absolute paths are also rejected unless they resolve within
//! the project root.

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
    let resolved = safe_resolve_path(project_root, path_str)?;

    // Symlink traversal protection (#526): safe_resolve_path uses lexical
    // normalization (path_clean) which can't detect symlinks. Since reads
    // target existing files, we can canonicalize and verify the real path
    // is still inside the project root.
    if resolved.exists() {
        let canon = resolved.canonicalize().unwrap_or_else(|_| resolved.clone());
        let canon_root = project_root
            .canonicalize()
            .unwrap_or_else(|_| project_root.to_path_buf());
        if !canon.starts_with(&canon_root) {
            anyhow::bail!(
                "Path escapes project root via symlink. Requested: {path_str:?}, \
                 Real path: {}",
                canon.display()
            );
        }
    }

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
        if let Some(&(cached_size, cached_mtime)) = cache_guard.get(&cache_key)
            && cached_size == size
            && cached_mtime == mtime
        {
            return Ok(format!(
                "[File '{}' is unchanged since last read. Full content is already in \
                 your conversation history. To read a specific section, use the \
                 start_line and num_lines parameters instead of re-reading the whole file.]",
                path_str
            ));
        }
    }

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
        cache_guard.insert(cache_key, (size, mtime));
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

/// Apply targeted find-and-replace edits to an existing file.
pub async fn edit_file(project_root: &Path, args: &Value) -> Result<String> {
    let path_str = args["file_path"]
        .as_str()
        .or_else(|| args["path"].as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'file_path' argument"))?;
    let replacements = args["replacements"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Missing 'replacements' argument"))?;

    let resolved = safe_resolve_path(project_root, path_str)?;
    let mut content = tokio::fs::read_to_string(&resolved).await?;
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

        // ── Exact match path ──────────────────────────────────────────────
        if content.contains(old_str) {
            if replace_all {
                let count = content.matches(old_str).count();
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
                    "Replacement {i}: 'old_str' is ambiguous — {n} fuzzy matches in '{}'. \
                     Use a more specific snippet.",
                    path_str
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
    let resolved = safe_resolve_path(project_root, path_str)?;

    let mut entries = Vec::new();
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
    } else if total_count > max_entries {
        Ok(format!(
            "{}\n\n... [CAPPED at {max_entries} entries. Use a subdirectory path to narrow results.]",
            entries.join("\n")
        ))
    } else {
        Ok(entries.join("\n"))
    }
}
