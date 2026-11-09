//! Grep tool — recursive text search across files.
//!
//! Uses the `FileSystem` trait (Phase 2d, #934) to walk directories
//! (respecting `.gitignore`) and search for text patterns. This lets
//! `LocalFileSystem` walk the host tree directly, while `SandboxedFileSystem`
//! routes the same walk through the policy-enforcing worker process.
//!
//! The options follow Claude Code's Grep, the peer Koda is measured against
//! in evals: a `glob` or `type` filter, three output modes with paths only by
//! default, context lines, `multiline`, and pages of `head_limit` entries
//! from `offset`. The default page size is set by `OutputCaps`
//! (context-scaled).
//!
//! ## Parameters
//!
//! - **`pattern`** (required) — Regex to search for; one that doesn't compile
//!   is searched as literal text
//! - **`file_path`** (optional, default `.`; `path` also works) — Directory or
//!   file to search in
//! - **`glob`**, **`type`** (optional) — Only files matching an rg-style glob,
//!   or of one of rg's file types
//! - **`output_mode`** (optional) — `files_with_matches` (default), `content`
//!   or `count`
//! - **`context`**, **`context_before`**, **`context_after`** (optional) —
//!   Lines around each match, in `content` mode
//! - **`case_insensitive`**, **`multiline`** (optional) — Regex flags
//! - **`head_limit`**, **`offset`** (optional) — Page through the entries
//!
//! ## Output format
//!
//! `files_with_matches`: one path per line, newest first. `content`:
//! `path:line:text` for a match and `path-line-text` for context, with `--`
//! between separate runs of lines. `count`: `path:count`. A page that doesn't
//! reach the end says which `offset` continues it.
//!
//! ## When to use Grep vs Bash
//!
//! - **Grep tool**: fast, respects `.gitignore`, context-aware output caps
//! - **`bash: grep/rg`**: only when you need complex flags the tool doesn't expose

use super::resolve_read_path;
use crate::providers::ToolDefinition;
use anyhow::Result;
use koda_sandbox::fs::FileSystem;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Return tool definitions for the LLM.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "Grep".to_string(),
        description: "Search file contents for a regex (respects .gitignore). \
            By default it returns the paths of the files that match, newest first; \
            output_mode='content' returns the matching lines as path:line:text, with \
            context lines if you ask, and 'count' returns the matches per file. \
            Narrow it with glob (e.g. '*.rs', 'src/**/*.{ts,tsx}') or type (e.g. 'rust', 'py'). \
            Results come in pages of head_limit entries; a page that stops short says \
            which offset continues it. \
            The pattern is a regex, so `foo|bar` finds either word; \
            escape special characters to match them literally. \
            Prefer this over Bash + rg/grep."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "The regex to search for"
                },
                "file_path": {
                    "type": "string",
                    "description": "Directory or file to search in (default: project root)"
                },
                "glob": {
                    "type": "string",
                    "description": "Only search files matching this glob, as rg --glob takes it (e.g. '*.rs', '*.{ts,tsx}', 'src/**/*.py'; '!*.md' leaves those files out)"
                },
                "type": {
                    "type": "string",
                    "description": "Only search files of this type, as rg --type takes it (e.g. 'rust', 'py', 'js', 'go', 'md')"
                },
                "output_mode": {
                    "type": "string",
                    "enum": ["files_with_matches", "content", "count"],
                    "description": "files_with_matches: paths only (default). content: matching lines with line numbers. count: matches per file."
                },
                "context": {
                    "type": "integer",
                    "description": "Lines to show before and after each match (content mode only)"
                },
                "context_before": {
                    "type": "integer",
                    "description": "Lines to show before each match (content mode only)"
                },
                "context_after": {
                    "type": "integer",
                    "description": "Lines to show after each match (content mode only)"
                },
                "case_insensitive": {
                    "type": "boolean",
                    "description": "Whether to ignore case (default: false)"
                },
                "multiline": {
                    "type": "boolean",
                    "description": "Let the pattern span lines, with . matching newlines (default: false)"
                },
                "head_limit": {
                    "type": "integer",
                    "description": "Entries to return: paths, lines or counts (default: one page)"
                },
                "offset": {
                    "type": "integer",
                    "description": "Entries to skip before the page starts (default: 0)"
                }
            },
            "required": ["pattern"]
        }),
    }]
}

