//! Response display formatting with hierarchical layout and smart graying.
//!
//! Renders tool calls with icons and key arguments, and formats LLM responses
//! with visual hierarchy: important content stays bright, verbose narration
//! is dimmed, and metadata is shown in a footer.

use crate::providers::ToolCall;

// ── ANSI helpers ──────────────────────────────────────────────────

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[90m";
const BOLD: &str = "\x1b[1m";

// Semantic color palette (our own colors)
//
// Cool blues/teals  → reading & navigation (calm, informational)
// Warm tones        → actions & changes (edits, shell, delete)
// Purples           → AI thinking & reasoning (the "brain" colors)
// Greens            → completions & success (your answer)
// Neutrals          → search & listings

// Cool — informational
const STEEL_BLUE: &str = "\x1b[38;2;95;135;175m"; // read (calm, reading)
const SKY_BLUE: &str = "\x1b[38;2;0;135;215m"; // list (navigation, exploring)

// Warm — actions that change things
const AMBER: &str = "\x1b[38;2;175;135;0m"; // edit (modification, gold)
const ORANGE: &str = "\x1b[38;2;200;100;0m"; // shell (system action)
const CRIMSON: &str = "\x1b[38;2;200;40;40m"; // delete (danger)

// AI — thinking & delegation
const VIOLET: &str = "\x1b[38;2;130;80;200m"; // thinking (contemplation)
const RUBY: &str = "\x1b[38;2;175;50;120m"; // agent (delegation)

// Neutral
const SILVER: &str = "\x1b[38;2;140;140;140m"; // search (neutral)
const CYAN: &str = "\x1b[38;2;0;180;200m"; // MCP (external, distinct)

// Success — the most important
const EMERALD: &str = "\x1b[38;2;42;160;60m"; // response (your answer!)

// ── Tool call display (dot + label + detail, flush left) ───────

/// Print a tool call with a colored dot and label (flush left).
pub fn print_tool_call(tc: &ToolCall, is_sub_agent: bool) {
    let indent = if is_sub_agent { "  " } else { "" };
    let (color, label, detail) = tool_info(&tc.function_name, &tc.arguments);

    // ShareReasoning gets special rendering: thinking banner + structured block
    if tc.function_name == "ShareReasoning" {
        print_thinking_banner();
        let args: serde_json::Value = serde_json::from_str(&tc.arguments).unwrap_or_default();
        if let Some(reasoning) = args.get("reasoning").and_then(|v| v.as_str()) {
            render_thinking_block(reasoning);
        }
        return;
    }

    if tc.function_name == "Bash" {
        println!("\n{indent}{color}\u{25cf}{RESET} {BOLD}{label}{RESET} {detail}");
    } else {
        println!("\n{indent}{color}\u{25cf}{RESET} {BOLD}{label}{RESET} {DIM}{detail}{RESET}");
    }

    // Show extra metadata for shell commands
    if tc.function_name == "Bash" {
        let args: serde_json::Value = serde_json::from_str(&tc.arguments).unwrap_or_default();
        let timeout = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(60);
        let cwd = args.get("cwd").and_then(|v| v.as_str());

        let mut meta = format!("{indent}  {DIM}\u{23f1} Timeout: {timeout}s");
        if let Some(dir) = cwd {
            meta.push_str(&format!("  \u{1f4c2} {dir}"));
        }
        meta.push_str(RESET);
        println!("{meta}");
    }
}

/// Map tool names to accent color, short label, and detail.
pub fn tool_info(name: &str, args_json: &str) -> (&'static str, &'static str, String) {
    let args: serde_json::Value = serde_json::from_str(args_json).unwrap_or_default();

    match name {
        "Read" => {
            let path = json_str_multi(&args, &["file_path", "path"]);
            let line_info = if let Some(start) = args.get("start_line").and_then(|v| v.as_i64()) {
                let num = args.get("num_lines").and_then(|v| v.as_i64()).unwrap_or(0);
                format!("{path} (lines {start}-{})", start + num - 1)
            } else {
                path
            };
            (STEEL_BLUE, "Read", line_info)
        }
        "List" => {
            let dir = json_str_or_multi(&args, &["directory", "path", "dir"], ".");
            let recursive = args
                .get("recursive")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let mode = if recursive { "recursive" } else { "flat" };
            (SKY_BLUE, "List", format!("{dir} ({mode})"))
        }
        "Write" => {
            let path = json_str_multi(&args, &["file_path", "path"]);
            let overwrite = args
                .get("overwrite")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let action = if overwrite { "overwrite" } else { "create" };
            (AMBER, "Write", format!("{path} ({action})"))
        }
        "Edit" => {
            let path = extract_edit_path(&args);
            let action = extract_edit_action(&args);
            (AMBER, "Edit", format!("{path} ({action})"))
        }
        "Delete" => {
            let path = json_str_multi(&args, &["file_path", "path"]);
            let recursive = args
                .get("recursive")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let detail = if recursive {
                format!("{path} (recursive)")
            } else {
                path
            };
            (CRIMSON, "Delete", detail)
        }
        "Grep" => {
            let pattern = json_str_multi(&args, &["search_string", "pattern", "query"]);
            let dir = json_str_or_multi(&args, &["directory", "path"], ".");
            let short = truncate(&pattern, 30);
            (SILVER, "Search", format!("{dir} for '{short}'"))
        }
        "Glob" => {
            let pattern = json_str_multi(&args, &["pattern"]);
            let dir = json_str_or_multi(&args, &["path"], ".");
            (SILVER, "Glob", format!("{dir} → {pattern}"))
        }
        "Bash" => {
            let cmd = json_str_multi(&args, &["command", "cmd"]);
            let short = truncate(&cmd, 60);
            (ORANGE, "Shell", format!("$ {short}"))
        }
        "WebFetch" => {
            let url = json_str_multi(&args, &["url"]);
            let short = truncate(&url, 60);
            (SKY_BLUE, "Fetch", short)
        }
        "InvokeAgent" => {
            let agent = json_str_multi(&args, &["agent_name", "name"]);
            (RUBY, "Agent", agent)
        }
        "CreateAgent" => {
            let name = json_str_multi(&args, &["name"]);
            (VIOLET, "Create", format!("agent: {name}"))
        }
        "MemoryRead" => (SILVER, "Memory", "reading memory".to_string()),
        "MemoryWrite" => {
            let scope = args
                .get("scope")
                .and_then(|v| v.as_str())
                .unwrap_or("project");
            let content = json_str_multi(&args, &["content"]);
            let short = truncate(&content, 40);
            (AMBER, "Memory", format!("{scope}: {short}"))
        }
        "ShareReasoning" => {
            let title = args
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("reasoning");
            (VIOLET, "Thinking", title.to_string())
        }
        "TodoWrite" => {
            let content = args
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let total = content.lines().filter(|l| l.trim().starts_with("- [")).count();
            let done = content.lines().filter(|l| {
                let t = l.trim();
                t.starts_with("- [x]") || t.starts_with("- [X]")
            }).count();
            (EMERALD, "Todo", format!("{done}/{total} complete"))
        }
        // MCP tools: server_name.tool_name
        other if other.contains('.') => {
            let parts: Vec<&str> = other.splitn(2, '.').collect();
            let server = parts[0];
            let tool = parts.get(1).copied().unwrap_or(other);
            let summary = format!("{server}::{tool}");
            (CYAN, "MCP", summary)
        }
        _ => (DIM, "Tool", name.to_string()),
    }
}

/// Detect what kind of edit operation (create, replace, delete).
fn extract_edit_action(args: &serde_json::Value) -> &'static str {
    if let Some(payload) = args.get("payload") {
        if payload.get("content").is_some() {
            return if payload
                .get("overwrite")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                "overwrite"
            } else {
                "create"
            };
        }
        if payload.get("replacements").is_some() {
            return "replace";
        }
        if payload.get("delete_snippet").is_some() {
            return "delete snippet";
        }
    }
    "modify"
}

