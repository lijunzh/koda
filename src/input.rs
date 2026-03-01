//! REPL input helpers: completions, hints, and highlighting.
//!
//! Implements `rustyline::Helper` with context-aware completions
//! for slash commands (`/model`, `/help`…) and file references (`@path`).

use crate::providers::ImageData;
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
    /// Base64-encoded images from @image references.
    pub images: Vec<ImageData>,
}

/// A file's contents loaded from an `@path` reference.
#[derive(Debug)]
pub struct FileContext {
    pub path: String,
    pub content: String,
}

/// Image file extensions we recognize for multi-modal input.
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp"];

/// Detect if a file path refers to an image by extension.
fn is_image_file(path: &str) -> bool {
    let lower = path.to_lowercase();
    IMAGE_EXTENSIONS.iter().any(|ext| lower.ends_with(ext))
}

/// Determine MIME type from file extension.
fn mime_type_for(path: &str) -> &'static str {
    let lower = path.to_lowercase();
    if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".bmp") {
        "image/bmp"
    } else {
        "application/octet-stream"
    }
}

/// Strip surrounding quotes from a token (terminals often quote dragged paths).
fn strip_quotes(s: &str) -> &str {
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Check if a token looks like a bare file path (absolute, ~/, or ./ prefixed).
fn looks_like_file_path(token: &str) -> bool {
    let cleaned = strip_quotes(token);
    cleaned.starts_with('/')
        || cleaned.starts_with("~/")
        || cleaned.starts_with("./")
        || cleaned.starts_with("..")
}

/// Try to load an image file, returning the ImageData if successful.
fn try_load_image(path: &Path, display_path: &str) -> Option<ImageData> {
    match std::fs::read(path) {
        Ok(bytes) => {
            use base64::Engine;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            let media_type = mime_type_for(display_path).to_string();
            Some(ImageData {
                media_type,
                base64: b64,
            })
        }
        Err(_) => {
            eprintln!("  \x1b[33m\u{26a0} Could not read image: {display_path}\x1b[0m");
            None
        }
    }
}

/// Resolve a bare path token to an absolute path, expanding ~ if needed.
fn resolve_bare_path(token: &str) -> Option<PathBuf> {
    let cleaned = strip_quotes(token);
    if cleaned.starts_with("~/") {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .ok()?;
        Some(PathBuf::from(home).join(&cleaned[2..]))
    } else {
        let p = PathBuf::from(cleaned);
        if p.is_absolute() {
            Some(p)
        } else {
            // Relative paths like ./foo or ../foo — resolve from cwd
            std::env::current_dir().ok().map(|cwd| cwd.join(cleaned))
        }
    }
}

/// Scan input for `@path` tokens and bare image paths (drag-and-drop),
/// read the files, and return cleaned prompt plus file contents and images.
pub fn process_input(input: &str, project_root: &Path) -> ProcessedInput {
    let mut prompt_parts = Vec::new();
    let mut context_files = Vec::new();
    let mut images = Vec::new();

    for token in input.split_whitespace() {
        // ── @path references (explicit) ───────────────────────
        if let Some(raw_path) = token.strip_prefix('@') {
            if raw_path.is_empty() {
                prompt_parts.push(token.to_string());
                continue;
            }

            let full_path = project_root.join(raw_path);

            // Image files → base64 encode for multi-modal
            if is_image_file(raw_path) {
                if let Some(img) = try_load_image(&full_path, raw_path) {
                    images.push(img);
                } else {
                    prompt_parts.push(token.to_string());
                }
                continue;
            }

            // Text files → read as string context
            match std::fs::read_to_string(&full_path) {
                Ok(content) => {
                    context_files.push(FileContext {
                        path: raw_path.to_string(),
                        content,
                    });
                }
                Err(_) => {
                    eprintln!("  \x1b[33m\u{26a0} Could not read: {raw_path}\x1b[0m");
                    prompt_parts.push(token.to_string());
                }
            }
            continue;
        }

        // ── Bare image paths (drag-and-drop) ──────────────────
        // Detect absolute/relative paths to image files pasted directly
        let unquoted = strip_quotes(token);
        if looks_like_file_path(token) && is_image_file(unquoted) {
            if let Some(resolved) = resolve_bare_path(token) {
                if resolved.exists() {
                    let display = resolved.display().to_string();
                    if let Some(img) = try_load_image(&resolved, &display) {
                        images.push(img);
                        continue;
                    }
                }
            }
        }

        prompt_parts.push(token.to_string());
    }

    let prompt = prompt_parts.join(" ");

    // If only @refs were provided with no other text, add a default prompt
    let prompt = if prompt.trim().is_empty() && (!context_files.is_empty() || !images.is_empty()) {
        if !images.is_empty() && context_files.is_empty() {
            "Describe and analyze this image.".to_string()
        } else {
            "Describe and explain the attached files.".to_string()
        }
    } else {
        prompt
    };

    ProcessedInput {
        prompt,
        context_files,
        images,
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

    #[test]
    fn test_is_image_file() {
        assert!(is_image_file("photo.png"));
        assert!(is_image_file("photo.PNG"));
        assert!(is_image_file("photo.jpg"));
        assert!(is_image_file("photo.jpeg"));
        assert!(is_image_file("photo.gif"));
        assert!(is_image_file("photo.webp"));
        assert!(is_image_file("photo.bmp"));
        assert!(!is_image_file("code.rs"));
        assert!(!is_image_file("data.json"));
        assert!(!is_image_file("readme.md"));
    }

    #[test]
    fn test_mime_type_for() {
        assert_eq!(mime_type_for("x.png"), "image/png");
        assert_eq!(mime_type_for("x.jpg"), "image/jpeg");
        assert_eq!(mime_type_for("x.jpeg"), "image/jpeg");
        assert_eq!(mime_type_for("x.gif"), "image/gif");
        assert_eq!(mime_type_for("x.webp"), "image/webp");
        assert_eq!(mime_type_for("x.bmp"), "image/bmp");
    }

    #[test]
    fn test_process_input_image_ref() {
        let dir = TempDir::new().unwrap();
        // Create a tiny 1x1 PNG (valid minimal)
        let png_bytes: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        fs::write(dir.path().join("screenshot.png"), &png_bytes).unwrap();

        let result = process_input("what is this @screenshot.png", dir.path());
        assert_eq!(result.prompt, "what is this");
        assert!(result.context_files.is_empty());
        assert_eq!(result.images.len(), 1);
        assert_eq!(result.images[0].media_type, "image/png");
        assert!(!result.images[0].base64.is_empty());
    }

    #[test]
    fn test_process_input_image_only_default_prompt() {
        let dir = TempDir::new().unwrap();
        let png_bytes: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        fs::write(dir.path().join("ui.png"), &png_bytes).unwrap();

        let result = process_input("@ui.png", dir.path());
        assert_eq!(result.prompt, "Describe and analyze this image.");
        assert_eq!(result.images.len(), 1);
    }

    #[test]
    fn test_process_input_mixed_image_and_file() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("code.rs"), "fn main() {}").unwrap();
        let png_bytes: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        fs::write(dir.path().join("error.png"), &png_bytes).unwrap();

        let result = process_input("fix this @code.rs @error.png", dir.path());
        assert_eq!(result.prompt, "fix this");
        assert_eq!(result.context_files.len(), 1);
        assert_eq!(result.images.len(), 1);
    }

    #[test]
    fn test_strip_quotes() {
        assert_eq!(strip_quotes("'/path/to/file.png'"), "/path/to/file.png");
        assert_eq!(strip_quotes("\"/path/to/file.png\""), "/path/to/file.png");
        assert_eq!(strip_quotes("/no/quotes.png"), "/no/quotes.png");
        assert_eq!(strip_quotes("'mismatched"), "'mismatched");
    }

    #[test]
    fn test_looks_like_file_path() {
        assert!(looks_like_file_path("/absolute/path.png"));
        assert!(looks_like_file_path("~/Desktop/img.jpg"));
        assert!(looks_like_file_path("./relative/img.png"));
        assert!(looks_like_file_path("../parent/img.png"));
        assert!(looks_like_file_path("'/quoted/path.png'"));
        assert!(!looks_like_file_path("just-a-word"));
        assert!(!looks_like_file_path("relative.png"));
    }

    #[test]
    fn test_drag_and_drop_absolute_path() {
        let dir = TempDir::new().unwrap();
        let png_bytes: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let img_path = dir.path().join("screenshot.png");
        fs::write(&img_path, &png_bytes).unwrap();

        let input = format!("what is this {}", img_path.display());
        let result = process_input(&input, dir.path());
        assert_eq!(result.prompt, "what is this");
        assert_eq!(result.images.len(), 1);
        assert_eq!(result.images[0].media_type, "image/png");
    }

    #[test]
    fn test_drag_and_drop_quoted_path() {
        let dir = TempDir::new().unwrap();
        let png_bytes: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let img_path = dir.path().join("screenshot.png");
        fs::write(&img_path, &png_bytes).unwrap();

        // Single-quoted (some terminals do this)
        let input = format!("explain '{}'", img_path.display());
        let result = process_input(&input, dir.path());
        assert_eq!(result.prompt, "explain");
        assert_eq!(result.images.len(), 1);
    }

    #[test]
    fn test_drag_and_drop_nonexistent_stays_in_prompt() {
        let dir = TempDir::new().unwrap();
        let input = "/tmp/nonexistent_image_12345.png what is this";
        let result = process_input(input, dir.path());
        // Non-existent file stays as text in prompt
        assert!(result.prompt.contains("/tmp/nonexistent_image_12345.png"));
        assert!(result.images.is_empty());
    }

    #[test]
    fn test_non_image_absolute_path_stays_in_prompt() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("data.json"), "{}").unwrap();
        let input = format!("read {}", dir.path().join("data.json").display());
        let result = process_input(&input, dir.path());
        // Non-image absolute paths are NOT auto-consumed (only images)
        assert!(result.prompt.contains("data.json"));
        assert!(result.images.is_empty());
    }
}