/// One match: the file, and the lines it starts and ends on (1-based; the
/// same line unless `multiline` let it span more).
struct Hit {
    path: PathBuf,
    line: usize,
    end_line: usize,
    text: String,
}

/// Which files a search may report, from `glob` and `type`.
struct PathFilter {
    glob: Option<ignore::overrides::Override>,
    types: Option<ignore::types::Types>,
}

impl PathFilter {
    fn new(root: &Path, glob: Option<&str>, file_type: Option<&str>) -> Result<Self> {
        let glob = glob
            .map(|g| -> Result<_> {
                let mut builder = ignore::overrides::OverrideBuilder::new(root);
                builder
                    .add(g)
                    .map_err(|e| anyhow::anyhow!("Invalid glob {g:?}: {e}"))?;
                builder
                    .build()
                    .map_err(|e| anyhow::anyhow!("Invalid glob {g:?}: {e}"))
            })
            .transpose()?;
        let types = file_type
            .map(|t| -> Result<_> {
                let mut builder = ignore::types::TypesBuilder::new();
                builder.add_defaults().select(t);
                builder.build().map_err(|_| {
                    anyhow::anyhow!(
                        "Unknown file type {t:?}. Use one of rg's types, such as rust, py, \
                         js, ts, go, java, c, cpp, md, toml, json or yaml, or a glob instead."
                    )
                })
            })
            .transpose()?;
        Ok(Self { glob, types })
    }

    fn allows(&self, path: &Path) -> bool {
        // As rg reads an override: a path no glob names is excluded only when
        // some glob selects files, so a lone `!*.md` keeps everything else.
        self.glob
            .as_ref()
            .is_none_or(|g| !g.matched(path, false).is_ignore())
            && self
                .types
                .as_ref()
                .is_none_or(|t| t.matched(path, false).is_whitelist())
    }
}

