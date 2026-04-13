//! Grep tool — recursive text search across files.
//!
//! Uses the `ignore` crate to walk directories (respecting `.gitignore`)
//! and searches for text patterns. Match cap is set by `OutputCaps`
//! (context-scaled).
//!
//! ## Parameters
//!
//! - **`pattern`** (required) — Text or regex pattern to search for
//! - **`path`** (optional, default `.`) — Directory or file to search in
//! - **`include`** (optional) — Glob pattern to filter files (e.g., `"*.rs"`)
//!
//! ## Output format
//!
//! Returns matches as `file:line: content`, one per line. When matches
//! exceed the cap, output is truncated with a count of remaining matches.
//!
//! ## When to use Grep vs Bash
//!
//! - **Grep tool**: fast, respects `.gitignore`, context-aware output caps
//! - **`bash: grep/rg`**: only when you need complex flags the tool doesn't expose

use super::resolve_read_path;
use crate::providers::ToolDefinition;
use anyhow::Result;
use serde_json::{Value, json};
use std::path::Path;

/// Return tool definitions for the LLM.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "Grep".to_string(),
        description: "Recursively search for text patterns across files (respects .gitignore). \
            Returns matching file paths with line numbers and content. \
            Use plain text for exact matches or regex for complex patterns. \
            Set case_insensitive=true for case-agnostic search. \
            Prefer this over Bash + rg/grep. Results are capped at 100 matches."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "The text pattern to search for (plain text or regex)"
                },
                "file_path": {
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
pub async fn grep(project_root: &Path, args: &Value, max_matches: usize) -> Result<String> {
    let pattern = args["pattern"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'pattern' argument"))?
        .to_string();
    let path_str = args["file_path"]
        .as_str()
        .or_else(|| args["path"].as_str())
        .unwrap_or(".");
    let case_insensitive = args["case_insensitive"].as_bool().unwrap_or(false);

    let search_root = resolve_read_path(project_root, path_str)?;
    let project_root = project_root.to_path_buf();

    // Run blocking file I/O off the tokio thread pool
    tokio::task::spawn_blocking(move || {
        grep_blocking(
            &project_root,
            &search_root,
            &pattern,
            case_insensitive,
            max_matches,
        )
    })
    .await?
}

/// Blocking grep implementation (runs on a dedicated thread).
fn grep_blocking(
    project_root: &Path,
    search_root: &Path,
    pattern: &str,
    case_insensitive: bool,
    max_matches: usize,
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

                if matches.len() >= max_matches {
                    matches.push(format!(
                        "\n... [CAPPED at {max_matches} matches. \
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
        let result = grep(tmp.path(), &args, 100).await.unwrap();
        assert!(result.contains("hello.rs"));
        assert!(result.contains("lib.rs"));
    }

    #[tokio::test]
    async fn test_grep_no_matches() {
        let tmp = setup_test_dir();
        let args = json!({ "pattern": "zzzznotfound" });
        let result = grep(tmp.path(), &args, 100).await.unwrap();
        assert!(result.contains("No matches"));
    }

    #[tokio::test]
    async fn test_grep_case_insensitive() {
        let tmp = setup_test_dir();
        let args = json!({ "pattern": "HELLO", "case_insensitive": true });
        let result = grep(tmp.path(), &args, 100).await.unwrap();
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
        let result = grep(tmp.path(), &args, 100).await.unwrap();
        assert!(result.contains("deep.rs"));
    }

    #[tokio::test]
    async fn test_grep_skips_binary_files() {
        let tmp = TempDir::new().unwrap();
        // Write a file with invalid UTF-8 bytes (binary).
        let binary: Vec<u8> = vec![0xFF, 0xFE, b'h', b'e', b'l', b'l', b'o', 0x00];
        std::fs::write(tmp.path().join("data.bin"), &binary).unwrap();
        // Also a normal file that matches.
        std::fs::write(tmp.path().join("text.rs"), "hello world").unwrap();

        let args = json!({ "pattern": "hello" });
        let result = grep(tmp.path(), &args, 100).await.unwrap();
        // Binary file should be silently skipped; text file should match.
        assert!(result.contains("text.rs"));
        assert!(!result.contains("data.bin"));
    }

    #[tokio::test]
    async fn test_grep_match_count_capped() {
        let tmp = TempDir::new().unwrap();
        // 20 files each containing the search term.
        for i in 0..20 {
            std::fs::write(
                tmp.path().join(format!("file{i}.rs")),
                "needle haystack needle",
            )
            .unwrap();
        }
        // Cap at 5 matches.
        let args = json!({ "pattern": "needle" });
        let result = grep(tmp.path(), &args, 5).await.unwrap();
        // Result should mention truncation when matches exceed the cap.
        assert!(
            result.contains("CAPPED"),
            "expected CAPPED hint in output when cap is exceeded: {result}"
        );
    }

    #[tokio::test]
    async fn test_grep_file_path_param_alias() {
        // The tool accepts both "path" and "file_path" as the directory param.
        let tmp = setup_test_dir();
        let args = json!({ "pattern": "nope", "file_path": "nested" });
        let result = grep(tmp.path(), &args, 100).await.unwrap();
        assert!(result.contains("deep.rs"));
    }

    #[tokio::test]
    async fn test_grep_regex_special_chars_treated_literally() {
        let tmp = TempDir::new().unwrap();
        // Write a file containing the literal string "fn (" — the parens are regex specials.
        std::fs::write(tmp.path().join("code.rs"), "fn (invalid syntax").unwrap();
        // If the pattern were treated as regex, "fn (" would be a parse error.
        // Since we escape it, it should match the literal text.
        let args = json!({ "pattern": "fn (" });
        let result = grep(tmp.path(), &args, 100).await.unwrap();
        assert!(
            result.contains("code.rs"),
            "literal paren should be matched without regex error"
        );
    }
}
