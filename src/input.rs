//! REPL input helpers: completions, hints, and highlighting.
//!
//! Implements `rustyline::Helper` with context-aware completions
//! for slash commands (`/model`, `/help`…) and file references (`@path`).

use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Context, Helper};
use std::borrow::Cow;
use std::path::{Path, PathBuf};

/// All known slash commands with short descriptions.
const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/copy", "Copy last response or code block"),
    ("/cost", "Show token usage for this session"),
    (
        "/diff",
        "Show git diff, review changes, or generate commit msg",
    ),
    ("/compact", "Summarize conversation to reclaim context"),
    ("/help", "Command palette"),
    ("/memory", "View/save project & global memory"),
    ("/model", "Pick a model interactively"),
    ("/paste", "Show clipboard contents"),
    ("/provider", "Switch LLM provider"),
    ("/proxy", "Set HTTP proxy"),
    ("/sessions", "List/resume/delete sessions"),
    ("/quit", "Exit Koda"),
];

/// The combined helper wired into `rustyline::Editor`.
pub struct KodaHelper {
    /// Project root for resolving `@path` references.
    project_root: PathBuf,
    /// Known model names for `/model` completion.
    pub model_names: Vec<String>,
}

impl KodaHelper {
    pub fn new(project_root: PathBuf) -> Self {
        Self {
            project_root,
            model_names: Vec::new(),
        }
    }
}

// ── Completer ───────────────────────────────────────────────────

impl Completer for KodaHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let text = &line[..pos];

        // ── Slash commands ─────────────────────────────────────
        let trimmed = text.trim_start();
        if trimmed.starts_with('/') && !trimmed.contains(' ') {
            return Ok(complete_slash_command(trimmed, pos));
        }

        // ── /model <name> completion ──────────────────────────
        if let Some(partial) = trimmed.strip_prefix("/model ") {
            let matches = complete_model_name(partial, &self.model_names);
            let start = pos - partial.len();
            return Ok((start, matches));
        }

        // ── @file path completion ──────────────────────────────
        if let Some(at_pos) = text.rfind('@') {
            // Only trigger if @ is at start or preceded by whitespace
            if at_pos == 0 || text.as_bytes()[at_pos - 1] == b' ' {
                let partial = &text[at_pos + 1..];
                let matches = complete_file_path(partial, &self.project_root);
                return Ok((at_pos + 1, matches));
            }
        }

        Ok((pos, Vec::new()))
    }
}

fn complete_slash_command(partial: &str, pos: usize) -> (usize, Vec<Pair>) {
    let start = pos - partial.len();
    let matches: Vec<Pair> = SLASH_COMMANDS
        .iter()
        .filter(|(cmd, _)| cmd.starts_with(partial))
        .map(|(cmd, desc)| Pair {
            display: format!("{cmd}  {desc}"),
            replacement: cmd.to_string(),
        })
        .collect();
    (start, matches)
}

fn complete_model_name(partial: &str, model_names: &[String]) -> Vec<Pair> {
    let lower = partial.to_lowercase();
    model_names
        .iter()
        .filter(|name| name.to_lowercase().starts_with(&lower))
        .map(|name| Pair {
            display: name.clone(),
            replacement: name.clone(),
        })
        .collect()
}

fn complete_file_path(partial: &str, project_root: &Path) -> Vec<Pair> {
    let search_dir = if partial.is_empty() || !partial.contains('/') {
        project_root.to_path_buf()
    } else {
        // Split into directory part and filename prefix
        let dir_part = &partial[..partial.rfind('/').unwrap_or(0) + 1];
        project_root.join(dir_part)
    };

    let prefix = if partial.contains('/') {
        partial.rsplit('/').next().unwrap_or("")
    } else {
        partial
    };

    let dir_prefix = if partial.contains('/') {
        &partial[..partial.rfind('/').unwrap_or(0) + 1]
    } else {
        ""
    };

    let Ok(entries) = std::fs::read_dir(&search_dir) else {
        return Vec::new();
    };

    let mut results: Vec<Pair> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            // Skip hidden files unless user typed a dot
            if name.starts_with('.') && !prefix.starts_with('.') {
                return None;
            }
            if !name.to_lowercase().starts_with(&prefix.to_lowercase()) {
                return None;
            }
            let is_dir = entry.file_type().ok()?.is_dir();
            // Skip common noise directories
            if is_dir
                && matches!(
                    name.as_str(),
                    "target" | "node_modules" | ".git" | "__pycache__"
                )
            {
                return None;
            }
            let display = if is_dir {
                format!("{name}/")
            } else {
                name.clone()
            };
            let replacement = if is_dir {
                format!("{dir_prefix}{name}/")
            } else {
                format!("{dir_prefix}{name}")
            };
            Some(Pair {
                display,
                replacement,
            })
        })
        .collect();

    results.sort_by(|a, b| a.display.cmp(&b.display));
    results
}