/// Search for a text pattern across files in a directory.
///
/// The pattern is a **regex**. One that doesn't compile, like `fn (`, is
/// escaped and searched as literal text, and the output says so.
/// `case_insensitive=true` prepends a `(?i)` inline flag so the regex engine
/// handles case folding; `multiline=true` adds `(?s)` and searches whole files.
/// `page` is the number of entries returned when the model sets no
/// `head_limit`.
pub async fn grep(
    project_root: &Path,
    args: &Value,
    page: usize,
    fs: &dyn FileSystem,
) -> Result<String> {
    let raw_pattern = args["pattern"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'pattern' argument"))?;
    let path_str = args["file_path"]
        .as_str()
        .or_else(|| args["path"].as_str())
        .unwrap_or(".");
    let case_insensitive = args["case_insensitive"].as_bool().unwrap_or(false);
    let multiline = args["multiline"].as_bool().unwrap_or(false);
    let mode = args["output_mode"].as_str().unwrap_or("files_with_matches");
    if !matches!(mode, "files_with_matches" | "content" | "count") {
        anyhow::bail!("output_mode must be files_with_matches, content or count, not {mode:?}");
    }
    let count_arg = |name: &str| args[name].as_u64().map(|n| n as usize);
    let context = count_arg("context").unwrap_or(0);
    let before = count_arg("context_before").unwrap_or(context);
    let after = count_arg("context_after").unwrap_or(context);
    let head_limit = count_arg("head_limit").unwrap_or(page).max(1);
    let offset = count_arg("offset").unwrap_or(0);

    let search_root = resolve_read_path(project_root, path_str)?;
    let filter = PathFilter::new(&search_root, args["glob"].as_str(), args["type"].as_str())?;

    // Search as a regex, as the tool definition says. An invalid pattern falls
    // back to literal text, so "fn (" still works (#807 family).
    let is_regex = regex::Regex::new(raw_pattern).is_ok();
    let pattern = if is_regex {
        raw_pattern.to_string()
    } else {
        regex::escape(raw_pattern)
    };
    let flags = match (case_insensitive, multiline) {
        (false, false) => "",
        (true, false) => "(?i)",
        (false, true) => "(?s)",
        (true, true) => "(?is)",
    };
    let regex_pattern = format!("{flags}{pattern}");
    let literal_note = (!is_regex).then_some("[Not a valid regex: searched as literal text.]");

    let mut hits = if multiline {
        search_whole_files(&regex_pattern, &search_root, &filter, fs).await?
    } else {
        fs.grep(&regex_pattern, &search_root, None)
            .await
            .map_err(|e| anyhow::anyhow!("Grep error: {e}"))?
            .into_iter()
            .filter(|m| filter.allows(&m.path))
            .map(|m| Hit {
                path: m.path,
                line: m.line,
                end_line: m.line,
                text: m.text,
            })
            .collect()
    };
    hits.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));

    if hits.is_empty() {
        let mut out = format!("No matches found for '{raw_pattern}'");
        if let Some(g) = args["glob"].as_str() {
            out.push_str(&format!(" in files matching {g}"));
        }
        if let Some(t) = args["type"].as_str() {
            out.push_str(&format!(" in {t} files"));
        }
        if let Some(note) = literal_note {
            out.push_str(&format!("\n{note}"));
        }
        return Ok(out);
    }

    let rel = |p: &Path| -> String {
        p.strip_prefix(project_root)
            .unwrap_or(p)
            .display()
            .to_string()
    };
    let mut per_file: BTreeMap<&Path, Vec<&Hit>> = BTreeMap::new();
    for hit in &hits {
        per_file.entry(&hit.path).or_default().push(hit);
    }

    let (header, entries) = match mode {
        "files_with_matches" => {
            let mut files: Vec<(Option<std::time::SystemTime>, &Path)> = Vec::new();
            for path in per_file.keys() {
                let modified = tokio::fs::metadata(path)
                    .await
                    .ok()
                    .and_then(|m| m.modified().ok());
                files.push((modified, path));
            }
            // Newest first, as Claude Code and Codex list them; ties by path.
            files.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
            let n = files.len();
            (
                if n == 1 {
                    "1 file contains matches:".to_string()
                } else {
                    format!("{n} files contain matches:")
                },
                files.into_iter().map(|(_, p)| rel(p)).collect::<Vec<_>>(),
            )
        }
        "count" => {
            let (n, files) = (hits.len(), per_file.len());
            (
                format!(
                    "{n} match{} in {files} file{}:",
                    plural_es(n),
                    plural(files)
                ),
                per_file
                    .iter()
                    .map(|(p, h)| format!("{}:{}", rel(p), h.len()))
                    .collect(),
            )
        }
        _ => {
            let (n, files) = (hits.len(), per_file.len());
            let mut lines = Vec::new();
            for (path, file_hits) in &per_file {
                content_lines(path, &rel(path), file_hits, before, after, fs, &mut lines).await;
            }
            (
                format!(
                    "{n} match{} in {files} file{}:",
                    plural_es(n),
                    plural(files)
                ),
                lines,
            )
        }
    };

    let total = entries.len();
    let mut out = header;
    if offset >= total {
        out.push_str(&format!(
            "\n[offset {offset} is past the end: there are {total} entries.]"
        ));
    } else {
        let end = (offset + head_limit).min(total);
        for entry in &entries[offset..end] {
            out.push('\n');
            out.push_str(entry);
        }
        if end < total || offset > 0 {
            out.push_str(&format!(
                "\n\n[Showing entries {}-{end} of {total}.",
                offset + 1
            ));
            if end < total {
                out.push_str(&format!(" Continue with offset={end}."));
            }
            out.push(']');
        }
    }
    if let Some(note) = literal_note {
        out.push_str(&format!("\n\n{note}"));
    }
    Ok(out)
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn plural_es(n: usize) -> &'static str {
    if n == 1 { "" } else { "es" }
}

