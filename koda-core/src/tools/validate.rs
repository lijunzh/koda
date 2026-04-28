//! Pre-flight validation for tool calls.
//!
//! Runs **before** the approval prompt so we never ask the user to approve
//! an operation that will inevitably fail. Cheap checks only — no mutations.
//!
//! ## What it validates
//!
//! - **Write**: target path resolves within project root
//! - **Write (overwrite)**: file exists and `overwrite: true` is set
//! - **Edit**: file exists, `old_str` is found, `old_str` is unique
//!   (unless `replace_all: true`), `new_str` does not contain omission
//!   placeholders (e.g. `// rest of code ...`)
//! - **Delete**: file exists
//! - **Bash**: command is non-empty
//!
//! Validation errors are returned as tool results (not panics), so the
//! model sees the error and can self-correct.

use super::safe_resolve_path;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Format a duration into a compact human-readable age string.
///
/// ```text
/// <5s  → "just now"
/// <60s → "12s ago"
/// else → "3m ago"
/// ```
fn fmt_age(age: std::time::Duration) -> String {
    let secs = age.as_secs();
    if secs < 5 {
        "just now".to_string()
    } else if secs < 60 {
        format!("{secs}s ago")
    } else {
        format!("{}m ago", secs / 60)
    }
}

/// Validate a tool call before approval.
///
/// `read_cache` is the session file-read cache.  When provided, `validate_edit`
/// uses it to detect files that have been modified on disk since the model last
/// read them — catching the most common source of lost-context edits.
///
/// `last_writer` and `last_bash` add context to staleness errors: instead of a
/// generic "file was modified" message the model sees which tool was responsible
/// and how long ago it ran (#804 item 7).
///
/// Returns `None` if the call looks valid, or `Some(error_message)` describing
/// why it will fail. The error message is fed back to the model so it can
/// self-correct without consuming an approval prompt.
pub async fn validate_tool_call(
    tool_name: &str,
    args: &serde_json::Value,
    project_root: &Path,
    read_cache: Option<&super::FileReadCache>,
    last_writer: Option<&super::LastWriterCache>,
    last_bash: Option<&super::LastBashCache>,
) -> Option<String> {
    match tool_name {
        "Edit" => validate_edit(args, project_root, read_cache, last_writer, last_bash).await,
        "Write" => validate_write(args, project_root).await,
        "Delete" => validate_delete(args, project_root).await,
        "Bash" => validate_bash(args),
        _ => None,
    }
}

/// DRY wrapper: pull the three caches from a `ToolRegistry` and run
/// the standard pre-flight validation against the given root.
///
/// Three call sites used to inline the same 9-line cache-pull-then-
/// call-validate dance:
///
///   * `tool_dispatch::validate_then_execute_one_tool` (parallel +
///     split-batch arms, validates after auto-approval)
///   * `tool_dispatch::execute_tools_sequential` (sequential arm,
///     validates before approval prompting so we don't bother the
///     user with doomed prompts)
///   * `sub_agent_dispatch::execute_sub_agent` (sub-agent loop,
///     validates before per-tool approval branch)
///
/// `project_root` is taken explicitly because the sub-agent path
/// validates against its (possibly worktree-shifted) effective root,
/// not the registry's `project_root`. Everything else is mechanical
/// cache plumbing that's identical across call sites.
pub async fn validate_with_registry(
    registry: &super::ToolRegistry,
    tool_name: &str,
    args: &serde_json::Value,
    project_root: &Path,
) -> Option<String> {
    let read_cache = registry.file_read_cache();
    let last_writer = registry.last_writer_cache();
    let last_bash = registry.last_bash_cache();
    validate_tool_call(
        tool_name,
        args,
        project_root,
        Some(&read_cache),
        Some(&last_writer),
        Some(&last_bash),
    )
    .await
}

