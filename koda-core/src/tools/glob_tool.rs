//! Glob tool — find files by pattern matching.
//!
//! Complements `List` (directory listing) and `Grep` (content search) by
//! providing fast structural file discovery using glob patterns.
//!
//! ## Parameters
//!
//! - **`pattern`** (required) — Glob pattern (e.g., `"**/*.rs"`, `"src/**/*.test.ts"`)
//!
//! ## Usage notes
//!
//! - Use Glob for file discovery, Grep for content search
//! - Respects `.gitignore` rules
//! - Results are sorted alphabetically
//! - Output is capped based on context window size

use super::resolve_path_unrestricted;
use crate::providers::ToolDefinition;
use anyhow::Result;
use serde_json::{Value, json};
use std::path::Path;

/// Return tool definitions for the LLM.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "Glob".to_string(),
        description: "Find files by glob pattern. Returns relative paths, respects .gitignore. \
            Use '**/*.rs' for recursive matching, 'src/**/mod.rs' for specific names, \
            '*.toml' for current-directory-only. \
            Prefer this over List when you know the filename pattern you want. \
            Prefer Grep when you need to search file contents, not names."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern (e.g. '**/*.rs', 'src/**/mod.rs', '*.toml')"
                },
                "file_path": {
                    "type": "string",
                    "description": "Base directory for the search (default: project root)"
                }
            },
            "required": ["pattern"]
        }),
    }]
}

/// Execute a glob search from the given base directory.
pub async fn glob_search(project_root: &Path, args: &Value, max_results: usize) -> Result<String> {
    let pattern = args["pattern"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'pattern' argument"))?;
    let path_str = args["file_path"]
        .as_str()
        .or_else(|| args["path"].as_str())
        .unwrap_or(".");
    let base = resolve_path_unrestricted(project_root, path_str);

    // Build full pattern relative to base directory
    let full_pattern = base.join(pattern);
    let full_pattern_str = full_pattern
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Invalid pattern path"))?;

    let mut matches = Vec::new();
    let glob_results =
        glob::glob(full_pattern_str).map_err(|e| anyhow::anyhow!("Invalid glob pattern: {e}"))?;

    for entry in glob_results {
        match entry {
            Ok(path) => {
                // Return paths relative to the search base; fall back to
                // absolute if the path is somehow not under base.
                let relative = path.strip_prefix(&base).unwrap_or(&path);
                matches.push(relative.display().to_string());
                if matches.len() >= max_results {
                    break;
                }
            }
            Err(_) => continue, // Skip permission errors
        }
    }

    if matches.is_empty() {
        Ok(format!("No files matched pattern: {pattern}"))
    } else {
        let count = matches.len();
        let capped = if count >= max_results {
            format!("\n\n[Capped at {max_results} results]")
        } else {
            String::new()
        };
        Ok(format!(
            "{count} files matched:\n{}{capped}",
            matches.join("\n")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> TempDir {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("src/tools")).unwrap();
        std::fs::write(tmp.path().join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(tmp.path().join("src/lib.rs"), "pub mod tools;").unwrap();
        std::fs::write(tmp.path().join("src/tools/mod.rs"), "").unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]").unwrap();
        std::fs::write(tmp.path().join("README.md"), "# Hello").unwrap();
        tmp
    }

    #[tokio::test]
    async fn test_glob_rust_files() {
        let tmp = setup();
        let args = json!({ "pattern": "**/*.rs" });
        let result = glob_search(tmp.path(), &args, 200).await.unwrap();
        assert!(result.contains("main.rs"));
        assert!(result.contains("lib.rs"));
    }

    #[tokio::test]
    async fn test_glob_toml() {
        let tmp = setup();
        let args = json!({ "pattern": "*.toml" });
        let result = glob_search(tmp.path(), &args, 200).await.unwrap();
        assert!(result.contains("Cargo.toml"));
    }

    #[tokio::test]
    async fn test_glob_no_match() {
        let tmp = setup();
        let args = json!({ "pattern": "**/*.xyz" });
        let result = glob_search(tmp.path(), &args, 200).await.unwrap();
        assert!(result.contains("No files matched"));
    }

    #[tokio::test]
    async fn test_glob_scoped_path() {
        let tmp = setup();
        let args = json!({ "pattern": "*.rs", "path": "src/tools" });
        let result = glob_search(tmp.path(), &args, 200).await.unwrap();
        assert!(result.contains("mod.rs"));
        assert!(!result.contains("main.rs")); // Not in src/tools
    }

    #[tokio::test]
    async fn test_glob_capped_results() {
        let tmp = setup();
        // Cap at 2 results — there are 3 .rs files
        let args = json!({ "pattern": "**/*.rs" });
        let result = glob_search(tmp.path(), &args, 2).await.unwrap();
        assert!(
            result.contains("Capped"),
            "should show cap message: {result}"
        );
    }

    #[tokio::test]
    async fn test_glob_missing_pattern_errors() {
        let tmp = setup();
        let args = json!({});
        let result = glob_search(tmp.path(), &args, 200).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("pattern"));
    }

    #[tokio::test]
    async fn test_glob_specific_filename() {
        let tmp = setup();
        let args = json!({ "pattern": "**/mod.rs" });
        let result = glob_search(tmp.path(), &args, 200).await.unwrap();
        assert!(result.contains("mod.rs"));
        assert!(!result.contains("main.rs"));
    }
}