/// Matches of `regex_pattern` across whole files, for `multiline`. Reads go
/// through `fs`, like the line search, so the sandbox sees them.
async fn search_whole_files(
    regex_pattern: &str,
    root: &Path,
    filter: &PathFilter,
    fs: &dyn FileSystem,
) -> Result<Vec<Hit>> {
    let re = regex::Regex::new(regex_pattern)
        .map_err(|e| anyhow::anyhow!("Grep error: invalid regex: {e}"))?;
    let files = if tokio::fs::metadata(root).await.is_ok_and(|m| m.is_file()) {
        vec![root.to_path_buf()]
    } else {
        fs.glob("**/*", root)
            .await
            .map_err(|e| anyhow::anyhow!("Grep error: {e}"))?
    };
    let mut hits = Vec::new();
    for path in files {
        if !filter.allows(&path) {
            continue;
        }
        let Ok(bytes) = fs.read(&path, None).await else {
            continue;
        };
        // Not UTF-8 means binary: skipped, as the line search does.
        let Ok(content) = String::from_utf8(bytes) else {
            continue;
        };
        for m in re.find_iter(&content) {
            let line = content[..m.start()].matches('\n').count() + 1;
            let end_line = line + m.as_str().trim_end_matches('\n').matches('\n').count();
            let text = m.as_str().lines().next().unwrap_or("").to_string();
            hits.push(Hit {
                path: path.clone(),
                line,
                end_line,
                text,
            });
        }
    }
    Ok(hits)
}

