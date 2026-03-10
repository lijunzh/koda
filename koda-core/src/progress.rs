//! Structured progress tracking.
//!
//! Auto-extracts progress from tool results into DB metadata.
//! Survives compaction. Injected into system prompt so the LLM
//! always knows what's been done even after context is trimmed.

use crate::db::Database;
use crate::persistence::Persistence;

/// Extract progress from a tool call and persist it.
pub async fn track_progress(
    db: &Database,
    session_id: &str,
    tool_name: &str,
    _tool_args: &str,
    tool_result: &str,
) {
    let entry = match tool_name {
        "Write" => extract_write_progress(tool_result),
        "Edit" => extract_edit_progress(tool_result),
        "Delete" => extract_delete_progress(tool_result),
        "Bash" => extract_bash_progress(tool_result),
        _ => None,
    };

    if let Some(entry) = entry {
        append_progress(db, session_id, &entry).await;
    }
}

/// Get the current progress summary for injection into the system prompt.
pub async fn get_progress_summary(db: &Database, session_id: &str) -> Option<String> {
    match db.get_metadata(session_id, "progress").await {
        Ok(Some(progress)) if !progress.is_empty() => Some(format!(
            "\n## Session Progress\n\
                 The following actions have been completed this session:\n\
                 {progress}"
        )),
        _ => None,
    }
}

async fn append_progress(db: &Database, session_id: &str, entry: &str) {
    let existing = db
        .get_metadata(session_id, "progress")
        .await
        .ok()
        .flatten()
        .unwrap_or_default();

    // Cap at 20 entries to avoid unbounded growth
    let lines: Vec<&str> = existing.lines().collect();
    let mut updated = if lines.len() >= 20 {
        // Keep last 15 + new entry
        lines[lines.len() - 15..].join("\n")
    } else {
        existing
    };

    if !updated.is_empty() {
        updated.push('\n');
    }
    updated.push_str(entry);

    let _ = db.set_metadata(session_id, "progress", &updated).await;
}

fn extract_write_progress(result: &str) -> Option<String> {
    // Write tool output: "Created file: path" or "Wrote N bytes to path"
    if result.contains("Created") || result.contains("Wrote") {
        let path = result.lines().next().unwrap_or(result).trim();
        Some(format!("- \u{2705} {path}"))
    } else {
        None
    }
}

fn extract_edit_progress(result: &str) -> Option<String> {
    if result.contains("Applied") || result.contains("edited") || result.contains("replacement") {
        let first_line = result.lines().next().unwrap_or(result).trim();
        let short = if first_line.len() > 80 {
            format!("{}...", &first_line[..80])
        } else {
            first_line.to_string()
        };
        Some(format!("- \u{270f}\u{fe0f} {short}"))
    } else {
        None
    }
}

fn extract_delete_progress(result: &str) -> Option<String> {
    if result.contains("Deleted") || result.contains("removed") {
        let first_line = result.lines().next().unwrap_or(result).trim();
        Some(format!("- \u{1f5d1}\u{fe0f} {first_line}"))
    } else {
        None
    }
}

fn extract_bash_progress(result: &str) -> Option<String> {
    // Track test results and build outcomes
    let lower = result.to_lowercase();
    if lower.contains("test result: ok") || lower.contains("tests passed") {
        Some("- \u{2705} Tests passed".to_string())
    } else if lower.contains("test result: failed") || lower.contains("tests failed") {
        Some("- \u{274c} Tests failed".to_string())
    } else if lower.contains("build succeeded")
        || lower.contains("finished") && lower.contains("target")
    {
        Some("- \u{1f3d7}\u{fe0f} Build succeeded".to_string())
    } else if lower.contains("error:") && lower.contains("could not compile") {
        Some("- \u{274c} Build failed".to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_write_progress() {
        assert!(extract_write_progress("Created file: src/main.rs").is_some());
        assert!(extract_write_progress("Wrote 100 bytes to foo.rs").is_some());
        assert!(extract_write_progress("Error: permission denied").is_none());
    }

    #[test]
    fn test_edit_progress() {
        assert!(extract_edit_progress("Applied 2 replacements to src/lib.rs").is_some());
        assert!(extract_edit_progress("No changes needed").is_none());
    }

    #[test]
    fn test_bash_progress() {
        assert!(extract_bash_progress("test result: ok. 50 passed").is_some());
        assert!(extract_bash_progress("test result: FAILED. 1 failed").is_some());
        assert!(extract_bash_progress("hello world").is_none());
    }

    #[test]
    fn test_delete_progress() {
        assert!(extract_delete_progress("Deleted src/old.rs").is_some());
        assert!(extract_delete_progress("File not found").is_none());
    }
}