/// Print a sub-agent delegation header.
pub fn print_sub_agent_start(agent_name: &str) {
    println!("\n{RUBY}●{RESET} {BOLD}Agent{RESET} {DIM}Delegating → {agent_name}{RESET}");
}

/// Print the THINKING indicator (for `<think>` blocks from reasoning models).
pub fn print_thinking_banner() {
    println!("\n{VIOLET}\u{1f36f}{RESET} {BOLD}Thinking{RESET} {DIM}⚡{RESET}");
}

const THINKING_PREVIEW_LINES: usize = 20;

/// Render a structured thinking block with violet `│` gutter and bold `#` headers.
pub fn render_thinking_block(text: &str) {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let show = total.min(THINKING_PREVIEW_LINES);

    for line in &lines[..show] {
        if line.starts_with('#') {
            println!("{CONTENT_INDENT}{VIOLET}│{RESET} \x1b[1;90m{line}{RESET}");
        } else {
            println!("{CONTENT_INDENT}{VIOLET}│{RESET} {DIM}{line}{RESET}");
        }
    }
    if total > THINKING_PREVIEW_LINES {
        let hidden = total - THINKING_PREVIEW_LINES;
        println!("{CONTENT_INDENT}{VIOLET}│{RESET} {DIM}... +{hidden} more lines{RESET}");
    }
    println!();
}

