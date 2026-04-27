//! Input processing — @file references and image loading.
//!
//! Processes user input for `@path` references, loading file contents
//! as additional context and images for multi-modal prompts.

use koda_core::providers::ImageData;
use std::path::{Path, PathBuf};

// ── @file pre-processor ────────────────────────────────────────

/// Content pasted via clipboard (bracketed paste).
#[derive(Debug, Clone)]
pub struct PasteBlock {
    /// The raw pasted text.
    pub content: String,
    /// Character count.
    pub char_count: usize,
}

/// Result of processing user input for `@path` references.
#[derive(Debug)]
pub struct ProcessedInput {
    /// The cleaned prompt text (with @references stripped).
    pub prompt: String,
    /// File contents to inject as additional context.
    pub context_files: Vec<FileContext>,
    /// Base64-encoded images from @image references.
    pub images: Vec<ImageData>,
    /// Pasted content blocks (from bracketed paste).
    pub paste_blocks: Vec<PasteBlock>,
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
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Remove shell escape backslashes from a path (POSIX only).
///
/// Handles paths like `/Users/foo/Screenshot\ 2026-04-09\ at\ 4.37.01\u{202f}PM.png`
/// by stripping single backslashes used as escape chars while preserving
/// literal backslashes (`\\` → `\`).
///
/// Borrowed from Gemini CLI's `unescapePath` and Claude Code's
/// `stripBackslashEscapes`.
fn unescape_path(path: &str) -> String {
    if cfg!(windows) {
        // On Windows, backslashes are path separators — don't strip them.
        return path.to_string();
    }
    let mut result = String::with_capacity(path.len());
    let mut chars = path.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            // Backslash: consume the next char literally (space, parens, etc.)
            if let Some(next) = chars.next() {
                result.push(next);
            } else {
                // Trailing backslash — keep it
                result.push(c);
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Shell-aware tokenizer that keeps backslash-escaped and quoted paths intact.
///
/// Unlike `split_whitespace()`, this handles:
/// - Backslash-escaped spaces: `path\ with\ spaces` → single token
/// - Quoted strings: `"path with spaces"` or `'path with spaces'` → single token
/// - Regular whitespace splitting for everything else
///
/// Inspired by Gemini CLI's regex-based `@path` parsing and Claude Code's
/// `stripBackslashEscapes`.
fn tokenize_shell_aware(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars().peekable();

    while let Some(&c) = chars.peek() {
        match c {
            // ── Whitespace: flush current token ──
            ' ' | '\t' | '\n' | '\r' => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                chars.next();
            }
            // ── Backslash: escape the next character ──
            '\\' => {
                chars.next(); // consume '\\'
                // Keep the backslash + next char as-is in the token
                // (unescape_path strips them later for path tokens)
                current.push('\\');
                if let Some(&next) = chars.peek() {
                    current.push(next);
                    chars.next();
                }
            }
            // ── Quoted string: consume until matching close quote ──
            '"' | '\'' => {
                let quote = c;
                current.push(quote);
                chars.next(); // consume opening quote
                while let Some(&inner) = chars.peek() {
                    current.push(inner);
                    chars.next();
                    if inner == quote {
                        break;
                    }
                }
            }
            // ── Regular character ──
            _ => {
                current.push(c);
                chars.next();
            }
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Check if a token looks like a bare file path (absolute, ~/, or ./ prefixed).
fn looks_like_file_path(token: &str) -> bool {
    let cleaned = strip_quotes(token);
    // Also try with backslash escapes removed
    let unescaped = unescape_path(cleaned);
    let check = |s: &str| -> bool {
        s.starts_with('/')
            || s.starts_with("~/")
            || s.starts_with("./")
            || s.starts_with("..")
            // Windows absolute paths: C:\\ or D:\\
            || (s.len() >= 3
                && s.as_bytes()[0].is_ascii_alphabetic()
                && s.as_bytes()[1] == b':'
                && (s.as_bytes()[2] == b'\\' || s.as_bytes()[2] == b'/'))
    };
    check(cleaned) || check(&unescaped)
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

/// Resolve a bare path token to an absolute path, expanding ~ and
/// stripping shell escapes.
fn resolve_bare_path(token: &str) -> Option<PathBuf> {
    let cleaned = strip_quotes(token);
    let unescaped = unescape_path(cleaned);
    let path_str = unescaped.as_str();
    if let Some(rest) = path_str.strip_prefix("~/") {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .ok()?;
        Some(PathBuf::from(home).join(rest))
    } else {
        let p = PathBuf::from(path_str);
        if p.is_absolute() {
            Some(p)
        } else {
            // Relative paths like ./foo or ../foo — resolve from cwd
            std::env::current_dir().ok().map(|cwd| cwd.join(path_str))
        }
    }
}

/// Scan input for `@path` tokens and bare image paths (drag-and-drop),
/// read the files, and return cleaned prompt plus file contents and images.
pub fn process_input(input: &str, project_root: &Path) -> ProcessedInput {
    let mut prompt_parts = Vec::new();
    let mut context_files = Vec::new();
    let mut images = Vec::new();

    for token in tokenize_shell_aware(input) {
        // ── @path references (explicit) ───────────────────────
        if let Some(raw_path) = token.strip_prefix('@') {
            if raw_path.is_empty() {
                prompt_parts.push(token.to_string());
                continue;
            }

            // Strip quotes and shell escapes for the actual path
            let raw_path = strip_quotes(raw_path);
            let clean_path = unescape_path(raw_path);

            // Security: reject paths that escape the project root
            let full_path = match koda_core::tools::safe_resolve_path(project_root, &clean_path) {
                Ok(p) => p,
                Err(_) => {
                    tracing::warn!("@file path escapes project root: {clean_path}");
                    prompt_parts.push(token.to_string());
                    continue;
                }
            };

            // Image files → base64 encode for multi-modal
            if is_image_file(&clean_path) {
                if let Some(img) = try_load_image(&full_path, &clean_path) {
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
                        path: clean_path,
                        content,
                    });
                }
                Err(_) => {
                    eprintln!("  \x1b[33m\u{26a0} Could not read: {clean_path}\x1b[0m");
                    prompt_parts.push(token.to_string());
                }
            }
            continue;
        }

        // ── Bare image paths (drag-and-drop) ──────────────────
        // Detect absolute/relative paths to image files pasted directly.
        // After shell-aware tokenization, paths with escaped spaces like
        // `/Users/foo/Screenshot\ 2026-04-09.png` arrive as one token.
        let unescaped = unescape_path(strip_quotes(&token));
        if looks_like_file_path(&token)
            && is_image_file(&unescaped)
            && let Some(resolved) = resolve_bare_path(&token)
            && resolved.exists()
        {
            let display = resolved.display().to_string();
            if let Some(img) = try_load_image(&resolved, &display) {
                images.push(img);
                continue;
            }
        }

        prompt_parts.push(token);
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
        paste_blocks: Vec::new(),
    }
}

/// Format file contexts into a string suitable for injection into the user
/// message sent to the LLM.
///
/// Returns `None` when `files` is empty. Each file is wrapped in an XML
/// `<file path="...">` tag and joined with a double newline.
/// Contents exceeding 40 000 bytes are truncated with a note.
///
/// # Examples
///
/// ```ignore
/// // pub(crate): input is not importable from a doc-test binary; illustrative only.
/// use koda_cli::input::{FileContext, format_context_files};
///
/// // Empty list → None
/// assert!(format_context_files(&[]).is_none());
///
/// // Single file → XML-tagged content
/// let files = vec![FileContext {
///     path: "src/main.rs".into(),
///     content: "fn main() {}".into(),
/// }];
/// let out = format_context_files(&files).unwrap();
/// assert!(out.contains("<file path=\"src/main.rs\">"));
/// assert!(out.contains("fn main() {}"));
/// assert!(out.contains("</file>"));
///
/// // Multiple files are joined with a blank line
/// let two = vec![
///     FileContext { path: "a.rs".into(), content: "// a".into() },
///     FileContext { path: "b.rs".into(), content: "// b".into() },
/// ];
/// let out2 = format_context_files(&two).unwrap();
/// assert!(out2.contains("a.rs"));
/// assert!(out2.contains("b.rs"));
/// ```
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

/// Pastes shorter than this go inline in the textarea; longer ones become PasteBlocks.
pub const PASTE_BLOCK_THRESHOLD: usize = 200;

/// Max chars per paste block (~40k chars, matching file truncation policy).
const PASTE_BLOCK_MAX_CHARS: usize = 40_000;

/// Format paste blocks into semantically tagged XML for the LLM.
///
/// Each block is wrapped in `<reference type="pasted" chars="N">...</reference>`
/// so the model can distinguish pasted reference material from direct instructions.
///
/// Returns `None` when `blocks` is empty. Content exceeding 40 000 chars is
/// truncated with a note.
///
/// # Examples
///
/// ```ignore
/// // pub(crate): input is not importable from a doc-test binary; illustrative only.
/// use koda_cli::input::{PasteBlock, format_paste_blocks};
///
/// // Empty list → None
/// assert!(format_paste_blocks(&[]).is_none());
///
/// // Single block → tagged output
/// let blocks = vec![PasteBlock {
///     content: "SELECT 1;".into(),
///     char_count: 9,
/// }];
/// let out = format_paste_blocks(&blocks).unwrap();
/// assert!(out.contains("<reference type=\"pasted\" chars=\"9\">"));
/// assert!(out.contains("SELECT 1;"));
/// assert!(out.contains("</reference>"));
/// ```
pub fn format_paste_blocks(blocks: &[PasteBlock]) -> Option<String> {
    if blocks.is_empty() {
        return None;
    }

    let parts: Vec<String> = blocks
        .iter()
        .map(|b| {
            let content = if b.content.len() > PASTE_BLOCK_MAX_CHARS {
                let mut end = PASTE_BLOCK_MAX_CHARS;
                while !b.content.is_char_boundary(end) {
                    end -= 1;
                }
                format!(
                    "{}\n\n[truncated — {} chars total]",
                    &b.content[..end],
                    b.char_count
                )
            } else {
                b.content.clone()
            };
            format!(
                "<reference type=\"pasted\" chars=\"{}\">{}</reference>",
                b.char_count, content
            )
        })
        .collect();

    Some(parts.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

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
        fs::write(dir.path().join("screenshot.png"), png_bytes).unwrap();

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
        fs::write(dir.path().join("ui.png"), png_bytes).unwrap();

        let result = process_input("@ui.png", dir.path());
        assert_eq!(result.prompt, "Describe and analyze this image.");
        assert_eq!(result.images.len(), 1);
    }

    #[test]
    fn test_process_input_mixed_image_and_file() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("code.rs"), "fn main() {}").unwrap();
        let png_bytes: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        fs::write(dir.path().join("error.png"), png_bytes).unwrap();

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
        assert_eq!(strip_quotes("'"), "'");
        assert_eq!(strip_quotes("\""), "\"");
    }

    #[test]
    fn test_looks_like_file_path() {
        assert!(looks_like_file_path("/absolute/path.png"));
        assert!(looks_like_file_path("~/Desktop/img.jpg"));
        assert!(looks_like_file_path("./relative/img.png"));
        assert!(looks_like_file_path("../parent/img.png"));
        assert!(looks_like_file_path("'/quoted/path.png'"));
        // Windows paths
        assert!(looks_like_file_path("C:\\Users\\test\\img.png"));
        assert!(looks_like_file_path("D:/tmp/img.png"));
        // Backslash-escaped path (starts with /)
        assert!(looks_like_file_path("/Users/foo/Screenshot\\ 2026.png"));
        assert!(!looks_like_file_path("just-a-word"));
        assert!(!looks_like_file_path("relative.png"));
    }

    // ── tokenize_shell_aware tests ───────────────────────────

    #[test]
    fn test_tokenize_simple() {
        assert_eq!(tokenize_shell_aware("hello world"), vec!["hello", "world"],);
    }

    #[test]
    fn test_tokenize_backslash_spaces() {
        let tokens = tokenize_shell_aware(
            "explain /Users/foo/Screenshot\\ 2026-04-09\\ at\\ 4.37.01\\ PM.png",
        );
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0], "explain");
        assert_eq!(
            tokens[1],
            "/Users/foo/Screenshot\\ 2026-04-09\\ at\\ 4.37.01\\ PM.png",
        );
    }

