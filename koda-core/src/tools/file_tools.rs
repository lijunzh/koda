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
use super::{allowed_mutation_roots, safe_resolve_path};
use crate::providers::ToolDefinition;
use anyhow::Result;
use koda_sandbox::fs::FileSystem;
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
    fs: &dyn FileSystem,
) -> Result<String> {
    let path_str = args["file_path"]
        .as_str()
        .or_else(|| args["path"].as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'file_path' argument"))?;
    let resolved = resolve_read_path(project_root, path_str)?;

    // No symlink traversal check — reads are unrestricted (docs/src/sandbox.md).

    let start_line = args["start_line"].as_u64();
    let num_lines = args["num_lines"].as_u64();

    // Use tokio::fs::metadata for the mtime/size cache check — this is a
    // host-side performance optimization only; actual content reads go through
    // the FileSystem abstraction so the sandbox can intercept them (Phase 2d).
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

    // Read the full file through the FileSystem abstraction (no cap — we truncate
    // for display below, but hash the raw content for staleness detection).
    let raw_bytes = fs
        .read(&resolved, None)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", resolved.display(), e))?;
    let raw_content = String::from_utf8_lossy(&raw_bytes).into_owned();

    let output = match (start_line, num_lines) {
        (Some(start), Some(count)) => {
            // Line-range slice — the file is already in memory from the trait call above.
            let start_idx = (start as usize).saturating_sub(1); // 1-based to 0-based
            raw_content
                .lines()
                .skip(start_idx)
                .take(count as usize)
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => {
            // Full read with token safety cap.
            // Hash the raw (un-truncated) content so edit_file can detect if
            // the file changes between this read and the subsequent write.
            content_sha256 = format!("{:x}", Sha256::digest(raw_content.as_bytes()));

            if raw_content.len() > 20_000 {
                // Snap to char boundary to avoid panic on multi-byte chars
                let mut end = 20_000;
                while !raw_content.is_char_boundary(end) {
                    end -= 1;
                }
                format!(
                    "{}\n\n... [TRUNCATED: file is {} bytes. Use start_line/num_lines for large files]",
                    &raw_content[..end],
                    raw_content.len()
                )
            } else {
                raw_content
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
pub async fn write_file(project_root: &Path, args: &Value, fs: &dyn FileSystem) -> Result<String> {
    let path_str = args["file_path"]
        .as_str()
        .or_else(|| args["path"].as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'file_path' argument"))?;
    let content = args["content"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'content' argument"))?;
    let overwrite = args["overwrite"].as_bool().unwrap_or(false);

    let resolved = safe_resolve_path(project_root, path_str)?;

    // #1281: re-verify the resolved path against canonicalized allowed
    // roots so a symlink at the final component or any parent dir
    // can't escape the project. safe_resolve_path is purely logical
    // (no canonicalize) and is blind to symlinks. The two checks layer:
    // safe_resolve_path catches literal `../etc/passwd` style escapes
    // (works for non-existing files), verify_mutation_safe catches
    // symlink escapes (requires walking the existing FS).
    koda_sandbox::fs::verify_mutation_safe(&resolved, &allowed_mutation_roots(project_root))
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Overwrite protection: refuse to clobber existing files without explicit opt-in.
    // Use fs.stat() so the check respects the sandbox boundary.
    let already_exists = fs.stat(&resolved).await.is_ok();
    if already_exists && !overwrite {
        anyhow::bail!(
            "File '{}' already exists. Set overwrite=true to replace it, \
             or use Edit for targeted changes.",
            path_str
        );
    }

    // Delegate write (+ parent-dir creation) to the FileSystem impl.
    fs.write(&resolved, content.as_bytes())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to write {}: {}", resolved.display(), e))?;

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
    fs: &dyn FileSystem,
) -> Result<String> {
    let path_str = args["file_path"]
        .as_str()
        .or_else(|| args["path"].as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'file_path' argument"))?;
    let replacements = args["replacements"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Missing 'replacements' argument"))?;

    let resolved = safe_resolve_path(project_root, path_str)?;

    // #1281: symlink-aware re-check (see write_file for why).
    koda_sandbox::fs::verify_mutation_safe(&resolved, &allowed_mutation_roots(project_root))
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Read through the FileSystem abstraction so the sandbox can gate it.
    let raw_bytes = fs
        .read(&resolved, None)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", resolved.display(), e))?;
    let mut content = String::from_utf8(raw_bytes)
        .map_err(|e| anyhow::anyhow!("File '{}' is not valid UTF-8: {}", path_str, e))?;

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

    fs.write(&resolved, content.as_bytes())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to write {}: {}", resolved.display(), e))?;

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

    // #1281: symlink-aware re-check before any unlink. delete is the
    // most dangerous of the three: a symlink swap could trick us into
    // unlinking the wrong path (though `remove_file` on a symlink
    // removes the link itself, so the blast radius is small — we still
    // want a clear error rather than silent surprise).
    koda_sandbox::fs::verify_mutation_safe(&resolved, &allowed_mutation_roots(project_root))
        .map_err(|e| anyhow::anyhow!("{e}"))?;

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
///
/// ## Output format (#1094)
///
/// Every output begins with a one-line header naming the directory
/// that was listed:
///
/// ```text
/// Listing: /Users/lijun/repo (17 entries)
/// d monitor
/// d math_puzzles
///   README.md
/// ...
/// ```
///
/// The header is essential when the model fires several `List` calls
/// in parallel: pre-#1094 the outputs were unattributable bare
/// basenames, and an `(empty directory)` reply could not be matched
/// to a request. The header costs one extra line per call and makes
/// every reply self-identifying.
///
/// Entries are sorted: directories first, then files, alphabetical
/// within each group. This is stable across runs and across
/// filesystems (pre-#1094 the order was whatever the OS returned
/// from `read_dir`, which differs between APFS, ext4, and tmpfs).
pub async fn list_files(project_root: &Path, args: &Value, max_entries: usize) -> Result<String> {
    let path_str = args["file_path"]
        .as_str()
        .or_else(|| args["path"].as_str())
        .unwrap_or(".");
    let recursive = args["recursive"].as_bool().unwrap_or(false);
    let resolved = resolve_read_path(project_root, path_str)?;
    // (is_dir, display_name) pairs so we can sort directories-first,
    // alphabetical, before formatting with the "d " / "  " prefix.
    // Sorting on the formatted strings would put files ("  foo")
    // before directories ("d foo") because space (0x20) < 'd' (0x64),
    // which is the opposite of what we want.
    let mut entries: Vec<(bool, String)> = Vec::new();
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
            entries.push((path.is_dir(), relative.display().to_string()));
            total_count += 1;
            if entries.len() >= max_entries {
                break;
            }
        }
    } else {
        let mut reader = tokio::fs::read_dir(&resolved).await?;
        while let Some(entry) = reader.next_entry().await? {
            let ft = entry.file_type().await?;
            entries.push((
                ft.is_dir(),
                entry.file_name().to_string_lossy().into_owned(),
            ));
            total_count += 1;
            if entries.len() >= max_entries {
                break;
            }
        }
    }

    // Sort: directories first (true > false reversed), then alphabetical.
    // Stable sort preserves insertion order for ties, which doesn't
    // matter once we sort by name too — every output is fully
    // determined by the directory contents.
    entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

    let header = format!("Listing: {}", resolved.display());

    if entries.is_empty() {
        Ok(format!("{header}\n(empty directory)"))
    } else {
        let formatted: Vec<String> = entries
            .into_iter()
            .map(|(is_dir, name)| {
                let prefix = if is_dir { "d " } else { "  " };
                format!("{prefix}{name}")
            })
            .collect();
        let header = format!("{header} ({total_count} entries)");
        if total_count >= max_entries {
            Ok(format!(
                "{header}\n{}\n\n... [CAPPED at {max_entries} entries. Use a subdirectory path to narrow results.]",
                formatted.join("\n")
            ))
        } else {
            Ok(format!("{header}\n{}", formatted.join("\n")))
        }
    }
}

// =============================================================
// Tool trait implementations (#1265 item 5, PR-4/N).
//
// Each struct delegates to the corresponding free function above
// — the functions stay public because tests in `tests/file_tools_test.rs`
// call them directly. The structs are the canonical entry point
// for production dispatch (via `ToolCatalog::get_tool` → `Tool::execute`).
//
// Why structs *and* free functions, instead of moving the bodies
// into the trait impl?
//
//   1. Test stability. `tests/file_tools_test.rs` exercises
//      `file_tools::edit_file` directly with custom fixtures. Moving
//      the body into a trait method would force test churn for no
//      win — the existing tests already cover the bodies and
//      changing them is out of scope for this PR.
//   2. Optionality. Other code paths (e.g. future scripted
//      pre/post hooks) may want to call the file-op functions
//      without going through the trait. Keeping them public costs
//      nothing.
//   3. Reviewability. This PR's diff stays focused on "add the
//      trait seam"; the trait impls are mechanical 4-line forwarders
//      that any reviewer can read top-to-bottom.
//
// `definition()` for each struct re-derives its definition from the
// existing `definitions()` aggregator — single source of truth, no
// risk of drift. We `expect()` because the names are compile-time
// literals that `definitions()` is *required* to contain (an unwrap
// here would crash on registry construction, which is exactly when
// you want to find out).
// =============================================================

use crate::tools::{Tool, ToolEffect, ToolExecCtx, ToolResult};
use async_trait::async_trait;

/// Helper for the trait impls below — looks up a definition by
/// name from the centralized `definitions()` aggregator. Panics if
/// the name isn't registered, which can only happen if `definitions()`
/// drifted out of sync with the structs below — a bug worth crashing
/// the registry's construction over.
fn def(name: &str) -> ToolDefinition {
    definitions()
        .into_iter()
        .find(|d| d.name == name)
        .unwrap_or_else(|| panic!("file_tools::definitions() is missing {name:?}"))
}

/// `Read` — read a file's contents.
pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "Read"
    }
    fn definition(&self) -> ToolDefinition {
        def("Read")
    }
    fn classify(&self, _args: &Value) -> ToolEffect {
        ToolEffect::ReadOnly
    }
    async fn execute(&self, ctx: &ToolExecCtx<'_>, args: &Value) -> ToolResult {
        let r = read_file(ctx.project_root, args, ctx.read_cache, ctx.fs).await;
        crate::tools::wrap_result(r)
    }
}

/// `Write` — create or overwrite a file.
pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "Write"
    }
    fn definition(&self) -> ToolDefinition {
        def("Write")
    }
    fn classify(&self, _args: &Value) -> ToolEffect {
        ToolEffect::LocalMutation
    }
    fn extract_undo_path(&self, args: &Value) -> Option<std::path::PathBuf> {
        extract_file_path_arg(args)
    }
    async fn execute(&self, ctx: &ToolExecCtx<'_>, args: &Value) -> ToolResult {
        let r = write_file(ctx.project_root, args, ctx.fs).await;
        crate::tools::wrap_result(r)
    }
}

/// `Edit` — in-place find/replace edit on an existing file.
pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "Edit"
    }
    fn definition(&self) -> ToolDefinition {
        def("Edit")
    }
    fn classify(&self, _args: &Value) -> ToolEffect {
        ToolEffect::LocalMutation
    }
    fn extract_undo_path(&self, args: &Value) -> Option<std::path::PathBuf> {
        extract_file_path_arg(args)
    }
    async fn execute(&self, ctx: &ToolExecCtx<'_>, args: &Value) -> ToolResult {
        let r = edit_file(ctx.project_root, args, ctx.read_cache, ctx.fs).await;
        crate::tools::wrap_result(r)
    }
}

/// `Delete` — remove a file.
pub struct DeleteTool;

#[async_trait]
impl Tool for DeleteTool {
    fn name(&self) -> &'static str {
        "Delete"
    }
    fn definition(&self) -> ToolDefinition {
        def("Delete")
    }
    fn classify(&self, _args: &Value) -> ToolEffect {
        // Matches the pre-#1265 `tools::classify_tool` arm — Delete
        // is irreversible-without-undo, the strictest gating tier.
        ToolEffect::Destructive
    }
    fn extract_undo_path(&self, args: &Value) -> Option<std::path::PathBuf> {
        extract_file_path_arg(args)
    }
    async fn execute(&self, ctx: &ToolExecCtx<'_>, args: &Value) -> ToolResult {
        let r = delete_file(ctx.project_root, args).await;
        crate::tools::wrap_result(r)
    }
}

/// `List` — directory listing.
pub struct ListTool;

#[async_trait]
impl Tool for ListTool {
    fn name(&self) -> &'static str {
        "List"
    }
    fn definition(&self) -> ToolDefinition {
        def("List")
    }
    fn classify(&self, _args: &Value) -> ToolEffect {
        ToolEffect::ReadOnly
    }
    async fn execute(&self, ctx: &ToolExecCtx<'_>, args: &Value) -> ToolResult {
        let r = list_files(ctx.project_root, args, ctx.caps.list_entries).await;
        crate::tools::wrap_result(r)
    }
}

/// Pull `file_path` (or its `path` alias) out of an args object.
/// Used by `Write`/`Edit`/`Delete`'s `extract_undo_path` — same
/// extraction logic that lives in `undo::extract_file_path` today.
/// In the cleanup PR, `undo::extract_file_path` will delegate here
/// (or be deleted entirely once all mutating tools have migrated).
fn extract_file_path_arg(args: &Value) -> Option<std::path::PathBuf> {
    args.get("file_path")
        .or_else(|| args.get("path"))
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use koda_sandbox::fs::LocalFileSystem;
    use std::collections::HashMap;
    use std::sync::Mutex;

    fn cache() -> super::super::FileReadCache {
        std::sync::Arc::new(Mutex::new(HashMap::new()))
    }

    fn fs() -> LocalFileSystem {
        LocalFileSystem::new()
    }

    // ── Read ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn read_file_basic() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("hello.txt");
        std::fs::write(&f, "line1\nline2\nline3").unwrap();

        let args = json!({"file_path": f.to_string_lossy()});
        let result = read_file(tmp.path(), &args, &cache(), &fs()).await.unwrap();
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
        let result = read_file(tmp.path(), &args, &cache(), &fs()).await.unwrap();
        assert!(result.contains("line 50"));
        assert!(result.contains("line 52"));
        assert!(!result.contains("line 53"));
    }

    #[tokio::test]
    async fn read_file_nonexistent_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let args = json!({"file_path": "does_not_exist.txt"});
        let result = read_file(tmp.path(), &args, &cache(), &fs()).await;
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
        let r1 = read_file(tmp.path(), &args, &c, &fs()).await.unwrap();
        assert!(r1.contains("original content"));

        // Second read — same file, same mtime → stale-read.
        let r2 = read_file(tmp.path(), &args, &c, &fs()).await.unwrap();
        assert!(r2.contains("unchanged"), "expected stale-read: {r2}");
    }

    #[tokio::test]
    async fn read_file_missing_path_arg_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let args = json!({});
        let result = read_file(tmp.path(), &args, &cache(), &fs()).await;
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
        let result = read_file(tmp.path(), &args, &cache(), &fs()).await.unwrap();
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
        let result = read_file(&project, &args, &cache(), &fs()).await;
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
        let result = write_file(tmp.path(), &args, &fs()).await.unwrap();
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
        write_file(tmp.path(), &args, &fs()).await.unwrap();
        assert!(tmp.path().join("a/b/c/deep.txt").exists());
    }

    #[tokio::test]
    async fn write_file_refuses_overwrite_without_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("existing.txt");
        std::fs::write(&f, "original").unwrap();

        let args = json!({"file_path": "existing.txt", "content": "replaced"});
        let result = write_file(tmp.path(), &args, &fs()).await;
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
        write_file(tmp.path(), &args, &fs()).await.unwrap();
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
        let result = edit_file(tmp.path(), &args, &cache(), &fs()).await.unwrap();
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
        let result = edit_file(tmp.path(), &args, &cache(), &fs()).await.unwrap();
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
        let err = edit_file(tmp.path(), &args, &cache(), &fs())
            .await
            .unwrap_err();
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
        let result = edit_file(tmp.path(), &args, &cache(), &fs()).await;
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
        let result = edit_file(tmp.path(), &args, &cache(), &fs()).await;
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
        edit_file(tmp.path(), &args, &cache(), &fs()).await.unwrap();
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
        let err = edit_file(tmp.path(), &args, &c, &fs()).await.unwrap_err();
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

    // ── #1094: List output must self-identify the directory ──────────

    /// Regression for #1094: every output must begin with a
    /// `Listing: <path>` header so parallel `List` calls can be
    /// attributed back to their requests. Pre-#1094 the output was
    /// just bare basenames with no parent context.
    #[tokio::test]
    async fn list_files_includes_directory_header() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "").unwrap();

        let args = json!({"directory": "."});
        let result = list_files(tmp.path(), &args, 200).await.unwrap();
        let first_line = result.lines().next().unwrap();
        assert!(
            first_line.starts_with("Listing: "),
            "first line must be a `Listing: ...` header, got: {first_line:?}"
        );
        assert!(
            first_line.contains("(1 entries)"),
            "header must include entry count, got: {first_line:?}"
        );
    }

    /// The empty-directory case is the most ambiguous one for parallel
    /// calls (pre-#1094 it returned just `(empty directory)` with no
    /// indication of WHICH directory). The header must still appear.
    #[tokio::test]
    async fn list_files_empty_directory_still_has_header() {
        let tmp = tempfile::tempdir().unwrap();
        let empty = tmp.path().join("empty_subdir");
        std::fs::create_dir(&empty).unwrap();

        let args = json!({"file_path": "empty_subdir"});
        let result = list_files(tmp.path(), &args, 200).await.unwrap();
        let mut lines = result.lines();
        let header = lines.next().unwrap();
        assert!(
            header.starts_with("Listing: ") && header.contains("empty_subdir"),
            "empty-dir header must name the directory, got: {header:?}"
        );
        assert_eq!(lines.next(), Some("(empty directory)"));
        // No entry count for empty case (would be redundant with
        // "(empty directory)" on the next line).
        assert!(
            !header.contains("entries"),
            "empty header should omit count, got: {header:?}"
        );
    }

    /// Entries must be sorted: directories first, then files,
    /// alphabetical within each group. Pre-#1094 the order was
    /// whatever `read_dir` returned, which differs across
    /// filesystems and made the output unstable across machines.
    #[tokio::test]
    async fn list_files_sorts_dirs_first_then_alphabetical() {
        let tmp = tempfile::tempdir().unwrap();
        // Create in deliberately-scrambled order to make sure we're
        // not just getting lucky with insertion order.
        std::fs::write(tmp.path().join("zzz.txt"), "").unwrap();
        std::fs::create_dir(tmp.path().join("yankee")).unwrap();
        std::fs::write(tmp.path().join("aaa.txt"), "").unwrap();
        std::fs::create_dir(tmp.path().join("alpha")).unwrap();

        let args = json!({"directory": "."});
        let result = list_files(tmp.path(), &args, 200).await.unwrap();
        let body: Vec<&str> = result.lines().skip(1).collect();
        assert_eq!(
            body,
            vec!["d alpha", "d yankee", "  aaa.txt", "  zzz.txt"],
            "entries must be sorted dirs-first then alphabetical: {result}"
        );
    }

    /// The capped-output path must also carry the header, otherwise
    /// the model loses attribution exactly when the output is most
    /// confusing (long lists are the ones that get truncated).
    #[tokio::test]
    async fn list_files_capped_output_carries_header() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..20 {
            std::fs::write(tmp.path().join(format!("file_{i:02}.txt")), "").unwrap();
        }

        let args = json!({"file_path": "."});
        let result = list_files(tmp.path(), &args, 5).await.unwrap();
        let first_line = result.lines().next().unwrap();
        assert!(
            first_line.starts_with("Listing: "),
            "capped output must still start with header: {first_line:?}"
        );
        assert!(
            result.contains("CAPPED"),
            "capped output must still include cap message: {result}"
        );
    }

    // ========================================================================
    // #1281: symlink-mutation guards
    //
    // The unit tests in koda_sandbox::fs::policy already cover the verifier
    // in isolation. These integration tests prove the wiring — that
    // write_file / edit_file / delete_file actually call the verifier and
    // refuse to mutate when an attacker plants an escaping symlink under
    // the project root. Each test asserts BOTH that the tool errors AND
    // that the file the symlink points to is unchanged.
    //
    // Why /etc/hosts as the canonical outside target?
    //   `allowed_mutation_roots` includes /tmp, /var/tmp, AND
    //   std::env::temp_dir() (e.g. /var/folders/.../T on macOS). So a
    //   symlink from one tempdir to another tempdir is correctly
    //   considered safe by the verifier. To exercise the escape path we
    //   need a target outside *every* allowed root. /etc exists on
    //   every Unix CI and is read-only for non-root users — so even if
    //   the verifier had a bug, the tool couldn't actually clobber it.
    // ========================================================================

    /// Path used as the canonical "outside every allowed root" target.
    /// /etc/hosts exists on every Unix host and is outside all tempdirs.
    #[cfg(unix)]
    const OUTSIDE_TARGET: &str = "/etc/hosts";

    /// Skip a test if /etc/hosts somehow doesn't exist (very stripped
    /// container?) so we don't false-fail in exotic environments.
    #[cfg(unix)]
    fn outside_or_skip() -> Option<std::path::PathBuf> {
        let p = std::path::PathBuf::from(OUTSIDE_TARGET);
        if p.exists() { Some(p) } else { None }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_file_refuses_symlink_escaping_project_root() {
        let Some(outside_file) = outside_or_skip() else {
            return;
        };
        let original = std::fs::read(&outside_file).unwrap();

        let project = tempfile::tempdir().unwrap();
        let link = project.path().join("sneaky.txt");
        std::os::unix::fs::symlink(&outside_file, &link).unwrap();

        let fs = koda_sandbox::fs::LocalFileSystem::new();
        let args = json!({
            "file_path": link.to_str().unwrap(),
            "content": "clobber",
            "overwrite": true,
        });
        let err = write_file(project.path(), &args, &fs)
            .await
            .expect_err("write through escaping symlink must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("symlink") || msg.contains("outside"),
            "error should mention symlink/outside: {msg}"
        );
        // Critical: the file the link pointed to is byte-identical.
        assert_eq!(
            std::fs::read(&outside_file).unwrap(),
            original,
            "the verifier must reject BEFORE any byte touches the outside file"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_file_refuses_symlinked_parent_dir_escaping_project_root() {
        // <project>/escape -> /etc, then write to <project>/escape/koda-attack.txt
        // should be refused because the parent canonicalizes outside the
        // project AND outside every allowed tempdir.
        let Some(_) = outside_or_skip() else { return };
        let outside_dir = std::path::PathBuf::from("/etc");
        if !outside_dir.is_dir() {
            return;
        }

        let project = tempfile::tempdir().unwrap();
        let escape = project.path().join("escape");
        std::os::unix::fs::symlink(&outside_dir, &escape).unwrap();

        let target = escape.join("koda-attack.txt");
        let fs = koda_sandbox::fs::LocalFileSystem::new();
        let args = json!({
            "file_path": target.to_str().unwrap(),
            "content": "clobber",
        });
        let err = write_file(project.path(), &args, &fs)
            .await
            .expect_err("write through escaping parent symlink must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("escape") || msg.contains("outside") || msg.contains("symlink"),
            "error should explain the escape: {msg}"
        );
        assert!(
            !outside_dir.join("koda-attack.txt").exists(),
            "outside dir must remain untouched"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn edit_file_refuses_symlink_escaping_project_root() {
        let Some(outside_file) = outside_or_skip() else {
            return;
        };
        let original = std::fs::read(&outside_file).unwrap();

        let project = tempfile::tempdir().unwrap();
        let link = project.path().join("alias.txt");
        std::os::unix::fs::symlink(&outside_file, &link).unwrap();

        let cache = cache();
        let fs = koda_sandbox::fs::LocalFileSystem::new();
        // Use a needle that's almost certainly not in /etc/hosts so even
        // if the verifier failed open, the edit would no-op rather than
        // mutate. Defense in depth.
        let args = json!({
            "file_path": link.to_str().unwrap(),
            "replacements": [{"old_str": "KODA_ATTACK_NEEDLE_XYZ", "new_str": "PWNED"}],
        });
        let err = edit_file(project.path(), &args, &cache, &fs)
            .await
            .expect_err("edit through escaping symlink must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("symlink") || msg.contains("outside"),
            "error should mention symlink/outside: {msg}"
        );
        assert_eq!(std::fs::read(&outside_file).unwrap(), original);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn delete_file_refuses_symlink_escaping_project_root() {
        let Some(outside_file) = outside_or_skip() else {
            return;
        };

        let project = tempfile::tempdir().unwrap();
        let link = project.path().join("to_delete");
        std::os::unix::fs::symlink(&outside_file, &link).unwrap();

        let args = json!({ "file_path": link.to_str().unwrap() });
        let err = delete_file(project.path(), &args)
            .await
            .expect_err("delete through escaping symlink must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("symlink") || msg.contains("outside"),
            "error should mention symlink/outside: {msg}"
        );
        // Both the link AND the target must still exist.
        assert!(outside_file.exists(), "outside target must not be deleted");
        assert!(link.exists(), "link itself must still exist");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_file_allows_in_project_symlink() {
        // Counter-example: a symlink whose target is INSIDE the
        // project root must keep working. Lots of repos use
        // `examples/latest -> v3/` patterns; we don't want this PR to
        // regress them.
        let project = tempfile::tempdir().unwrap();
        let real_dir = project.path().join("v3");
        std::fs::create_dir(&real_dir).unwrap();
        let real_file = real_dir.join("config.toml");
        std::fs::write(&real_file, b"old").unwrap();

        let alias_dir = project.path().join("latest");
        std::os::unix::fs::symlink(&real_dir, &alias_dir).unwrap();
        let via_link = alias_dir.join("config.toml");

        let fs = koda_sandbox::fs::LocalFileSystem::new();
        let args = json!({
            "file_path": via_link.to_str().unwrap(),
            "content": "new",
            "overwrite": true,
        });
        write_file(project.path(), &args, &fs)
            .await
            .expect("in-project symlink write must succeed");
        assert_eq!(std::fs::read(&real_file).unwrap(), b"new");
    }

    // =========================================================
    // Tool trait invariants (#1265 item 5, PR-4/N).
    //
    // These tests live alongside the structs they exercise so any
    // future contributor adding a file-op tool sees the contract
    // they need to maintain. They cover four classes of bug the
    // trait migration is *supposed* to make impossible:
    //
    //   1. `name()` and `definition().name` drift apart — breaks
    //      registry lookup.
    //   2. `classify()` returns the wrong tier — breaks approval
    //      gating (this is what the duplicated `is_mutating_tool`
    //      lists allowed pre-PR).
    //   3. `extract_undo_path()` returns `None` for a mutating tool
    //      — breaks `/undo`.
    //   4. `extract_undo_path()` returns `Some` for a read-only
    //      tool — wastes snapshot work and pollutes the undo stack.
    // =========================================================

    /// All five file-op trait impls plus their expected metadata.
    /// Centralized so the assertions below stay table-driven.
    fn cohort() -> Vec<(
        &'static str,
        Box<dyn crate::tools::Tool>,
        ToolEffect,
        bool, // expects an undo path when args carry `file_path`
    )> {
        vec![
            ("Read", Box::new(ReadTool), ToolEffect::ReadOnly, false),
            (
                "Write",
                Box::new(WriteTool),
                ToolEffect::LocalMutation,
                true,
            ),
            ("Edit", Box::new(EditTool), ToolEffect::LocalMutation, true),
            (
                "Delete",
                Box::new(DeleteTool),
                ToolEffect::Destructive,
                true,
            ),
            ("List", Box::new(ListTool), ToolEffect::ReadOnly, false),
        ]
    }

    #[test]
    fn name_matches_definition_for_every_file_op() {
        // Registry lookup is keyed by `name()` and the LLM sees
        // `definition().name`. Drift = silent dispatch failure.
        for (expected, tool, _, _) in cohort() {
            assert_eq!(tool.name(), expected);
            assert_eq!(tool.definition().name, expected);
        }
    }

    #[test]
    fn classify_matches_pre_migration_tiers() {
        // Locks the classification each tool was assigned by the
        // pre-#1265 `tools::classify_tool` arms. Any change here
        // is a behavior change — must be intentional.
        for (name, tool, expected, _) in cohort() {
            assert_eq!(
                tool.classify(&serde_json::json!({})),
                expected,
                "{name} classified incorrectly",
            );
        }
    }

    #[test]
    fn extract_undo_path_only_for_mutating_tools() {
        // Mutating cohort returns Some(path); read-only cohort returns
        // None even when `file_path` is present. Prevents pollution
        // of the undo stack with read-only operations.
        let args = serde_json::json!({"file_path": "src/main.rs"});
        for (name, tool, _, expects_path) in cohort() {
            let actual = tool.extract_undo_path(&args);
            assert_eq!(
                actual.is_some(),
                expects_path,
                "{name} undo-path extraction wrong",
            );
            if expects_path {
                assert_eq!(actual, Some(std::path::PathBuf::from("src/main.rs")));
            }
        }
    }

    #[test]
    fn extract_undo_path_accepts_path_alias() {
        // The legacy `undo::extract_file_path` accepts both
        // `file_path` and `path`. Migrated tools must too — some
        // older test fixtures and a few prompt examples use `path`.
        let args = serde_json::json!({"path": "lib.rs"});
        assert_eq!(
            WriteTool.extract_undo_path(&args),
            Some(std::path::PathBuf::from("lib.rs")),
        );
        assert_eq!(
            EditTool.extract_undo_path(&args),
            Some(std::path::PathBuf::from("lib.rs")),
        );
        assert_eq!(
            DeleteTool.extract_undo_path(&args),
            Some(std::path::PathBuf::from("lib.rs")),
        );
    }

    #[test]
    fn extract_undo_path_handles_missing_args_gracefully() {
        // No path at all — must return None, not panic. (A common
        // failure mode of "unwrap because args _should_ have it".)
        for tool in [
            Box::new(WriteTool) as Box<dyn Tool>,
            Box::new(EditTool),
            Box::new(DeleteTool),
        ] {
            assert!(tool.extract_undo_path(&serde_json::json!({})).is_none());
            // Wrong-typed value — also graceful.
            assert!(
                tool.extract_undo_path(&serde_json::json!({"file_path": 42}))
                    .is_none()
            );
        }
    }

    #[tokio::test]
    async fn trait_dispatch_executes_read() {
        // End-to-end proof that a migrated tool's full code path
        // (definition + classification + execution) works through
        // the trait. Mirrors `dyn_dispatch_executes_correct_tool`
        // from PR-3's seam tests but on a *real* tool.
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("x.txt");
        std::fs::write(&f, "hi").unwrap();

        let cache = cache();
        let fs = fs();
        let caps = crate::output_caps::OutputCaps::for_context(100_000);
        let ctx = crate::tools::ToolExecCtx {
            project_root: tmp.path(),
            read_cache: &cache,
            fs: &fs,
            caps: &caps,
            sink: None,
            caller_spawner: None,
        };
        let tool: Box<dyn Tool> = Box::new(ReadTool);
        let result = tool
            .execute(&ctx, &serde_json::json!({"file_path": "x.txt"}))
            .await;
        assert!(result.success, "trait dispatch failed: {}", result.output);
        assert!(result.output.contains("hi"));
    }

    #[tokio::test]
    async fn trait_dispatch_executes_write_and_writes_file() {
        // Verifies `WriteTool::execute` actually mutates the FS —
        // not just "returns success". The bug class this catches is
        // a trait impl that delegates to the wrong free function.
        let tmp = tempfile::tempdir().unwrap();
        let cache = cache();
        let fs = fs();
        let caps = crate::output_caps::OutputCaps::for_context(100_000);
        let ctx = crate::tools::ToolExecCtx {
            project_root: tmp.path(),
            read_cache: &cache,
            fs: &fs,
            caps: &caps,
            sink: None,
            caller_spawner: None,
        };
        let tool: Box<dyn Tool> = Box::new(WriteTool);
        let result = tool
            .execute(
                &ctx,
                &serde_json::json!({"file_path": "new.txt", "content": "abc"}),
            )
            .await;
        assert!(result.success, "{}", result.output);
        assert_eq!(std::fs::read(tmp.path().join("new.txt")).unwrap(), b"abc");
    }

    #[tokio::test]
    async fn trait_dispatch_error_path_returns_failure_result() {
        // Errors from the underlying free function must land as
        // `success: false` (the contract `wrap_result` enforces).
        let tmp = tempfile::tempdir().unwrap();
        let cache = cache();
        let fs = fs();
        let caps = crate::output_caps::OutputCaps::for_context(100_000);
        let ctx = crate::tools::ToolExecCtx {
            project_root: tmp.path(),
            read_cache: &cache,
            fs: &fs,
            caps: &caps,
            sink: None,
            caller_spawner: None,
        };
        let tool: Box<dyn Tool> = Box::new(ReadTool);
        let result = tool
            .execute(&ctx, &serde_json::json!({"file_path": "does-not-exist"}))
            .await;
        assert!(!result.success);
        assert!(
            result.output.starts_with("Error:"),
            "error output should be prefixed: {}",
            result.output
        );
    }
}
