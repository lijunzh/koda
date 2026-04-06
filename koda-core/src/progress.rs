//! Structured progress tracking.
//!
//! Auto-extracts progress from tool results into DB metadata.
//! Survives compaction. Injected into system prompt so the LLM
//! always knows what's been done even after context is trimmed.
//!
//! ## Why progress tracking exists
//!
//! When compaction summarizes old messages, the model loses awareness of
//! what files were created/modified and what steps were completed. Progress
//! entries are stored in the DB (not in messages) and re-injected into
//! the system prompt, providing a persistent "done" list.
//!
//! ## What gets tracked
//!
//! - Files created (Write tool)
//! - Files modified (Edit tool)
//! - Tests run and their results (Bash tool with test patterns)
//! - Commands executed with exit codes

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
    // Background process start
    if result.contains("Background process started") {
        // Extract the command from the result: "  Command: <cmd>"
        let cmd = result
            .lines()
            .find(|l| l.trim_start().starts_with("Command:"))
            .and_then(|l| l.split_once(':').map(|(_, v)| v))
            .map(|s| s.trim())
            .unwrap_or("?");
        let short = if cmd.len() > 60 {
            format!("{}...", &cmd[..60])
        } else {
            cmd.to_string()
        };
        return Some(format!("- \u{1f4e1} Started background: {short}"));
    }
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

    // ── content / format assertions ───────────────────────────────────────

    #[test]
    fn test_write_progress_content_includes_first_line() {
        let result = extract_write_progress("Created file: src/main.rs").unwrap();
        assert!(
            result.contains("Created file: src/main.rs"),
            "got: {result}"
        );
    }

    #[test]
    fn test_delete_progress_removed_keyword() {
        // "removed" is an alternative trigger word
        assert!(extract_delete_progress("5 files removed successfully").is_some());
    }

    #[test]
    fn test_delete_progress_content_includes_first_line() {
        let result = extract_delete_progress("Deleted src/old.rs").unwrap();
        assert!(result.contains("Deleted src/old.rs"), "got: {result}");
    }

    #[test]
    fn test_edit_progress_long_line_is_truncated() {
        let long = "Applied replacements to ".to_string() + &"x".repeat(100);
        let result = extract_edit_progress(&long).unwrap();
        // Should be capped at 80 chars + "..."
        assert!(
            result.contains("..."),
            "long line should be truncated: {result}"
        );
    }

    #[test]
    fn test_edit_progress_short_line_not_truncated() {
        let result = extract_edit_progress("Applied 3 replacements to src/lib.rs").unwrap();
        assert!(
            !result.contains("..."),
            "short line should not be truncated: {result}"
        );
    }

    // ── extract_bash_progress edge cases ─────────────────────────────────

    #[test]
    fn test_bash_progress_background_process() {
        let result = extract_bash_progress(
            "Background process started\n  Command: cargo watch -x test\n  PID: 1234",
        )
        .unwrap();
        assert!(
            result.contains("cargo watch"),
            "should include command: {result}"
        );
        assert!(result.contains("Started background"), "got: {result}");
    }

    #[test]
    fn test_bash_progress_build_cargo_finish() {
        // cargo build output typically contains "Finished" and "target"
        let result =
            extract_bash_progress("   Finished `release` profile [optimized] target(s)").unwrap();
        assert!(result.contains("Build succeeded"), "got: {result}");
    }

    #[test]
    fn test_bash_progress_build_succeeded_literal() {
        let result = extract_bash_progress("build succeeded").unwrap();
        assert!(result.contains("Build succeeded"), "got: {result}");
    }

    #[test]
    fn test_bash_progress_build_failed() {
        let result =
            extract_bash_progress("error: could not compile `myapp` due to 3 errors").unwrap();
        assert!(result.contains("Build failed"), "got: {result}");
    }

    #[test]
    fn test_bash_progress_tests_passed_text() {
        // Some test runners emit "tests passed" not "test result: ok"
        let result = extract_bash_progress("All 42 tests passed").unwrap();
        assert!(result.contains("Tests passed"), "got: {result}");
    }

    #[test]
    fn test_bash_progress_tests_failed_text() {
        let result = extract_bash_progress("3 tests failed").unwrap();
        assert!(result.contains("Tests failed"), "got: {result}");
    }

    #[test]
    fn test_bash_progress_background_long_command_truncated() {
        let long_cmd = "x".repeat(80);
        let output = format!("Background process started\n  Command: {long_cmd}\n");
        let result = extract_bash_progress(&output).unwrap();
        assert!(
            result.contains("..."),
            "long command should be truncated: {result}"
        );
    }

    // ── async / DB-backed tests ──────────────────────────────────────────

    async fn test_db() -> (Database, tempfile::TempDir, String) {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("progress_test.db"))
            .await
            .unwrap();
        let sid = db.create_session("koda", dir.path()).await.unwrap();
        (db, dir, sid)
    }

    #[tokio::test]
    async fn test_get_progress_summary_empty() {
        let (db, _dir, sid) = test_db().await;
        let result = get_progress_summary(&db, &sid).await;
        assert!(result.is_none(), "empty session should return None");
    }

    #[tokio::test]
    async fn test_track_progress_write_tool() {
        let (db, _dir, sid) = test_db().await;
        track_progress(&db, &sid, "Write", "", "Created file: src/main.rs").await;
        let summary = get_progress_summary(&db, &sid).await.unwrap();
        assert!(summary.contains("src/main.rs"), "got: {summary}");
        assert!(summary.contains("Session Progress"), "got: {summary}");
    }

    #[tokio::test]
    async fn test_track_progress_edit_tool() {
        let (db, _dir, sid) = test_db().await;
        track_progress(
            &db,
            &sid,
            "Edit",
            "",
            "Applied 2 replacements to src/lib.rs",
        )
        .await;
        let summary = get_progress_summary(&db, &sid).await.unwrap();
        assert!(summary.contains("lib.rs"), "got: {summary}");
    }

    #[tokio::test]
    async fn test_track_progress_bash_tool() {
        let (db, _dir, sid) = test_db().await;
        track_progress(
            &db,
            &sid,
            "Bash",
            "",
            "test result: ok. 10 passed; 0 failed",
        )
        .await;
        let summary = get_progress_summary(&db, &sid).await.unwrap();
        assert!(summary.contains("Tests passed"), "got: {summary}");
    }

    #[tokio::test]
    async fn test_track_progress_delete_tool() {
        let (db, _dir, sid) = test_db().await;
        track_progress(&db, &sid, "Delete", "", "Deleted src/old.rs").await;
        let summary = get_progress_summary(&db, &sid).await.unwrap();
        assert!(summary.contains("Deleted"), "got: {summary}");
    }

    #[tokio::test]
    async fn test_track_progress_unknown_tool_no_entry() {
        let (db, _dir, sid) = test_db().await;
        track_progress(&db, &sid, "UnknownTool", "", "some result").await;
        // Unknown tool → no entry tracked
        let summary = get_progress_summary(&db, &sid).await;
        assert!(summary.is_none(), "unknown tool should not create progress");
    }

    #[tokio::test]
    async fn test_progress_accumulates_multiple_entries() {
        let (db, _dir, sid) = test_db().await;
        track_progress(&db, &sid, "Write", "", "Created file: a.rs").await;
        track_progress(&db, &sid, "Write", "", "Created file: b.rs").await;
        let summary = get_progress_summary(&db, &sid).await.unwrap();
        assert!(summary.contains("a.rs"), "got: {summary}");
        assert!(summary.contains("b.rs"), "got: {summary}");
    }
}
