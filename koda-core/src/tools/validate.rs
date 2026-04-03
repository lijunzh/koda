//! Pre-flight validation for tool calls.
//!
//! Runs **before** the approval prompt so we never ask the user to approve
//! an operation that will inevitably fail. Cheap checks only — no mutations.

use super::safe_resolve_path;
use std::path::Path;
use std::time::SystemTime;

/// Validate a tool call before approval.
///
/// `read_cache` is the session file-read cache.  When provided, `validate_edit`
/// uses it to detect files that have been modified on disk since the model last
/// read them — catching the most common source of lost-context edits.
///
/// Returns `None` if the call looks valid, or `Some(error_message)` describing
/// why it will fail. The error message is fed back to the model so it can
/// self-correct without consuming an approval prompt.
pub async fn validate_tool_call(
    tool_name: &str,
    args: &serde_json::Value,
    project_root: &Path,
    read_cache: Option<&super::FileReadCache>,
) -> Option<String> {
    match tool_name {
        "Edit" => validate_edit(args, project_root, read_cache).await,
        "Write" => validate_write(args, project_root).await,
        "Delete" => validate_delete(args, project_root).await,
        "Bash" => validate_bash(args),
        _ => None,
    }
}

/// Edit: file must exist, replacements must be non-empty, each old_str must
/// be non-empty and actually present in the file.  Also warns if the file has
/// been modified on disk since the model last read it.
async fn validate_edit(
    args: &serde_json::Value,
    project_root: &Path,
    read_cache: Option<&super::FileReadCache>,
) -> Option<String> {
    let path_str = args["path"].as_str().unwrap_or("");
    if path_str.is_empty() {
        return Some("Missing 'path' argument.".into());
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
            .map(|&(_, mtime)| mtime);
        if let Some(cm) = cached_mtime
            && cm != current_mtime
        {
            return Some(format!(
                "File '{}' has been modified on disk since you last read it. \
                 Read it again to get the current content before editing.",
                path_str
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

        if replacement["new_str"].as_str().is_none() {
            return Some(format!("Replacement {i}: missing 'new_str'."));
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
    let path_str = args["path"].as_str().unwrap_or("");
    if path_str.is_empty() {
        return Some("Missing 'path' argument.".into());
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
    let path_str = args["path"].as_str().unwrap_or("");
    if path_str.is_empty() {
        return Some("Missing 'path' argument.".into());
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

    #[tokio::test]
    async fn edit_valid_replacement() {
        let dir = setup();
        let args = json!({
            "path": "hello.txt",
            "replacements": [{"old_str": "line two", "new_str": "line TWO"}]
        });
        assert!(validate_edit(&args, dir.path(), None).await.is_none());
    }

    #[tokio::test]
    async fn edit_missing_path() {
        let dir = setup();
        let args = json!({"replacements": [{"old_str": "x", "new_str": "y"}]});
        let err = validate_edit(&args, dir.path(), None).await.unwrap();
        assert!(err.contains("path"), "{err}");
    }

    #[tokio::test]
    async fn edit_file_not_found() {
        let dir = setup();
        let args = json!({
            "path": "nope.txt",
            "replacements": [{"old_str": "x", "new_str": "y"}]
        });
        let err = validate_edit(&args, dir.path(), None).await.unwrap();
        assert!(err.contains("Cannot read"), "{err}");
        assert!(err.contains("Write"), "{err}"); // suggests Write
    }

    #[tokio::test]
    async fn edit_empty_replacements() {
        let dir = setup();
        let args = json!({"path": "hello.txt", "replacements": []});
        let err = validate_edit(&args, dir.path(), None).await.unwrap();
        assert!(err.contains("empty"), "{err}");
    }

    #[tokio::test]
    async fn edit_empty_old_str() {
        let dir = setup();
        let args = json!({
            "path": "hello.txt",
            "replacements": [{"old_str": "", "new_str": "y"}]
        });
        let err = validate_edit(&args, dir.path(), None).await.unwrap();
        assert!(err.contains("empty"), "{err}");
    }

    #[tokio::test]
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
            validate_edit(&args, dir.path(), None).await.is_none(),
            "fuzzy match should pass validation"
        );
    }

    #[tokio::test]
    async fn edit_old_str_not_found() {
        let dir = setup();
        let args = json!({
            "path": "hello.txt",
            "replacements": [{"old_str": "does not exist", "new_str": "y"}]
        });
        let err = validate_edit(&args, dir.path(), None).await.unwrap();
        assert!(err.contains("not found"), "{err}");
    }

    #[tokio::test]
    async fn edit_missing_new_str() {
        let dir = setup();
        let args = json!({
            "path": "hello.txt",
            "replacements": [{"old_str": "line one"}]
        });
        let err = validate_edit(&args, dir.path(), None).await.unwrap();
        assert!(err.contains("new_str"), "{err}");
    }

    // ── Write validation ──────────────────────────────────────

    #[tokio::test]
    async fn write_new_file_valid() {
        let dir = setup();
        let args = json!({"path": "brand_new.txt", "content": "hello"});
        assert!(validate_write(&args, dir.path()).await.is_none());
    }

    #[tokio::test]
    async fn write_existing_without_overwrite() {
        let dir = setup();
        let args = json!({"path": "hello.txt", "content": "replaced"});
        let err = validate_write(&args, dir.path()).await.unwrap();
        assert!(err.contains("already exists"), "{err}");
        assert!(err.contains("overwrite=true"), "{err}");
    }

    #[tokio::test]
    async fn write_existing_with_overwrite() {
        let dir = setup();
        let args = json!({"path": "hello.txt", "content": "replaced", "overwrite": true});
        assert!(validate_write(&args, dir.path()).await.is_none());
    }

    #[tokio::test]
    async fn write_missing_content() {
        let dir = setup();
        let args = json!({"path": "foo.txt"});
        let err = validate_write(&args, dir.path()).await.unwrap();
        assert!(err.contains("content"), "{err}");
    }

    // ── Delete validation ─────────────────────────────────────

    #[tokio::test]
    async fn delete_valid_file() {
        let dir = setup();
        let args = json!({"path": "hello.txt"});
        assert!(validate_delete(&args, dir.path()).await.is_none());
    }

    #[tokio::test]
    async fn delete_not_found() {
        let dir = setup();
        let args = json!({"path": "nope.txt"});
        let err = validate_delete(&args, dir.path()).await.unwrap();
        assert!(err.contains("not found"), "{err}");
    }

    #[tokio::test]
    async fn delete_nonempty_dir_without_recursive() {
        let dir = setup();
        let args = json!({"path": "subdir"});
        let err = validate_delete(&args, dir.path()).await.unwrap();
        assert!(err.contains("recursive"), "{err}");
    }

    #[tokio::test]
    async fn delete_nonempty_dir_with_recursive() {
        let dir = setup();
        let args = json!({"path": "subdir", "recursive": true});
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
        cache.lock().unwrap().insert(key, (0, mtime));
        cache
    }

    #[tokio::test]
    async fn edit_stale_file_detected() {
        let dir = setup();
        let file = dir.path().join("hello.txt");
        // Populate cache with a deliberately old mtime (epoch = definitely stale).
        let cache = make_cache(&file, SystemTime::UNIX_EPOCH);
        let args = json!({
            "path": "hello.txt",
            "replacements": [{"old_str": "line two", "new_str": "LINE TWO"}]
        });
        let err = validate_edit(&args, dir.path(), Some(&cache))
            .await
            .unwrap();
        assert!(err.contains("modified on disk"), "{err}");
        assert!(err.contains("Read it again"), "{err}");
    }

    #[tokio::test]
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
            validate_edit(&args, dir.path(), Some(&cache))
                .await
                .is_none(),
            "up-to-date file should not trigger stale warning"
        );
    }

    #[tokio::test]
    async fn edit_no_cache_entry_no_stale_warning() {
        // File was never read via Read tool — empty cache, no stale warning.
        let dir = setup();
        let empty_cache = super::super::FileReadCache::default();
        let args = json!({
            "path": "hello.txt",
            "replacements": [{"old_str": "line two", "new_str": "LINE TWO"}]
        });
        assert!(
            validate_edit(&args, dir.path(), Some(&empty_cache))
                .await
                .is_none(),
            "no cache entry should not trigger stale warning"
        );
    }
}