/// `content` mode's lines for one file: `path:line:text` for a matched line
/// and `path-line-text` for context, with `--` between separate runs. Context
/// and multi-line matches need the file's lines; if it can't be read again,
/// the matched lines alone are shown.
async fn content_lines(
    path: &Path,
    shown_path: &str,
    hits: &[&Hit],
    before: usize,
    after: usize,
    fs: &dyn FileSystem,
    out: &mut Vec<String>,
) {
    let needs_file = before > 0 || after > 0 || hits.iter().any(|h| h.end_line > h.line);
    let file_lines: Option<Vec<String>> = if needs_file {
        fs.read(path, None).await.ok().map(|bytes| {
            String::from_utf8_lossy(&bytes)
                .lines()
                .map(str::to_string)
                .collect()
        })
    } else {
        None
    };
    let Some(file_lines) = file_lines else {
        for hit in hits {
            out.push(format!(
                "{shown_path}:{}:{}",
                hit.line,
                truncate_line(&hit.text, 200)
            ));
        }
        return;
    };

    let matched: std::collections::BTreeSet<usize> =
        hits.iter().flat_map(|h| h.line..=h.end_line).collect();
    let mut shown_until = 0; // last line number written
    for hit in hits {
        let from = hit.line.saturating_sub(before).max(1);
        let to = (hit.end_line + after).min(file_lines.len());
        let from = from.max(shown_until + 1);
        if from > to {
            continue;
        }
        if shown_until > 0 && from > shown_until + 1 {
            out.push("--".to_string());
        }
        for n in from..=to {
            let sep = if matched.contains(&n) { ':' } else { '-' };
            out.push(format!(
                "{shown_path}{sep}{n}{sep}{}",
                truncate_line(&file_lines[n - 1], 200)
            ));
        }
        shown_until = to;
    }
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

// =============================================================
// Tool trait implementation (#1265 item 5, PR-5/N).
//
// `Grep` is read-only — no `mutated_path` override needed.
// Reads `caps.grep_matches` off the context (added to `ToolExecCtx`
// in PR-4 alongside `caps.list_entries`).
// =============================================================

use crate::tools::{Tool, ToolEffect, ToolExecCtx, ToolResult};
use async_trait::async_trait;

/// `Grep` — ripgrep-style content search.
pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "Grep"
    }
    fn definition(&self) -> ToolDefinition {
        definitions()
            .into_iter()
            .find(|d| d.name == "Grep")
            .expect("grep::definitions() must contain Grep")
    }
    fn classify(&self, _args: &serde_json::Value) -> ToolEffect {
        ToolEffect::ReadOnly
    }
    async fn execute(&self, ctx: &ToolExecCtx<'_>, args: &serde_json::Value) -> ToolResult {
        let r = grep(ctx.project_root, args, ctx.caps.grep_matches, ctx.fs).await;
        crate::tools::wrap_result(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use koda_sandbox::fs::LocalFileSystem;
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
        let result = grep(tmp.path(), &args, 100, &LocalFileSystem::new())
            .await
            .unwrap();
        assert!(result.contains("hello.rs"));
        assert!(result.contains("lib.rs"));
    }

    #[tokio::test]
    async fn test_grep_no_matches() {
        let tmp = setup_test_dir();
        let args = json!({ "pattern": "zzzznotfound" });
        let result = grep(tmp.path(), &args, 100, &LocalFileSystem::new())
            .await
            .unwrap();
        assert!(result.contains("No matches"));
    }

    #[tokio::test]
    async fn test_grep_case_insensitive() {
        let tmp = setup_test_dir();
        let args = json!({ "pattern": "HELLO", "case_insensitive": true });
        let result = grep(tmp.path(), &args, 100, &LocalFileSystem::new())
            .await
            .unwrap();
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
        let result = grep(tmp.path(), &args, 100, &LocalFileSystem::new())
            .await
            .unwrap();
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
        let result = grep(tmp.path(), &args, 100, &LocalFileSystem::new())
            .await
            .unwrap();
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
        // A page of 5 files.
        let args = json!({ "pattern": "needle" });
        let result = grep(tmp.path(), &args, 5, &LocalFileSystem::new())
            .await
            .unwrap();
        assert!(result.starts_with("20 files contain matches:"), "{result}");
        assert_eq!(
            result.lines().filter(|l| l.ends_with(".rs")).count(),
            5,
            "{result}"
        );
        assert!(
            result.contains("[Showing entries 1-5 of 20. Continue with offset=5.]"),
            "{result}"
        );
    }

    #[tokio::test]
    async fn test_grep_file_path_param_alias() {
        // The tool accepts both "path" and "file_path" as the directory param.
        let tmp = setup_test_dir();
        let args = json!({ "pattern": "nope", "file_path": "nested" });
        let result = grep(tmp.path(), &args, 100, &LocalFileSystem::new())
            .await
            .unwrap();
        assert!(result.contains("deep.rs"));
    }

    #[tokio::test]
    async fn test_grep_invalid_regex_falls_back_to_literal() {
        let tmp = TempDir::new().unwrap();
        // Write a file containing the literal string "fn (" — the parens are regex specials.
        std::fs::write(tmp.path().join("code.rs"), "fn (invalid syntax").unwrap();
        // As a regex, "fn (" is a parse error, so it is searched as literal text.
        let args = json!({ "pattern": "fn (" });
        let result = grep(tmp.path(), &args, 100, &LocalFileSystem::new())
            .await
            .unwrap();
        assert!(
            result.contains("code.rs"),
            "literal paren should be matched without regex error"
        );
        assert!(result.contains("searched as literal text"), "{result}");
    }

    #[tokio::test]
    async fn test_grep_invalid_regex_with_no_match_says_it_was_literal() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("code.rs"), "fn main() {}").unwrap();
        let args = json!({ "pattern": "absent (" });
        let result = grep(tmp.path(), &args, 100, &LocalFileSystem::new())
            .await
            .unwrap();
        assert!(result.contains("No matches"), "{result}");
        assert!(result.contains("searched as literal text"), "{result}");
    }

    #[tokio::test]
    async fn test_grep_pattern_is_a_regex() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("code.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();
        for pattern in ["alpha|beta", r"fn \w+\(\)"] {
            let args = json!({ "pattern": pattern, "output_mode": "content" });
            let result = grep(tmp.path(), &args, 100, &LocalFileSystem::new())
                .await
                .unwrap();
            assert!(result.starts_with("2 matches"), "{pattern}: {result}");
            assert!(!result.contains("literal"), "{pattern}: {result}");
        }
    }

    #[tokio::test]
    async fn test_grep_regex_honors_case_insensitive() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("code.rs"), "ALPHA\nbeta\ngamma\n").unwrap();
        let args = json!({
            "pattern": "alpha|BETA", "case_insensitive": true, "output_mode": "content"
        });
        let result = grep(tmp.path(), &args, 100, &LocalFileSystem::new())
            .await
            .unwrap();
        assert!(result.starts_with("2 matches"), "{result}");
    }

    #[tokio::test]
    async fn test_grep_searches_a_single_file() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "needle\n").unwrap();
        std::fs::write(tmp.path().join("b.rs"), "needle\n").unwrap();
        let args =
            json!({ "pattern": "needle|pin", "file_path": "a.rs", "output_mode": "content" });
        let result = grep(tmp.path(), &args, 100, &LocalFileSystem::new())
            .await
            .unwrap();
        assert!(result.starts_with("1 match in 1 file"), "{result}");
        assert!(result.contains("a.rs"), "{result}");
    }

    async fn run(dir: &Path, args: Value) -> String {
        grep(dir, &args, 100, &LocalFileSystem::new())
            .await
            .unwrap()
    }

    /// Paths only is the default, as in Claude Code's Grep.
    #[tokio::test]
    async fn test_grep_lists_files_by_default() {
        let tmp = setup_test_dir();
        let result = run(tmp.path(), json!({ "pattern": "println" })).await;
        assert!(result.starts_with("2 files contain matches:"), "{result}");
        assert!(!result.contains("println"), "{result}");
        assert!(result.lines().any(|l| l == "hello.rs") && result.lines().any(|l| l == "lib.rs"));
    }

    #[tokio::test]
    async fn test_grep_lists_the_newest_file_first() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "needle").unwrap();
        std::fs::write(tmp.path().join("b.rs"), "needle").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(tmp.path().join("b.rs"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        let result = run(tmp.path(), json!({ "pattern": "needle" })).await;
        let files: Vec<&str> = result.lines().skip(1).collect();
        assert_eq!(files, ["a.rs", "b.rs"], "{result}");
    }

    #[tokio::test]
    async fn test_grep_glob_filters_like_rg() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("src/ui")).unwrap();
        for f in [
            "src/ui/view.ts",
            "src/ui/view.tsx",
            "src/main.rs",
            "notes.md",
        ] {
            std::fs::write(tmp.path().join(f), "needle").unwrap();
        }
        let only = |result: &str| -> Vec<String> {
            let mut v: Vec<String> = result.lines().skip(1).map(str::to_string).collect();
            v.sort();
            v
        };
        let braces = run(
            tmp.path(),
            json!({ "pattern": "needle", "glob": "*.{ts,tsx}" }),
        )
        .await;
        assert_eq!(
            only(&braces),
            ["src/ui/view.ts", "src/ui/view.tsx"],
            "{braces}"
        );
        let path = run(
            tmp.path(),
            json!({ "pattern": "needle", "glob": "src/**/*.rs" }),
        )
        .await;
        assert_eq!(only(&path), ["src/main.rs"], "{path}");
        let none = run(tmp.path(), json!({ "pattern": "needle", "glob": "*.py" })).await;
        assert!(
            none.starts_with("No matches found for 'needle' in files matching *.py"),
            "{none}"
        );
        // A `!` glob excludes, as in `rg --glob '!…'`: everything else is searched.
        let not_md = run(tmp.path(), json!({ "pattern": "needle", "glob": "!*.md" })).await;
        assert_eq!(
            only(&not_md),
            ["src/main.rs", "src/ui/view.ts", "src/ui/view.tsx"],
            "{not_md}"
        );
        let not_src = run(
            tmp.path(),
            json!({ "pattern": "needle", "glob": "!src/**" }),
        )
        .await;
        assert_eq!(only(&not_src), ["notes.md"], "{not_src}");
    }

    #[tokio::test]
    async fn test_grep_type_filters_like_rg() {
        let tmp = setup_test_dir();
        std::fs::write(tmp.path().join("readme.md"), "hello").unwrap();
        let result = run(tmp.path(), json!({ "pattern": "hello", "type": "md" })).await;
        assert!(
            result.starts_with("1 file contains matches:\nreadme.md"),
            "{result}"
        );
        let err = grep(
            tmp.path(),
            &json!({ "pattern": "hello", "type": "no-such-type" }),
            100,
            &LocalFileSystem::new(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("Unknown file type"), "{err}");
    }

    #[tokio::test]
    async fn test_grep_count_mode() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "x\nx\nx\n").unwrap();
        std::fs::write(tmp.path().join("b.rs"), "x\n").unwrap();
        let result = run(
            tmp.path(),
            json!({ "pattern": "x", "output_mode": "count" }),
        )
        .await;
        assert_eq!(result, "4 matches in 2 files:\na.rs:3\nb.rs:1");
    }

    #[tokio::test]
    async fn test_grep_context_lines_and_separators() {
        let tmp = TempDir::new().unwrap();
        let body: String = (1..=12)
            .map(|i| {
                if i == 3 || i == 10 {
                    "hit\n".into()
                } else {
                    format!("l{i}\n")
                }
            })
            .collect();
        std::fs::write(tmp.path().join("f.txt"), body).unwrap();
        let result = run(
            tmp.path(),
            json!({ "pattern": "hit", "output_mode": "content", "context": 1 }),
        )
        .await;
        assert_eq!(
            result,
            "2 matches in 1 file:\nf.txt-2-l2\nf.txt:3:hit\nf.txt-4-l4\n--\nf.txt-9-l9\nf.txt:10:hit\nf.txt-11-l11"
        );
        let after = run(
            tmp.path(),
            json!({ "pattern": "hit", "output_mode": "content", "context_after": 1, "head_limit": 2 }),
        )
        .await;
        assert!(
            after.starts_with("2 matches in 1 file:\nf.txt:3:hit\nf.txt-4-l4\n\n[Showing entries 1-2 of 5. Continue with offset=2.]"),
            "{after}"
        );
    }

    #[tokio::test]
    async fn test_grep_offset_continues_and_says_when_past_the_end() {
        let tmp = TempDir::new().unwrap();
        for i in 0..3 {
            std::fs::write(tmp.path().join(format!("f{i}.rs")), "needle").unwrap();
        }
        let page = |offset: u64| json!({ "pattern": "needle", "output_mode": "count", "head_limit": 2, "offset": offset });
        let second = run(tmp.path(), page(2)).await;
        assert_eq!(
            second,
            "3 matches in 3 files:\nf2.rs:1\n\n[Showing entries 3-3 of 3.]"
        );
        let past = run(tmp.path(), page(9)).await;
        assert!(
            past.ends_with("[offset 9 is past the end: there are 3 entries.]"),
            "{past}"
        );
    }

    #[tokio::test]
    async fn test_grep_multiline_spans_lines() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("m.rs"),
            "fn a() {\n    one();\n}\nfn b() {}\n",
        )
        .unwrap();
        let single = run(tmp.path(), json!({ "pattern": r"fn a\(\) \{.*\}" })).await;
        assert!(single.starts_with("No matches"), "{single}");
        let result = run(
            tmp.path(),
            json!({ "pattern": r"fn a\(\) \{.*?\}", "multiline": true, "output_mode": "content" }),
        )
        .await;
        assert_eq!(
            result,
            "1 match in 1 file:\nm.rs:1:fn a() {\nm.rs:2:    one();\nm.rs:3:}"
        );
    }

    #[tokio::test]
    async fn test_grep_rejects_an_unknown_output_mode() {
        let tmp = setup_test_dir();
        let err = grep(
            tmp.path(),
            &json!({ "pattern": "x", "output_mode": "lines" }),
            100,
            &LocalFileSystem::new(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("output_mode must be"), "{err}");
    }

    // ── Tool trait invariants (#1265 PR-5) ─────────────────────

    #[test]
    fn grep_tool_metadata_matches_definition() {
        let t = GrepTool;
        assert_eq!(t.name(), "Grep");
        assert_eq!(t.definition().name, "Grep");
        assert_eq!(t.classify(&serde_json::json!({})), ToolEffect::ReadOnly);
        // Read-only tools change no file.
        assert!(t.mutated_path(&serde_json::json!({})).is_none());
    }

    #[tokio::test]
    async fn grep_tool_dispatches_through_trait() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "hello world").unwrap();
        let cache: crate::tools::FileReadCache =
            std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let fs = LocalFileSystem::new();
        let caps = crate::output_caps::OutputCaps::for_context(100_000);
        let bg = crate::tools::bg_process::BgRegistry::new();
        let trust = crate::trust::TrustMode::Safe;
        let policy = koda_sandbox::SandboxPolicy::default();
        let skills = crate::skills::SkillRegistry::default();
        let ctx = crate::tools::ToolExecCtx::for_test(
            tmp.path(),
            &cache,
            &fs,
            &caps,
            &bg,
            &trust,
            &policy,
            &skills,
        );
        let tool: Box<dyn Tool> = Box::new(GrepTool);
        let result = tool
            .execute(&ctx, &serde_json::json!({"pattern": "hello"}))
            .await;
        assert!(result.success, "{}", result.output);
        assert!(result.output.contains("a.txt"));
    }
}
