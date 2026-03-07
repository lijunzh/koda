//! Grep tool: recursive text search across files.
//!
//! Uses the `ignore` crate to walk directories (respecting .gitignore)
//! and searches for text patterns. Results are capped at 100 matches
//! to protect the context window.

use super::safe_resolve_path;
use crate::providers::ToolDefinition;
use anyhow::Result;
use serde_json::{Value, json};
use std::path::Path;

const MAX_MATCHES: usize = 100;

/// Return tool definitions for the LLM.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "Grep".to_string(),
        description: "Recursively search for text across files (respects .gitignore). \
            Capped at 100 matches."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "The text pattern to search for (plain text or regex)"
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search in (default: project root)"
                },
                "case_insensitive": {
                    "type": "boolean",
                    "description": "Whether to ignore case (default: false)"
                }
            },
            "required": ["pattern"]
        }),
    }]
}

/// Search for a text pattern across files in a directory.
pub async fn grep(project_root: &Path, args: &Value) -> Result<String> {
    let pattern = args["pattern"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'pattern' argument"))?
        .to_string();
    let path_str = args["path"].as_str().unwrap_or(".");
    let case_insensitive = args["case_insensitive"].as_bool().unwrap_or(false);

    let search_root = safe_resolve_path(project_root, path_str)?;
    let project_root = project_root.to_path_buf();

    // Run blocking file I/O off the tokio thread pool
    tokio::task::spawn_blocking(move || {
        grep_blocking(&project_root, &search_root, &pattern, case_insensitive)
    })
    .await?
}

/// Blocking grep implementation (runs on a dedicated thread).
fn grep_blocking(
    project_root: &Path,
    search_root: &Path,
    pattern: &str,
    case_insensitive: bool,
) -> Result<String> {
    let regex = if case_insensitive {
        regex::RegexBuilder::new(&regex::escape(pattern))
            .case_insensitive(true)
            .build()?
    } else {
        regex::Regex::new(&regex::escape(pattern))?
    };

    let walker = ignore::WalkBuilder::new(search_root)
        .hidden(true)
        .git_ignore(true)
        .build();

    let mut matches = Vec::new();
    let mut files_searched = 0u64;

    for entry in walker.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        // Skip binary files: read only the first 8KB to check
        let content = match std::fs::read(path) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(s) => s,
                Err(_) => continue,
            },
            Err(_) => continue,
        };

        files_searched += 1;

        let relative = path.strip_prefix(project_root).unwrap_or(path);
        for (line_num, line) in content.lines().enumerate() {
            if regex.is_match(line) {
                matches.push(format!(
                    "{}:{}:{}",
                    relative.display(),
                    line_num + 1,
                    truncate_line(line, 200)
                ));

                if matches.len() >= MAX_MATCHES {
                    matches.push(format!(
                        "\n... [CAPPED at {MAX_MATCHES} matches. \
                         Narrow your search pattern.]"
                    ));
                    return Ok(format_output(&matches, files_searched));
                }
            }
        }
    }

    if matches.is_empty() {
        Ok(format!(
            "No matches found for \'{pattern}\' (searched {files_searched} files)"
        ))
    } else {
        Ok(format_output(&matches, files_searched))
    }
}

fn format_output(matches: &[String], files_searched: u64) -> String {
    format!(
        "{} matches (searched {} files):\n{}",
        matches.len(),
        files_searched,
        matches.join("\n")
    )
}

/// Truncate a line to at most `max_bytes` bytes, snapping down to
/// the nearest valid UTF-8 char boundary (never panics, never overshoots).
fn truncate_line(line: &str, max_bytes: usize) -> &str {
    if line.len() <= max_bytes {
        line
    } else if max_bytes == 0 {
        ""
    } else {
        // Walk backwards from max_bytes to find a valid char boundary.
        // UTF-8 continuation bytes are 10xxxxxx (0x80..=0xBF), so we
        // skip at most 3 of them to land on a leading byte.
        let mut end = max_bytes;
        while end > 0 && !line.is_char_boundary(end) {
            end -= 1;
        }
        &line[..end]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_test_dir() -> TempDir {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("hello.rs"),
            "fn main() {\n    println!(\"hello\");\n}\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("lib.rs"),
            "pub fn greet() {\n    println!(\"hello world\");\n}\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("nested")).unwrap();
        std::fs::write(
            tmp.path().join("nested/deep.rs"),
            "// no match here\nfn nope() {}\n",
        )
        .unwrap();
        tmp
    }

    #[tokio::test]
    async fn test_grep_finds_matches() {
        let tmp = setup_test_dir();
        let args = json!({ "pattern": "hello" });
        let result = grep(tmp.path(), &args).await.unwrap();
        assert!(result.contains("hello.rs"));
        assert!(result.contains("lib.rs"));
    }

    #[tokio::test]
    async fn test_grep_no_matches() {
        let tmp = setup_test_dir();
        let args = json!({ "pattern": "zzzznotfound" });
        let result = grep(tmp.path(), &args).await.unwrap();
        assert!(result.contains("No matches"));
    }

    #[tokio::test]
    async fn test_grep_case_insensitive() {
        let tmp = setup_test_dir();
        let args = json!({ "pattern": "HELLO", "case_insensitive": true });
        let result = grep(tmp.path(), &args).await.unwrap();
        assert!(result.contains("hello.rs"));
    }

    #[test]
    fn test_truncate_ascii() {
        assert_eq!(truncate_line("hello world", 5), "hello");
        assert_eq!(truncate_line("hi", 10), "hi");
        assert_eq!(truncate_line("", 5), "");
    }

    #[test]
    fn test_truncate_multibyte_boundary() {
        // 'ä' is 2 bytes (0xC3 0xA4), '🦀' is 4 bytes
        let line = "aää🦀b"; // a(1) + ä(2) + ä(2) + 🦀(4) + b(1) = 10 bytes
        assert_eq!(truncate_line(line, 10), line); // exact fit
        assert_eq!(truncate_line(line, 9), "aää🦀"); // drops 'b', 🦀 ends at byte 9
        assert_eq!(truncate_line(line, 8), "aää"); // mid 🦀, snap back to byte 5
        assert_eq!(truncate_line(line, 6), "aää"); // mid 🦀, snap back to byte 5
        assert_eq!(truncate_line(line, 5), "aää"); // exactly at 🦀 start = char boundary
        assert_eq!(truncate_line(line, 4), "aä"); // mid second ä, snap to byte 3
        assert_eq!(truncate_line(line, 3), "aä"); // exactly after first ä
        assert_eq!(truncate_line(line, 2), "a"); // mid first ä, snap to byte 1
        assert_eq!(truncate_line(line, 1), "a");
        assert_eq!(truncate_line(line, 0), "");
    }

    #[test]
    fn test_truncate_never_overshoots() {
        let line = "hello 🌍 world"; // 🌍 starts at byte 6, is 4 bytes
        let truncated = truncate_line(line, 7);
        assert!(truncated.len() <= 7, "got {} bytes", truncated.len());
        assert_eq!(truncated, "hello "); // can't fit 🌍, snaps to byte 6
    }

    #[tokio::test]
    async fn test_grep_scoped_to_subdirectory() {
        let tmp = setup_test_dir();
        let args = json!({ "pattern": "nope", "path": "nested" });
        let result = grep(tmp.path(), &args).await.unwrap();
        assert!(result.contains("deep.rs"));
    }
}