/// Build a parenthetical hint describing what tool last modified a file.
///
/// Priority:
/// 1. A recorded Write/Edit entry for this exact path  → " (last written by Edit 3s ago)"
/// 2. A recent Bash invocation (possible indirect modifier) → " (Bash ran 2s ago: `cargo fmt ...`)"
/// 3. Neither known                                        → "" (empty; generic message is fine)
fn writer_hint(
    resolved: &PathBuf,
    last_writer: Option<&super::LastWriterCache>,
    last_bash: Option<&super::LastBashCache>,
) -> String {
    // Check for a direct Write/Edit record for this path.
    if let Some(lw) = last_writer
        && let Ok(guard) = lw.lock()
        && let Some((tool, when)) = guard.get(resolved)
    {
        return format!(" (last written by {} {})", tool, fmt_age(when.elapsed()));
    }
    // Fall back to the most recent Bash call — it may have modified the file
    // indirectly (formatter, build script, etc.).
    if let Some(lb) = last_bash
        && let Ok(guard) = lb.lock()
        && let Some((snippet, when)) = guard.as_ref()
    {
        return format!(" (Bash ran {}: `{}`)", fmt_age(when.elapsed()), snippet);
    }
    String::new()
}

/// Edit: file must exist, replacements must be non-empty, each old_str must
/// be non-empty and actually present in the file.  Also warns if the file has
/// been modified on disk since the model last read it.
async fn validate_edit(
    args: &serde_json::Value,
    project_root: &Path,
    read_cache: Option<&super::FileReadCache>,
    last_writer: Option<&super::LastWriterCache>,
    last_bash: Option<&super::LastBashCache>,
) -> Option<String> {
    let path_str = args["file_path"]
        .as_str()
        .or_else(|| args["path"].as_str())
        .unwrap_or("");
    if path_str.is_empty() {
        return Some("Missing 'file_path' argument.".into());
    }

    let resolved = match safe_resolve_path(project_root, path_str) {
        Ok(p) => p,
        Err(e) => return Some(format!("Invalid path: {e}")),
    };

    let replacements = match args["replacements"].as_array() {
        Some(arr) if !arr.is_empty() => arr,
        Some(_) => return Some("'replacements' array is empty.".into()),
        None => return Some("Missing 'replacements' argument.".into()),
    };

    // File must exist (Edit is not Write)
    let content = match tokio::fs::read_to_string(&resolved).await {
        Ok(c) => c,
        Err(e) => {
            return Some(format!(
                "Cannot read '{}': {e}. Use Write to create new files.",
                path_str
            ));
        }
    };

    // Stale-file detection: warn if the file has been modified on disk since
    // the model last read it.  Only fires when a full-read cache entry exists
    // and the current mtime differs from the cached one.
    if let Some(cache) = read_cache
        && let Ok(meta) = tokio::fs::metadata(&resolved).await
    {
        let current_mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let cache_key = format!("{}:None:None", resolved.display());
        let cached_mtime = cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&cache_key)
            .map(|(_, mtime, _)| *mtime);
        if let Some(cm) = cached_mtime
            && cm != current_mtime
        {
            // Build a context hint naming the responsible tool so the model
            // doesn't have to guess why the file changed (#804 item 7).
            let hint = writer_hint(&resolved, last_writer, last_bash);
            return Some(format!(
                "File '{path_str}' has been modified on disk since you last read it{hint}. \
                 Read it again to get the current content before editing.",
            ));
        }
    }

    // Validate each replacement
    for (i, replacement) in replacements.iter().enumerate() {
        let old_str = match replacement["old_str"].as_str() {
            Some(s) if !s.is_empty() => s,
            Some(_) => {
                return Some(format!("Replacement {i}: 'old_str' cannot be empty."));
            }
            None => {
                return Some(format!("Replacement {i}: missing 'old_str'."));
            }
        };

        let new_str = match replacement["new_str"].as_str() {
            Some(s) => s,
            None => {
                return Some(format!("Replacement {i}: missing 'new_str'."));
            }
        };

        // Omission placeholder detection: catch lazy model output like
        // "// rest of code ..." or "(unchanged methods ...)" before it
        // silently replaces real code with a comment.
        if let Some(msg) = detect_new_omission_placeholder(old_str, new_str, i) {
            return Some(msg);
        }

        if !content.contains(old_str) {
            // Try fuzzy (whitespace-normalized) before hard-failing.
            let ranges = super::fuzzy::fuzzy_match_ranges(old_str, &content);
            if ranges.is_empty() {
                return Some(format!(
                    "Replacement {i}: 'old_str' not found in '{}'. \
                     Read the file first to get the exact text.",
                    path_str
                ));
            }
            // 2+ fuzzy matches is also a problem — flag it now so the model
            // can tighten the snippet before burning an approval prompt.
            if ranges.len() > 1 {
                return Some(format!(
                    "Replacement {i}: 'old_str' is ambiguous — {} fuzzy matches in '{}'. \
                     Use a more specific snippet.",
                    ranges.len(),
                    path_str
                ));
            }
        }
    }

    None
}