// ── Hinter ─────────────────────────────────────────────────────

impl Hinter for KodaHelper {
    type Hint = String;

    fn hint(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> Option<String> {
        // Only hint when cursor is at end of line
        if pos != line.len() {
            return None;
        }

        let trimmed = line.trim_start();

        // Hint slash commands
        if trimmed.starts_with('/') && !trimmed.contains(' ') {
            for (cmd, _) in SLASH_COMMANDS {
                if cmd.starts_with(trimmed) && *cmd != trimmed {
                    return Some(cmd[trimmed.len()..].to_string());
                }
            }
        }

        // Hint model names after "/model "
        if let Some(partial) = trimmed.strip_prefix("/model ") {
            let lower = partial.to_lowercase();
            for name in &self.model_names {
                if name.to_lowercase().starts_with(&lower) && name != partial {
                    return Some(name[partial.len()..].to_string());
                }
            }
        }

        None
    }
}

// ── Highlighter ────────────────────────────────────────────────

impl Highlighter for KodaHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        let trimmed = line.trim_start();

        // Highlight slash commands in cyan
        if trimmed.starts_with('/') {
            return Cow::Owned(format!("\x1b[36m{line}\x1b[0m"));
        }

        // Highlight @references in cyan
        if line.contains('@') {
            let highlighted = highlight_at_refs(line);
            return Cow::Owned(highlighted);
        }

        Cow::Borrowed(line)
    }

    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        // Hints shown in dim gray
        Cow::Owned(format!("\x1b[90m{hint}\x1b[0m"))
    }

    fn highlight_char(
        &self,
        _line: &str,
        _pos: usize,
        _forced: rustyline::highlight::CmdKind,
    ) -> bool {
        // Return true to enable real-time highlighting
        true
    }
}

/// Highlight `@path` tokens in cyan within a line.
fn highlight_at_refs(line: &str) -> String {
    let mut result = String::with_capacity(line.len() + 32);
    let mut chars = line.char_indices().peekable();

    while let Some((i, c)) = chars.next() {
        if c == '@' && (i == 0 || line.as_bytes()[i - 1] == b' ') {
            // Found an @ reference — color until next whitespace
            result.push_str("\x1b[36m@");
            while let Some(&(_, next_c)) = chars.peek() {
                if next_c.is_whitespace() {
                    break;
                }
                result.push(next_c);
                chars.next();
            }
            result.push_str("\x1b[0m");
        } else {
            result.push(c);
        }
    }
    result
}

impl Validator for KodaHelper {}
impl Helper for KodaHelper {}

// ── @file pre-processor ────────────────────────────────────────

/// Result of processing user input for `@path` references.
#[derive(Debug)]
pub struct ProcessedInput {
    /// The cleaned prompt text (with @references stripped).
    pub prompt: String,
    /// File contents to inject as additional context.
    pub context_files: Vec<FileContext>,
}

/// A file's contents loaded from an `@path` reference.
#[derive(Debug)]
pub struct FileContext {
    pub path: String,
    pub content: String,
}

/// Scan input for `@path` tokens, read the files, and return cleaned prompt
/// plus file contents for context injection.
pub fn process_input(input: &str, project_root: &Path) -> ProcessedInput {
    let mut prompt_parts = Vec::new();
    let mut context_files = Vec::new();

    for token in input.split_whitespace() {
        if let Some(raw_path) = token.strip_prefix('@') {
            if raw_path.is_empty() {
                prompt_parts.push(token.to_string());
                continue;
            }

            let full_path = project_root.join(raw_path);
            match std::fs::read_to_string(&full_path) {
                Ok(content) => {
                    context_files.push(FileContext {
                        path: raw_path.to_string(),
                        content,
                    });
                    // Don't add the @token to prompt — it's been consumed
                }
                Err(_) => {
                    // File doesn't exist or unreadable — leave as-is in prompt
                    eprintln!("  \x1b[33m⚠ Could not read: {raw_path}\x1b[0m");
                    prompt_parts.push(token.to_string());
                }
            }
        } else {
            prompt_parts.push(token.to_string());
        }
    }

    let prompt = prompt_parts.join(" ");

    // If only @refs were provided with no other text, add a default prompt
    let prompt = if prompt.trim().is_empty() && !context_files.is_empty() {
        "Describe and explain the attached files.".to_string()
    } else {
        prompt
    };

    ProcessedInput {
        prompt,
        context_files,
    }
}

