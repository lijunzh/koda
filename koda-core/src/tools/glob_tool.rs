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

use super::resolve_read_path;
use crate::providers::ToolDefinition;
use anyhow::Result;
use koda_sandbox::fs::FileSystem;
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
pub async fn glob_search(
    project_root: &Path,
    args: &Value,
    max_results: usize,
    fs: &dyn FileSystem,
) -> Result<String> {
    let pattern = args["pattern"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'pattern' argument"))?;
    let path_str = args["file_path"]
        .as_str()
        .or_else(|| args["path"].as_str())
        .unwrap_or(".");
    let base = resolve_read_path(project_root, path_str)?;

    // Delegate to the FileSystem abstraction — LocalFileSystem joins the
    // pattern against `base` and returns sorted absolute paths; SandboxedFileSystem
    // does the same through the worker (Phase 2d, #934).
    let all_matches = fs
        .glob(pattern, &base)
        .await
        .map_err(|e| anyhow::anyhow!("Glob error: {e}"))?;

    let matches: Vec<String> = all_matches
        .into_iter()
        .map(|path| {
            // Return paths relative to the search base; fall back to
            // absolute if the path is somehow not under base.
            let relative = path.strip_prefix(&base).unwrap_or(&path);
            relative.display().to_string()
        })
        .take(max_results)
        .collect();

    // Detect whether we hit the cap (collect stopped early).
    // Re-check against the pre-cap count is expensive; we simply inspect
    // whether we filled the bucket to the brim.
    let capped = matches.len() >= max_results;

    if matches.is_empty() {
        Ok(format!("No files matched pattern: {pattern}"))
    } else {
        let count = matches.len();
        let cap_note = if capped {
            format!("\n\n[Capped at {max_results} results]")
        } else {
            String::new()
        };
        Ok(format!(
            "{count} files matched:\n{}{cap_note}",
            matches.join("\n")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use koda_sandbox::fs::LocalFileSystem;
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
        let result = glob_search(tmp.path(), &args, 200, &LocalFileSystem::new())
            .await
            .unwrap();
        assert!(result.contains("main.rs"));
        assert!(result.contains("lib.rs"));
    }

    #[tokio::test]
    async fn test_glob_toml() {
        let tmp = setup();
        let args = json!({ "pattern": "*.toml" });
        let result = glob_search(tmp.path(), &args, 200, &LocalFileSystem::new())
            .await
            .unwrap();
        assert!(result.contains("Cargo.toml"));
    }

    #[tokio::test]
    async fn test_glob_no_match() {
        let tmp = setup();
        let args = json!({ "pattern": "**/*.xyz" });
        let result = glob_search(tmp.path(), &args, 200, &LocalFileSystem::new())
            .await
            .unwrap();
        assert!(result.contains("No files matched"));
    }

    #[tokio::test]
    async fn test_glob_scoped_path() {
        let tmp = setup();
        let args = json!({ "pattern": "*.rs", "path": "src/tools" });
        let result = glob_search(tmp.path(), &args, 200, &LocalFileSystem::new())
            .await
            .unwrap();
        assert!(result.contains("mod.rs"));
        assert!(!result.contains("main.rs")); // Not in src/tools
    }

    #[tokio::test]
    async fn test_glob_capped_results() {
        let tmp = setup();
        // Cap at 2 results — there are 3 .rs files
        let args = json!({ "pattern": "**/*.rs" });
        let result = glob_search(tmp.path(), &args, 2, &LocalFileSystem::new())
            .await
            .unwrap();
        assert!(
            result.contains("Capped"),
            "should show cap message: {result}"
        );
    }

    #[tokio::test]
    async fn test_glob_missing_pattern_errors() {
        let tmp = setup();
        let args = json!({});
        let result = glob_search(tmp.path(), &args, 200, &LocalFileSystem::new()).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("pattern"));
    }

    #[tokio::test]
    async fn test_glob_specific_filename() {
        let tmp = setup();
        let args = json!({ "pattern": "**/mod.rs" });
        let result = glob_search(tmp.path(), &args, 200, &LocalFileSystem::new())
            .await
            .unwrap();
        assert!(result.contains("mod.rs"));
        assert!(!result.contains("main.rs"));
    }
}