/// Print the AGENT RESPONSE indicator.
pub fn print_response_banner() {
    println!("\n{EMERALD}●{RESET} {BOLD}Response{RESET}");
}

/// Content indent prefix for tool output and LLM responses.
const CONTENT_INDENT: &str = "  ";

/// Print tool output with a colored left border (indented under banner).
pub fn print_tool_output(tool_name: &str, output: &str) {
    if output.is_empty() || tool_name == "ShareReasoning" {
        return;
    }

    let border_color = match tool_name {
        "Read" => STEEL_BLUE,
        "List" | "WebFetch" => SKY_BLUE,
        "Grep" | "Glob" | "MemoryRead" => SILVER,
        "Bash" => ORANGE,
        "Write" | "Edit" | "MemoryWrite" => AMBER,
        "Delete" => CRIMSON,
        _ => DIM,
    };

    match tool_name {
        // ── Mutation tools: show diffs/output ─────────────────
        "Bash" => {
            // Parse exit code and collect output lines (skip markers)
            let mut exit_code: Option<i32> = None;
            let mut output_lines: Vec<&str> = Vec::new();
            for line in output.lines() {
                if let Some(code_str) = line.strip_prefix("Exit code: ") {
                    exit_code = code_str.trim().parse().ok();
                } else if line.starts_with("--- stdout ---") || line.starts_with("--- stderr ---") {
                    continue;
                } else {
                    output_lines.push(line);
                }
            }

            // Strip leading/trailing blank lines
            while output_lines.first().is_some_and(|l| l.trim().is_empty()) {
                output_lines.remove(0);
            }
            while output_lines.last().is_some_and(|l| l.trim().is_empty()) {
                output_lines.pop();
            }

            // Head + tail display with collapsed middle
            const HEAD: usize = 2;
            const TAIL: usize = 2;
            let total = output_lines.len();
            if total <= HEAD + TAIL {
                for line in &output_lines {
                    let dl = truncate_display_line(line);
                    println!("{CONTENT_INDENT}{border_color}│{RESET} {dl}");
                }
            } else {
                for line in &output_lines[..HEAD] {
                    let dl = truncate_display_line(line);
                    println!("{CONTENT_INDENT}{border_color}│{RESET} {dl}");
                }
                let collapsed = total - HEAD - TAIL;
                println!(
                    "{CONTENT_INDENT}{border_color}│{RESET} {DIM}\u{2026} +{collapsed} lines{RESET}"
                );
                for line in &output_lines[total - TAIL..] {
                    let dl = truncate_display_line(line);
                    println!("{CONTENT_INDENT}{border_color}│{RESET} {dl}");
                }
            }

            // Only show exit code if non-zero
            if let Some(code) = exit_code
                && code != 0
            {
                println!(
                    "{CONTENT_INDENT}{border_color}│{RESET} {CRIMSON}Exit code: {code}{RESET}"
                );
            }
        }
        "Write" | "Edit" | "MemoryWrite" => {
            for line in output.lines() {
                if line.starts_with('+') && !line.starts_with("+++") {
                    println!("{CONTENT_INDENT}{border_color}│{RESET} \x1b[32m{line}\x1b[0m");
                } else if line.starts_with('-') && !line.starts_with("---") {
                    println!("{CONTENT_INDENT}{border_color}│{RESET} \x1b[31m{line}\x1b[0m");
                } else if line.starts_with("@@") {
                    println!("{CONTENT_INDENT}{border_color}│{RESET} \x1b[36m{line}\x1b[0m");
                } else {
                    println!("{CONTENT_INDENT}{border_color}│{RESET} {DIM}{line}{RESET}");
                }
            }
        }
        "Delete" => {
            print_capped_output(output, border_color, 5);
        }

        // ── Read-only tools: compact summaries ──────────────
        "Read" => {
            // Like Code Puppy: just show a summary, not the content
            let line_count = output.lines().count();
            let char_count = output.len();
            println!(
                "{CONTENT_INDENT}{border_color}\u{2502}{RESET} {DIM}{line_count} lines ({char_count} chars){RESET}"
            );
        }
        "List" => {
            // Show a compact tree: top-level files + directory summaries
            let lines: Vec<&str> = output.lines().collect();
            let mut shown = 0;
            let mut dir_stats: std::collections::HashMap<String, (usize, usize)> =
                std::collections::HashMap::new(); // dir -> (files, subdirs)

            // First pass: collect directory stats
            for line in &lines {
                let trimmed = line.trim();
                let is_dir = trimmed.starts_with("d ");
                let path = if is_dir { &trimmed[2..] } else { trimmed };

                // Count depth by path separators
                let depth = path.matches('/').count();
                if depth == 0 {
                    continue; // top-level items shown directly
                }

                // Find the top-level parent directory
                let top_dir = path.split('/').next().unwrap_or("").to_string();
                let entry = dir_stats.entry(top_dir).or_insert((0, 0));
                if is_dir {
                    entry.1 += 1; // subdir
                } else {
                    entry.0 += 1; // file
                }
            }

            // Second pass: show top-level items and directory summaries
            let mut shown_dirs: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            for line in &lines {
                let trimmed = line.trim();
                let is_dir = trimmed.starts_with("d ");
                let path = if is_dir { &trimmed[2..] } else { trimmed };
                let depth = path.matches('/').count();

                if depth == 0 {
                    if is_dir {
                        // Top-level directory: show summary
                        if shown_dirs.insert(path.to_string()) {
                            let (files, subdirs) = dir_stats.get(path).copied().unwrap_or((0, 0));
                            let mut parts = Vec::new();
                            if files > 0 {
                                parts.push(format!(
                                    "{files} file{}",
                                    if files == 1 { "" } else { "s" }
                                ));
                            }
                            if subdirs > 0 {
                                parts.push(format!(
                                    "{subdirs} subdir{}",
                                    if subdirs == 1 { "" } else { "s" }
                                ));
                            }
                            let summary = if parts.is_empty() {
                                String::new()
                            } else {
                                format!(" ({})", parts.join(", "))
                            };
                            println!(
                                "{CONTENT_INDENT}{border_color}\u{2502}{RESET} {DIM}\u{1f4c1} {path}/{summary}{RESET}"
                            );
                            shown += 1;
                        }
                    } else {
                        // Top-level file
                        let display_line = if path.len() > 120 { &path[..120] } else { path };
                        println!(
                            "{CONTENT_INDENT}{border_color}\u{2502}{RESET} {DIM}{display_line}{RESET}"
                        );
                        shown += 1;
                    }
                }
            }

            let total_files = lines.iter().filter(|l| !l.trim().starts_with("d ")).count();
            let total_dirs = lines.iter().filter(|l| l.trim().starts_with("d ")).count();
            if shown > 0 {
                println!(
                    "{CONTENT_INDENT}{border_color}\u{2502}{RESET} {DIM}({total_files} files, {total_dirs} dirs total){RESET}"
                );
            }
        }
        "Grep" => {
            // Group matches by file, show count per file
            let lines: Vec<&str> = output.lines().collect();
            let mut by_file: std::collections::BTreeMap<String, usize> =
                std::collections::BTreeMap::new();
            for line in &lines {
                // Grep output format: "file:line:content"
                if let Some(colon_pos) = line.find(':') {
                    let file = &line[..colon_pos];
                    *by_file.entry(file.to_string()).or_insert(0) += 1;
                }
            }
            if by_file.is_empty() {
                println!("{CONTENT_INDENT}{border_color}\u{2502}{RESET} {DIM}No matches{RESET}");
            } else {
                for (file, count) in &by_file {
                    let word = if *count == 1 { "match" } else { "matches" };
                    println!(
                        "{CONTENT_INDENT}{border_color}\u{2502}{RESET} {DIM}\u{1f4c4} {file} ({count} {word}){RESET}"
                    );
                }
                let total: usize = by_file.values().sum();
                let file_word = if by_file.len() == 1 { "file" } else { "files" };
                println!(
                    "{CONTENT_INDENT}{border_color}\u{2502}{RESET} {DIM}Found {total} matches across {} {file_word}{RESET}",
                    by_file.len()
                );
            }
        }
        "Glob" => {
            let lines: Vec<&str> = output.lines().collect();
            let count = lines.len();
            let show = count.min(10);
            for line in &lines[..show] {
                let display_line = if line.len() > 120 { &line[..120] } else { line };
                println!(
                    "{CONTENT_INDENT}{border_color}\u{2502}{RESET} {DIM}{display_line}{RESET}"
                );
            }
            if count > 10 {
                println!(
                    "{CONTENT_INDENT}{border_color}\u{2502}{RESET} {DIM}... ({} more){RESET}",
                    count - 10
                );
            }
            println!("{CONTENT_INDENT}{border_color}\u{2502}{RESET} {DIM}({count} matches){RESET}");
        }
        "WebFetch" => {
            let char_count = output.len();
            let line_count = output.lines().count();
            // Show just the first line as a preview
            let preview = output.lines().next().unwrap_or("");
            let preview = if preview.len() > 80 {
                &preview[..80]
            } else {
                preview
            };
            println!("{CONTENT_INDENT}{border_color}\u{2502}{RESET} {DIM}{preview}...{RESET}");
            println!(
                "{CONTENT_INDENT}{border_color}\u{2502}{RESET} {DIM}({line_count} lines, {char_count} chars fetched){RESET}"
            );
        }
        "MemoryRead" => {
            // Show compact summary with a few lines
            let line_count = output.lines().count();
            print_capped_output(output, border_color, 8);
            if line_count > 8 {
                println!(
                    "{CONTENT_INDENT}{border_color}\u{2502}{RESET} {DIM}({line_count} lines total){RESET}"
                );
            }
        }

        // ── Default: show a few lines ─────────────────────
        _ => {
            print_capped_output(output, border_color, 10);
        }
    }
}