    #[test]
    fn test_tokenize_double_quoted() {
        let tokens = tokenize_shell_aware(r#"explain "/Users/foo/Screenshot 2026.png" please"#);
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0], "explain");
        assert_eq!(tokens[1], "\"/Users/foo/Screenshot 2026.png\"");
        assert_eq!(tokens[2], "please");
    }

    #[test]
    fn test_tokenize_single_quoted() {
        let tokens = tokenize_shell_aware("explain '/Users/foo/Screenshot 2026.png'");
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0], "explain");
        assert_eq!(tokens[1], "'/Users/foo/Screenshot 2026.png'");
    }

    #[test]
    fn test_tokenize_at_ref_with_escaped_spaces() {
        let tokens = tokenize_shell_aware("what is @docs/my\\ file.rs");
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0], "what");
        assert_eq!(tokens[1], "is");
        assert_eq!(tokens[2], "@docs/my\\ file.rs");
    }

    #[test]
    fn test_tokenize_mixed() {
        let tokens = tokenize_shell_aware("fix @code.rs /tmp/err\\ log.png normal-word");
        assert_eq!(tokens.len(), 4);
        assert_eq!(tokens[0], "fix");
        assert_eq!(tokens[1], "@code.rs");
        assert_eq!(tokens[2], "/tmp/err\\ log.png");
        assert_eq!(tokens[3], "normal-word");
    }

    #[test]
    fn test_tokenize_empty() {
        assert!(tokenize_shell_aware("").is_empty());
        assert!(tokenize_shell_aware("   ").is_empty());
    }

    #[test]
    fn test_tokenize_unicode_cjk_path() {
        // CJK characters in paths must be preserved verbatim — they're
        // regular characters to the tokenizer, no escaping needed.
        let tokens = tokenize_shell_aware("read /home/用户/文件.rs please");
        assert_eq!(tokens, vec!["read", "/home/用户/文件.rs", "please"]);
    }

    #[test]
    fn test_tokenize_trailing_backslash() {
        // A trailing backslash (incomplete escape) should not panic and
        // should be kept as-is so unescape_path can handle it downstream.
        let tokens = tokenize_shell_aware("fix bad\\");
        assert_eq!(tokens, vec!["fix", "bad\\"]);
    }

    #[test]
    fn test_tokenize_multiple_consecutive_spaces() {
        // Multiple spaces between tokens must not produce empty tokens.
        let tokens = tokenize_shell_aware("fix  the  bug");
        assert_eq!(tokens, vec!["fix", "the", "bug"]);
    }

    // ── unescape_path tests ─────────────────────────────────

    #[test]
    fn test_unescape_path_spaces() {
        assert_eq!(
            unescape_path("/Users/foo/Screenshot\\ 2026.png"),
            "/Users/foo/Screenshot 2026.png",
        );
    }

    #[test]
    fn test_unescape_path_parens() {
        assert_eq!(unescape_path("file\\ \\(1\\).png"), "file (1).png",);
    }

    #[test]
    fn test_unescape_path_no_escapes() {
        assert_eq!(unescape_path("/simple/path.rs"), "/simple/path.rs");
    }

    #[test]
    fn test_unescape_path_trailing_backslash() {
        assert_eq!(unescape_path("trailing\\"), "trailing\\");
    }

    #[test]
    fn test_drag_and_drop_absolute_path() {
        let dir = TempDir::new().unwrap();
        let png_bytes: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let img_path = dir.path().join("screenshot.png");
        fs::write(&img_path, png_bytes).unwrap();

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
        fs::write(&img_path, png_bytes).unwrap();

        // Single-quoted (some terminals do this)
        let input = format!("explain '{}'", img_path.display());
        let result = process_input(&input, dir.path());
        assert_eq!(result.prompt, "explain");
        assert_eq!(result.images.len(), 1);
    }

    #[test]
    fn test_drag_and_drop_escaped_spaces() {
        let dir = TempDir::new().unwrap();
        let png_bytes: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        // Create a file with spaces in its name
        let img_path = dir.path().join("Screenshot 2026-04-09 at 4.37.01 PM.png");
        fs::write(&img_path, png_bytes).unwrap();

        // Simulate terminal drag-and-drop: backslash-escaped spaces
        let escaped_path = img_path.display().to_string().replace(' ', "\\ ");
        let input = format!("what is this {escaped_path}");
        let result = process_input(&input, dir.path());
        assert_eq!(result.prompt, "what is this");
        assert_eq!(
            result.images.len(),
            1,
            "image should be loaded from escaped path"
        );
        assert_eq!(result.images[0].media_type, "image/png");
    }

    #[test]
    fn test_drag_and_drop_quoted_spaces() {
        let dir = TempDir::new().unwrap();
        let png_bytes: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let img_path = dir.path().join("Screenshot 2026.png");
        fs::write(&img_path, png_bytes).unwrap();

        // Simulate terminal drag-and-drop: double-quoted path
        let input = format!("what is this \"{}\"", img_path.display());
        let result = process_input(&input, dir.path());
        assert_eq!(result.prompt, "what is this");
        assert_eq!(
            result.images.len(),
            1,
            "image should be loaded from quoted path"
        );
    }

    #[test]
    fn test_at_ref_with_escaped_spaces() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("my file.rs");
        fs::write(&file_path, "fn main() {}").unwrap();

        let result = process_input("explain @my\\ file.rs", dir.path());
        assert_eq!(result.prompt, "explain");
        assert_eq!(result.context_files.len(), 1);
        assert_eq!(result.context_files[0].content, "fn main() {}");
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

    #[test]
    fn test_resolve_bare_path_absolute() {
        #[cfg(unix)]
        {
            let resolved = resolve_bare_path("/tmp/test.png");
            assert_eq!(resolved, Some(PathBuf::from("/tmp/test.png")));
        }
        #[cfg(windows)]
        {
            let resolved = resolve_bare_path("C:\\tmp\\test.png");
            assert_eq!(resolved, Some(PathBuf::from("C:\\tmp\\test.png")));
        }
    }

    #[test]
    fn test_resolve_bare_path_home() {
        // Only works if HOME is set, which it always is in tests
        if std::env::var("HOME").is_ok() {
            let resolved = resolve_bare_path("~/test.png");
            assert!(resolved.is_some());
            let path = resolved.unwrap();
            assert!(!path.to_string_lossy().contains('~'));
            assert!(path.to_string_lossy().ends_with("test.png"));
        }
    }

    #[test]
    fn test_resolve_bare_path_quoted() {
        #[cfg(unix)]
        {
            let resolved = resolve_bare_path("'/tmp/test.png'");
            assert_eq!(resolved, Some(PathBuf::from("/tmp/test.png")));
        }
        #[cfg(windows)]
        {
            let resolved = resolve_bare_path("'C:\\tmp\\test.png'");
            assert_eq!(resolved, Some(PathBuf::from("C:\\tmp\\test.png")));
        }
    }

    #[test]
    fn test_resolve_bare_path_relative() {
        let resolved = resolve_bare_path("./test.png");
        assert!(resolved.is_some());
        // Should be resolved to an absolute path via cwd
        assert!(resolved.unwrap().is_absolute());
    }

    #[test]
    fn test_at_file_traversal_blocked() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("safe.rs"), "fn main() {}").unwrap();

        let result = process_input("read @../../etc/passwd", dir.path());
        // Traversal path should be rejected — no context files loaded
        assert!(
            result.context_files.is_empty(),
            "traversal should not load files outside project root"
        );
        // The @ref should remain in the prompt as-is
        assert!(result.prompt.contains("@../../etc/passwd"));
    }

    #[test]
    fn test_format_paste_blocks_empty() {
        assert!(format_paste_blocks(&[]).is_none());
    }

    #[test]
    fn test_format_paste_blocks_single() {
        let blocks = vec![PasteBlock {
            content: "hello world".into(),
            char_count: 11,
        }];
        let result = format_paste_blocks(&blocks).unwrap();
        assert!(result.contains("<reference type=\"pasted\" chars=\"11\">"));
        assert!(result.contains("hello world"));
        assert!(result.contains("</reference>"));
    }

    #[test]
    fn test_format_paste_blocks_multiple() {
        let blocks = vec![
            PasteBlock {
                content: "block one".into(),
                char_count: 9,
            },
            PasteBlock {
                content: "block two".into(),
                char_count: 9,
            },
        ];
        let result = format_paste_blocks(&blocks).unwrap();
        assert!(result.contains("block one"));
        assert!(result.contains("block two"));
        // Joined with double newline
        assert!(result.contains("</reference>\n\n<reference"));
    }

    #[test]
    fn test_format_paste_blocks_truncation() {
        let long_content = "a".repeat(50_000);
        let blocks = vec![PasteBlock {
            content: long_content,
            char_count: 50_000,
        }];
        let result = format_paste_blocks(&blocks).unwrap();
        assert!(result.contains("[truncated — 50000 chars total]"));
        // Should be capped, not the full 50k
        assert!(result.len() < 45_000);
    }
}