/// Format file contexts into a string suitable for injection into the user
/// message sent to the LLM.
pub fn format_context_files(files: &[FileContext]) -> Option<String> {
    if files.is_empty() {
        return None;
    }

    let mut parts = Vec::new();
    for f in files {
        parts.push(format!(
            "<file path=\"{}\">{}</file>",
            f.path,
            // Cap at ~40k chars (~10k tokens) per file
            if f.content.len() > 40_000 {
                // Snap to char boundary to avoid panic on multi-byte chars
                let mut end = 40_000;
                while !f.content.is_char_boundary(end) {
                    end -= 1;
                }
                format!(
                    "{}\n\n[truncated — {} bytes total]",
                    &f.content[..end],
                    f.content.len()
                )
            } else {
                f.content.clone()
            }
        ));
    }

    Some(parts.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_slash_command_completion() {
        let (start, matches) = complete_slash_command("/mo", 3);
        assert_eq!(start, 0);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].replacement, "/model");
    }

    #[test]
    fn test_slash_command_all() {
        let (_, matches) = complete_slash_command("/", 1);
        assert_eq!(matches.len(), SLASH_COMMANDS.len());
    }

    #[test]
    fn test_model_name_completion() {
        let models = vec!["gpt-4o".into(), "gpt-4o-mini".into(), "claude-3".into()];
        let matches = complete_model_name("gpt", &models);
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn test_model_name_case_insensitive() {
        let models = vec!["GPT-4o".into()];
        let matches = complete_model_name("gpt", &models);
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn test_file_path_completion() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("hello.rs"), "fn main() {}").unwrap();
        fs::write(dir.path().join("world.rs"), "fn world() {}").unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();

        let matches = complete_file_path("h", dir.path());
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].replacement, "hello.rs");

        // Empty partial shows all non-hidden entries
        let all = complete_file_path("", dir.path());
        assert_eq!(all.len(), 3); // hello.rs, world.rs, src/
    }

    #[test]
    fn test_file_path_skips_hidden() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(".hidden"), "").unwrap();
        fs::write(dir.path().join("visible.rs"), "").unwrap();

        let matches = complete_file_path("", dir.path());
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].replacement, "visible.rs");

        // Dot prefix shows hidden files
        let hidden = complete_file_path(".", dir.path());
        assert_eq!(hidden.len(), 1);
    }

    #[test]
    fn test_process_input_with_file_ref() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("test.rs"), "fn test() {}").unwrap();

        let result = process_input("explain @test.rs", dir.path());
        assert_eq!(result.prompt, "explain");
        assert_eq!(result.context_files.len(), 1);
        assert_eq!(result.context_files[0].path, "test.rs");
        assert_eq!(result.context_files[0].content, "fn test() {}");
    }

    #[test]
    fn test_process_input_no_refs() {
        let dir = TempDir::new().unwrap();
        let result = process_input("just a normal question", dir.path());
        assert_eq!(result.prompt, "just a normal question");
        assert!(result.context_files.is_empty());
    }

    #[test]
    fn test_process_input_only_ref() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("code.py"), "print('hi')").unwrap();

        let result = process_input("@code.py", dir.path());
        assert_eq!(result.prompt, "Describe and explain the attached files.");
        assert_eq!(result.context_files.len(), 1);
    }

    #[test]
    fn test_process_input_missing_file() {
        let dir = TempDir::new().unwrap();
        let result = process_input("explain @nonexistent.rs", dir.path());
        // Missing file stays in prompt as-is
        assert!(result.prompt.contains("@nonexistent.rs"));
        assert!(result.context_files.is_empty());
    }

    #[test]
    fn test_format_context_files_empty() {
        assert!(format_context_files(&[]).is_none());
    }

    #[test]
    fn test_format_context_files() {
        let files = vec![FileContext {
            path: "main.rs".into(),
            content: "fn main() {}".into(),
        }];
        let result = format_context_files(&files).unwrap();
        assert!(result.contains("<file path=\"main.rs\">"));
        assert!(result.contains("fn main() {}"));
        assert!(result.contains("</file>"));
    }

    #[test]
    fn test_highlight_at_refs() {
        let result = highlight_at_refs("explain @src/main.rs please");
        assert!(result.contains("\x1b[36m@src/main.rs\x1b[0m"));
        assert!(result.contains("explain "));
        assert!(result.contains(" please"));
    }

    #[test]
    fn test_highlight_at_refs_no_refs() {
        let result = highlight_at_refs("no refs here");
        assert_eq!(result, "no refs here");
    }

    #[test]
    fn test_highlight_at_refs_email_ignored() {
        // @ in middle of word (like email) should not be highlighted
        let result = highlight_at_refs("user@email.com");
        assert!(!result.contains("\x1b[36m"));
    }
}