/// Write: catch overwrite-without-flag before approval.
async fn validate_write(args: &serde_json::Value, project_root: &Path) -> Option<String> {
    let path_str = args["file_path"]
        .as_str()
        .or_else(|| args["path"].as_str())
        .unwrap_or("");
    if path_str.is_empty() {
        return Some("Missing 'file_path' argument.".into());
    }

    if args["content"].as_str().is_none() {
        return Some("Missing 'content' argument.".into());
    }

    let resolved = match safe_resolve_path(project_root, path_str) {
        Ok(p) => p,
        Err(e) => return Some(format!("Invalid path: {e}")),
    };

    let overwrite = args["overwrite"].as_bool().unwrap_or(false);
    if resolved.exists() && !overwrite {
        return Some(format!(
            "File '{}' already exists. Set overwrite=true to replace it, \
             or use Edit for targeted changes.",
            path_str
        ));
    }

    None
}

/// Delete: path must exist.
async fn validate_delete(args: &serde_json::Value, project_root: &Path) -> Option<String> {
    let path_str = args["file_path"]
        .as_str()
        .or_else(|| args["path"].as_str())
        .unwrap_or("");
    if path_str.is_empty() {
        return Some("Missing 'file_path' argument.".into());
    }

    let resolved = match safe_resolve_path(project_root, path_str) {
        Ok(p) => p,
        Err(e) => return Some(format!("Invalid path: {e}")),
    };

    if !resolved.exists() {
        return Some(format!("Path not found: '{path_str}'. Nothing to delete."));
    }

    // Non-empty dir without recursive flag
    if resolved.is_dir() {
        let is_empty = resolved
            .read_dir()
            .map(|mut d| d.next().is_none())
            .unwrap_or(false);
        let recursive = args["recursive"].as_bool().unwrap_or(false);
        if !is_empty && !recursive {
            return Some(format!(
                "Directory '{}' is not empty. Set recursive=true to delete it.",
                path_str
            ));
        }
    }

    None
}

/// Bash: command must be non-empty.
fn validate_bash(args: &serde_json::Value) -> Option<String> {
    let cmd = args["command"]
        .as_str()
        .or_else(|| args["cmd"].as_str())
        .unwrap_or("");
    if cmd.trim().is_empty() {
        return Some("Missing or empty 'command' argument.".into());
    }
    None
}

// ── Omission placeholder detection ────────────────────────────

/// Known omission phrase prefixes (lowercase, before the `...`).
const OMISSION_PREFIXES: &[&str] = &[
    "rest of",
    "rest of code",
    "rest of method",
    "rest of methods",
    "rest of file",
    "rest of function",
    "rest of implementation",
    "existing code",
    "existing implementation",
    "unchanged code",
    "unchanged method",
    "unchanged methods",
    "remaining code",
    "remaining implementation",
];

/// Check if `new_str` introduces an omission placeholder not present in `old_str`.
///
/// Returns an error message if the model is being lazy, or `None` if clean.
fn detect_new_omission_placeholder(
    old_str: &str,
    new_str: &str,
    replacement_idx: usize,
) -> Option<String> {
    let new_placeholders = detect_omission_placeholders(new_str);
    if new_placeholders.is_empty() {
        return None;
    }
    // If old_str already had the same placeholder, the model is preserving
    // an existing comment — that's fine.
    let old_set: HashSet<String> = detect_omission_placeholders(old_str).into_iter().collect();
    for p in &new_placeholders {
        if !old_set.contains(p) {
            return Some(format!(
                "Replacement {replacement_idx}: 'new_str' contains an omission placeholder \
                 ('{p}'). Write the actual code instead of abbreviating with comments."
            ));
        }
    }
    None
}