/// Truncate a display line to 256 chars.
fn truncate_display_line(line: &str) -> &str {
    if line.len() > 256 { &line[..256] } else { line }
}

/// Print output lines with a colored border, capped at `max_lines`.
fn print_capped_output(output: &str, border_color: &str, max_lines: usize) {
    let lines: Vec<&str> = output.lines().collect();
    let show = lines.len().min(max_lines);
    for line in &lines[..show] {
        let display_line = if line.len() > 120 { &line[..120] } else { line };
        println!("{CONTENT_INDENT}{border_color}│{RESET} {DIM}{display_line}{RESET}");
    }
    if lines.len() > max_lines {
        println!(
            "{CONTENT_INDENT}{border_color}│{RESET} {DIM}... ({} more lines){RESET}",
            lines.len() - max_lines
        );
    }
}

// ── JSON helpers ──────────────────────────────────────────────────

fn json_str_multi(v: &serde_json::Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(s) = v.get(*key).and_then(|v| v.as_str()) {
            return s.to_string();
        }
    }
    "?".to_string()
}

fn json_str_or_multi(v: &serde_json::Value, keys: &[&str], default: &str) -> String {
    for key in keys {
        if let Some(s) = v.get(*key).and_then(|v| v.as_str()) {
            return s.to_string();
        }
    }
    default.to_string()
}