/// Scan text for lines that look like omission placeholders.
///
/// Recognized patterns:
/// - `// rest of code ...`
/// - `# unchanged methods ...`
/// - `(rest of implementation ...)`
/// - `// (existing code ...)`
///
/// Returns normalized placeholder strings (e.g. `"rest of code ..."`).
fn detect_omission_placeholders(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    for line in text.lines() {
        if let Some(normalized) = normalize_placeholder_line(line) {
            found.push(normalized);
        }
    }
    found
}

/// Try to parse a single line as an omission placeholder.
///
/// Strips comment prefixes (`//`, `#`), optional parentheses, then checks
/// for a known phrase followed by `...`.
fn normalize_placeholder_line(line: &str) -> Option<String> {
    let mut text = line.trim();
    if text.is_empty() {
        return None;
    }

    // Strip comment prefix
    if let Some(rest) = text.strip_prefix("//") {
        text = rest.trim();
    } else if let Some(rest) = text.strip_prefix('#') {
        text = rest.trim();
    }

    // Strip optional parentheses: (rest of code ...)
    if text.starts_with('(') && text.ends_with(')') {
        text = &text[1..text.len() - 1];
        text = text.trim();
    }

    // Must contain "..."
    let ellipsis_pos = text.find("...")?;
    let prefix = text[..ellipsis_pos].trim();
    let suffix = text[ellipsis_pos + 3..].trim();

    // Suffix must be empty or all dots ("...." is fine)
    if !suffix.is_empty() && !suffix.chars().all(|c| c == '.') {
        return None;
    }

    // Normalize whitespace in prefix and check against known phrases
    let normalized: String = prefix.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = normalized.to_lowercase();

    if OMISSION_PREFIXES.contains(&lower.as_str()) {
        Some(format!("{lower} ..."))
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

    fn setup() -> TempDir {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("hello.txt"),
            "line one\nline two\nline three\n",
        )
        .unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        std::fs::write(dir.path().join("subdir/nested.txt"), "nested").unwrap();
        dir
    }

    // ── Edit validation ───────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn edit_valid_replacement() {
        let dir = setup();
        let args = json!({
            "path": "hello.txt",
            "replacements": [{"old_str": "line two", "new_str": "line TWO"}]
        });
        assert!(
            validate_edit(&args, dir.path(), None, None, None)
                .await
                .is_none()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn edit_missing_path() {
        let dir = setup();
        let args = json!({"replacements": [{"old_str": "x", "new_str": "y"}]});
        let err = validate_edit(&args, dir.path(), None, None, None)
            .await
            .unwrap();
        assert!(err.contains("path"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn edit_file_not_found() {
        let dir = setup();
        let args = json!({
            "path": "nope.txt",
            "replacements": [{"old_str": "x", "new_str": "y"}]
        });
        let err = validate_edit(&args, dir.path(), None, None, None)
            .await
            .unwrap();
        assert!(err.contains("Cannot read"), "{err}");
        assert!(err.contains("Write"), "{err}"); // suggests Write
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn edit_empty_replacements() {
        let dir = setup();
        let args = json!({"path": "hello.txt", "replacements": []});
        let err = validate_edit(&args, dir.path(), None, None, None)
            .await
            .unwrap();
        assert!(err.contains("empty"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn edit_empty_old_str() {
        let dir = setup();
        let args = json!({
            "path": "hello.txt",
            "replacements": [{"old_str": "", "new_str": "y"}]
        });
        let err = validate_edit(&args, dir.path(), None, None, None)
            .await
            .unwrap();
        assert!(err.contains("empty"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn edit_old_str_fuzzy_match_passes_validation() {
        // File has trailing spaces; model sends clean needle — should pass pre-flight.
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("hello.txt"),
            "line one   \nline two   \nline three\n",
        )
        .unwrap();
        let args = json!({
            "path": "hello.txt",
            "replacements": [{"old_str": "line two", "new_str": "LINE TWO"}]
        });
        assert!(
            validate_edit(&args, dir.path(), None, None, None)
                .await
                .is_none(),
            "fuzzy match should pass validation"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn edit_old_str_not_found() {
        let dir = setup();
        let args = json!({
            "path": "hello.txt",
            "replacements": [{"old_str": "does not exist", "new_str": "y"}]
        });
        let err = validate_edit(&args, dir.path(), None, None, None)
            .await
            .unwrap();
        assert!(err.contains("not found"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn edit_missing_new_str() {
        let dir = setup();
        let args = json!({
            "path": "hello.txt",
            "replacements": [{"old_str": "line one"}]
        });
        let err = validate_edit(&args, dir.path(), None, None, None)
            .await
            .unwrap();
        assert!(err.contains("new_str"), "{err}");
    }

    // ── Write validation ──────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn write_new_file_valid() {
        let dir = setup();
        let args = json!({"path": "brand_new.txt", "content": "hello"});
        assert!(validate_write(&args, dir.path()).await.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn write_existing_without_overwrite() {
        let dir = setup();
        let args = json!({"path": "hello.txt", "content": "replaced"});
        let err = validate_write(&args, dir.path()).await.unwrap();
        assert!(err.contains("already exists"), "{err}");
        assert!(err.contains("overwrite=true"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn write_existing_with_overwrite() {
        let dir = setup();
        let args = json!({"path": "hello.txt", "content": "replaced", "overwrite": true});
        assert!(validate_write(&args, dir.path()).await.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn write_missing_content() {
        let dir = setup();
        let args = json!({"path": "foo.txt"});
        let err = validate_write(&args, dir.path()).await.unwrap();
        assert!(err.contains("content"), "{err}");
    }

    // ── Delete validation ─────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_valid_file() {
        let dir = setup();
        let args = json!({"path": "hello.txt"});
        assert!(validate_delete(&args, dir.path()).await.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_not_found() {
        let dir = setup();
        let args = json!({"path": "nope.txt"});
        let err = validate_delete(&args, dir.path()).await.unwrap();
        assert!(err.contains("not found"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_nonempty_dir_without_recursive() {
        let dir = setup();
        let args = json!({"path": "subdir"});
        let err = validate_delete(&args, dir.path()).await.unwrap();
        assert!(err.contains("recursive"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_nonempty_dir_with_recursive() {
        let dir = setup();
        let args = json!({"path": "subdir", "recursive": true});
        assert!(validate_delete(&args, dir.path()).await.is_none());
    }

    // ── file_path alias acceptance ──────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn edit_accepts_file_path_param() {
        let dir = setup();
        let args = json!({
            "file_path": "hello.txt",
            "replacements": [{"old_str": "line two", "new_str": "LINE TWO"}]
        });
        assert!(
            validate_edit(&args, dir.path(), None, None, None)
                .await
                .is_none()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn write_accepts_file_path_param() {
        let dir = setup();
        let args = json!({"file_path": "brand_new.txt", "content": "hello"});
        assert!(validate_write(&args, dir.path()).await.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_accepts_file_path_param() {
        let dir = setup();
        let args = json!({"file_path": "hello.txt"});
        assert!(validate_delete(&args, dir.path()).await.is_none());
    }

    // ── Bash validation ───────────────────────────────────────

    #[test]
    fn bash_valid_command() {
        let args = json!({"command": "echo hello"});
        assert!(validate_bash(&args).is_none());
    }

    #[test]
    fn bash_empty_command() {
        let args = json!({"command": ""});
        assert!(validate_bash(&args).unwrap().contains("empty"));
    }

    #[test]
    fn bash_missing_command() {
        let args = json!({});
        assert!(validate_bash(&args).unwrap().contains("empty"));
    }

    #[test]
    fn bash_cmd_alias() {
        let args = json!({"cmd": "ls"});
        assert!(validate_bash(&args).is_none());
    }

    // ── Stale-file detection ──────────────────────────────────

    fn make_cache(path: &std::path::Path, mtime: SystemTime) -> super::super::FileReadCache {
        let cache = super::super::FileReadCache::default();
        let key = format!("{}:None:None", path.display());
        cache.lock().unwrap().insert(key, (0, mtime, String::new()));
        cache
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn edit_stale_file_detected() {
        let dir = setup();
        let file = dir.path().join("hello.txt");
        // Populate cache with a deliberately old mtime (epoch = definitely stale).
        let cache = make_cache(&file, SystemTime::UNIX_EPOCH);
        let args = json!({
            "path": "hello.txt",
            "replacements": [{"old_str": "line two", "new_str": "LINE TWO"}]
        });
        let err = validate_edit(&args, dir.path(), Some(&cache), None, None)
            .await
            .unwrap();
        assert!(err.contains("modified on disk"), "{err}");
        assert!(err.contains("Read it again"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn edit_fresh_file_no_stale_warning() {
        let dir = setup();
        let file = dir.path().join("hello.txt");
        // Populate cache with the real current mtime.
        let current_mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
        let cache = make_cache(&file, current_mtime);
        let args = json!({
            "path": "hello.txt",
            "replacements": [{"old_str": "line two", "new_str": "LINE TWO"}]
        });
        assert!(
            validate_edit(&args, dir.path(), Some(&cache), None, None)
                .await
                .is_none(),
            "up-to-date file should not trigger stale warning"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn edit_no_cache_entry_no_stale_warning() {
        // File was never read via Read tool — empty cache, no stale warning.
        let dir = setup();
        let empty_cache = super::super::FileReadCache::default();
        let args = json!({
            "path": "hello.txt",
            "replacements": [{"old_str": "line two", "new_str": "LINE TWO"}]
        });
        assert!(
            validate_edit(&args, dir.path(), Some(&empty_cache), None, None)
                .await
                .is_none(),
            "no cache entry should not trigger stale warning"
        );
    }

    // ── Staleness hint messages (#804 item 7) ─────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stale_file_hints_last_writer_tool() {
        let dir = setup();
        let file = dir.path().join("hello.txt");
        let cache = make_cache(&file, SystemTime::UNIX_EPOCH);

        // Populate last_writer with an Edit entry for this file.
        let last_writer = super::super::LastWriterCache::default();
        last_writer.lock().unwrap().insert(
            file.clone(),
            ("Edit".to_string(), std::time::Instant::now()),
        );

        let args = json!({
            "path": "hello.txt",
            "replacements": [{"old_str": "line two", "new_str": "LINE TWO"}]
        });
        let err = validate_edit(&args, dir.path(), Some(&cache), Some(&last_writer), None)
            .await
            .unwrap();
        assert!(err.contains("modified on disk"), "{err}");
        assert!(err.contains("last written by Edit"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stale_file_hints_bash_when_no_writer_entry() {
        let dir = setup();
        let file = dir.path().join("hello.txt");
        let cache = make_cache(&file, SystemTime::UNIX_EPOCH);

        // No last_writer entry for this file — fall through to last_bash.
        let last_writer = super::super::LastWriterCache::default();
        let last_bash = super::super::LastBashCache::default();
        *last_bash.lock().unwrap() = Some((
            "cargo fmt -- src/bash_safety.rs".to_string(),
            std::time::Instant::now(),
        ));

        let args = json!({
            "path": "hello.txt",
            "replacements": [{"old_str": "line two", "new_str": "LINE TWO"}]
        });
        let err = validate_edit(
            &args,
            dir.path(),
            Some(&cache),
            Some(&last_writer),
            Some(&last_bash),
        )
        .await
        .unwrap();
        assert!(err.contains("modified on disk"), "{err}");
        assert!(err.contains("Bash ran"), "{err}");
        assert!(err.contains("cargo fmt"), "{err}");
    }

    // ── Omission placeholder detection ────────────────────────

    #[test]
    fn omission_detects_comment_style() {
        let cases = vec![
            "// rest of code ...",
            "// rest of methods ...",
            "# rest of implementation ...",
            "// unchanged code ...",
            "# existing code ...",
            "// remaining code ...",
        ];
        for input in cases {
            let found = detect_omission_placeholders(input);
            assert!(!found.is_empty(), "should detect: {input}");
        }
    }

    #[test]
    fn omission_detects_paren_style() {
        let found = detect_omission_placeholders("(rest of code ...)");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0], "rest of code ...");
    }

    #[test]
    fn omission_detects_comment_plus_parens() {
        let found = detect_omission_placeholders("// (existing implementation ...)");
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn omission_ignores_normal_code() {
        let cases = vec![
            "let x = 42;",
            "// TODO: fix this later",
            "# This is a normal comment",
            "fn rest_of_things() {}",
            "use std::rest::of::things;",
            "println!(\"...\");", // "..." not after a known prefix
            "// See the rest of the docs at ...", // not a known prefix
        ];
        for input in cases {
            let found = detect_omission_placeholders(input);
            assert!(found.is_empty(), "false positive on: {input}");
        }
    }

    #[test]
    fn omission_case_insensitive() {
        let found = detect_omission_placeholders("// Rest Of Code ...");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0], "rest of code ...");
    }

    #[test]
    fn omission_extra_dots_ok() {
        // "// rest of code ......" should still match
        let found = detect_omission_placeholders("// rest of code ......");
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn omission_suffix_text_rejects() {
        // "// rest of code ... here" has non-dot suffix — not a placeholder
        let found = detect_omission_placeholders("// rest of code ... here");
        assert!(found.is_empty());
    }

    #[test]
    fn omission_preserving_existing_placeholder_is_fine() {
        // old_str already has the placeholder — model is preserving, not creating.
        let old = "fn foo() {\n    // rest of code ...\n}";
        let new = "fn foo() {\n    do_thing();\n    // rest of code ...\n}";
        assert!(detect_new_omission_placeholder(old, new, 0).is_none());
    }

    #[test]
    fn omission_introducing_new_placeholder_rejected() {
        let old = "fn foo() {\n    real_code();\n    more_code();\n}";
        let new = "fn foo() {\n    real_code();\n    // rest of code ...\n}";
        let err = detect_new_omission_placeholder(old, new, 0).unwrap();
        assert!(err.contains("omission placeholder"), "{err}");
        assert!(err.contains("actual code"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn edit_rejects_omission_in_new_str() {
        let dir = setup();
        let args = json!({
            "path": "hello.txt",
            "replacements": [{
                "old_str": "line two",
                "new_str": "// rest of code ..."
            }]
        });
        let err = validate_edit(&args, dir.path(), None, None, None)
            .await
            .unwrap();
        assert!(err.contains("omission placeholder"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn edit_allows_normal_new_str() {
        let dir = setup();
        let args = json!({
            "path": "hello.txt",
            "replacements": [{
                "old_str": "line two",
                "new_str": "line TWO\n// This comment has dots: ..."
            }]
        });
        // "..." without a known prefix should NOT be detected
        assert!(
            validate_edit(&args, dir.path(), None, None, None)
                .await
                .is_none()
        );
    }

    // ── fmt_age boundary values (#819) ───────────────────────

    #[test]
    fn fmt_age_under_5s_is_just_now() {
        assert_eq!(fmt_age(std::time::Duration::from_secs(0)), "just now");
        assert_eq!(fmt_age(std::time::Duration::from_secs(4)), "just now");
    }

    #[test]
    fn fmt_age_exactly_5s() {
        // Boundary: 5s is the first value that produces "Xs ago".
        assert_eq!(fmt_age(std::time::Duration::from_secs(5)), "5s ago");
    }

    #[test]
    fn fmt_age_under_60s() {
        assert_eq!(fmt_age(std::time::Duration::from_secs(30)), "30s ago");
        assert_eq!(fmt_age(std::time::Duration::from_secs(59)), "59s ago");
    }

    #[test]
    fn fmt_age_exactly_60s() {
        // Boundary: 60s is the first value that produces "Xm ago".
        assert_eq!(fmt_age(std::time::Duration::from_secs(60)), "1m ago");
    }

    #[test]
    fn fmt_age_minutes() {
        assert_eq!(fmt_age(std::time::Duration::from_secs(90)), "1m ago");
        assert_eq!(fmt_age(std::time::Duration::from_secs(120)), "2m ago");
        assert_eq!(fmt_age(std::time::Duration::from_secs(3600)), "60m ago");
    }
}