fn extract_edit_path(args: &serde_json::Value) -> String {
    if let Some(payload) = args.get("payload")
        && let Some(path) = payload
            .get("file_path")
            .or(payload.get("path"))
            .and_then(|v| v.as_str())
    {
        return path.to_string();
    }
    json_str_multi(args, &["file_path", "path"])
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}\u{2026}", &s[..max.saturating_sub(1)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_info_read() {
        let (color, label, detail) = tool_info("Read", r#"{"file_path": "src/main.rs"}"#);
        assert_eq!(color, STEEL_BLUE);
        assert_eq!(label, "Read");
        assert_eq!(detail, "src/main.rs");
    }

    #[test]
    fn test_tool_info_read_alt_key() {
        let (_, _, detail) = tool_info("Read", r#"{"path": "DESIGN.md"}"#);
        assert_eq!(detail, "DESIGN.md");
    }

    #[test]
    fn test_tool_info_bash() {
        let (color, label, detail) = tool_info("Bash", r#"{"command": "cargo build"}"#);
        assert_eq!(color, ORANGE);
        assert_eq!(label, "Shell");
        assert_eq!(detail, "$ cargo build");
    }

    #[test]
    fn test_tool_info_grep() {
        let (color, label, detail) =
            tool_info("Grep", r#"{"search_string": "TODO", "directory": "src/"}"#);
        assert_eq!(color, SILVER);
        assert_eq!(label, "Search");
        assert_eq!(detail, "src/ for 'TODO'");
    }

    #[test]
    fn test_tool_info_edit_nested_payload() {
        let (color, label, detail) = tool_info(
            "Edit",
            r#"{"payload": {"file_path": "src/lib.rs", "content": "hello"}}"#,
        );
        assert_eq!(color, AMBER);
        assert_eq!(label, "Edit");
        assert_eq!(detail, "src/lib.rs (create)");
    }

    #[test]
    fn test_tool_info_edit_replace() {
        let (_, _, detail) = tool_info(
            "Edit",
            r#"{"payload": {"file_path": "x.rs", "replacements": []}}"#,
        );
        assert_eq!(detail, "x.rs (replace)");
    }

    #[test]
    fn test_truncate() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("a long string here", 10), "a long st\u{2026}");
    }
}
