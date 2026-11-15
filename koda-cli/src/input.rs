//! Input processing: `@path` mentions, dragged-in images and pastes.
//!
//! An `@path` mention stays in the text as typed and goes to the host as a
//! path, which reads it on its own machine under the Read tool's rules, as
//! Claude Code includes an `@` file. An image dragged in as a bare path is
//! the client's own file, so the client loads it and sends its bytes.

use std::path::{Path, PathBuf};

// ── @file pre-processor ────────────────────────────────────────

/// An image dragged in as a bare path, sent with the prompt as the
/// protocol's `ImageInput`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageData {
    /// MIME type (e.g. "image/png", "image/jpeg").
    pub media_type: String,
    /// Base64-encoded image bytes.
    pub base64: String,
}

/// Content pasted via clipboard (bracketed paste).
#[derive(Debug, Clone)]
pub struct PasteBlock {
    /// The raw pasted text.
    pub content: String,
    /// Character count.
    pub char_count: usize,
}

/// Result of processing user input.
#[derive(Debug)]
pub struct ProcessedInput {
    /// The prompt: the input as typed, `@path` mentions included; only
    /// dragged-in images leave it.
    pub prompt: String,
    /// The paths mentioned with `@`, in order, each once: sent as the
    /// protocol's `mentions` for the host to read.
    pub mentions: Vec<String>,
    /// Images dragged in as bare paths, base64-encoded.
    pub images: Vec<ImageData>,
    /// Pasted content blocks (from bracketed paste).
    pub paste_blocks: Vec<PasteBlock>,
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

/// One piece of the user's input.
#[derive(Debug, PartialEq)]
enum Piece {
    Token(String),
    Space(String),
}

/// Shell-aware tokenizer that keeps backslash-escaped and quoted paths intact.
///
/// Unlike `split_whitespace()`, this handles:
/// - Backslash-escaped spaces: `path\ with\ spaces` → single token
/// - Quoted strings: `"path with spaces"` or `'path with spaces'` → single token
/// - Regular whitespace splitting for everything else
///
/// Whitespace comes back too, as its own pieces, so the pieces joined are
/// exactly the input.
///
/// Inspired by Gemini CLI's regex-based `@path` parsing and Claude Code's
/// `stripBackslashEscapes`.
fn split_shell_aware(input: &str) -> Vec<Piece> {
    let mut pieces = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars().peekable();

    while let Some(&c) = chars.peek() {
        match c {
            // ── Whitespace: flush current token ──
            ' ' | '\t' | '\n' | '\r' => {
                if !current.is_empty() {
                    pieces.push(Piece::Token(std::mem::take(&mut current)));
                }
                match pieces.last_mut() {
                    Some(Piece::Space(space)) => space.push(c),
                    _ => pieces.push(Piece::Space(c.to_string())),
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
        pieces.push(Piece::Token(current));
    }
    pieces
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

/// Remove the spaces an attachment leaves behind, stopping at a newline.
fn drop_trailing_spaces(prompt: &mut String) {
    prompt.truncate(prompt.trim_end_matches([' ', '\t']).len());
}

/// Scan input for `@path` mentions and bare image paths (drag-and-drop):
/// the mentions are collected for the host, the images loaded here.
///
/// The prompt keeps the input's text exactly, newlines and indentation
/// included, `@path` mentions too. A dragged-in image leaves with the spaces
/// before it, or the whitespace after it when it opens the prompt.
pub fn process_input(input: &str) -> ProcessedInput {
    let mut prompt = String::new();
    let mut mentions: Vec<String> = Vec::new();
    let mut images = Vec::new();

    for piece in split_shell_aware(input) {
        let token = match piece {
            Piece::Token(token) => token,
            Piece::Space(space) => {
                let opens_prompt = prompt.is_empty() && !input.starts_with(&space);
                if !opens_prompt {
                    prompt.push_str(&space);
                }
                continue;
            }
        };

        // ── @path mentions ────────────────────────────────────
        // Kept in the text as typed; the host reads the file.
        if let Some(raw_path) = token.strip_prefix('@') {
            if !raw_path.is_empty() {
                let path = unescape_path(strip_quotes(raw_path));
                if !mentions.contains(&path) {
                    mentions.push(path);
                }
            }
            prompt.push_str(&token);
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
                drop_trailing_spaces(&mut prompt);
                continue;
            }
        }

        prompt.push_str(&token);
    }

    // Only a dragged-in image, with no words: say what to do with it.
    let prompt = if prompt.trim().is_empty() && !images.is_empty() {
        "Describe and analyze this image.".to_string()
    } else {
        prompt
    };

    ProcessedInput {
        prompt,
        mentions,
        images,
        paste_blocks: Vec::new(),
    }
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

    /// The tokens alone, for the tests that only care where words split.
    fn tokenize_shell_aware(input: &str) -> Vec<String> {
        split_shell_aware(input)
            .into_iter()
            .filter_map(|piece| match piece {
                Piece::Token(token) => Some(token),
                Piece::Space(_) => None,
            })
            .collect()
    }
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn a_mention_stays_in_the_text_and_goes_to_the_host() {
        let result = process_input("explain @test.rs");
        assert_eq!(result.prompt, "explain @test.rs");
        assert_eq!(result.mentions, ["test.rs"]);
    }

    #[test]
    fn test_process_input_no_refs() {
        let result = process_input("just a normal question");
        assert_eq!(result.prompt, "just a normal question");
        assert!(result.mentions.is_empty());
    }

    #[test]
    fn test_process_input_keeps_text_exactly() {
        for prompt in [
            "Repair:\n```python\ndef f():\n    return 1\n```\n",
            "  leading and trailing  ",
            "tabs\tand\r\ncrlf\n\n\nblank lines",
            "diff:\n-    old\n+\tnew\n",
            "quotes 'keep\n  this' and \"that\" and a \\ backslash",
            "it's an unmatched\n    quote",
            "@missing.rs stays\n  as text",
            "",
        ] {
            let result = process_input(prompt);
            assert_eq!(result.prompt, prompt);
        }
    }

    #[test]
    fn mentions_leave_the_text_exactly_as_typed() {
        for (input, mentions) in [
            ("@a.rs\n\nfix this:\n    indented", vec!["a.rs"]),
            ("see @a.rs and @b.rs and\n  this", vec!["a.rs", "b.rs"]),
            ("@a.rs then @a.rs again", vec!["a.rs"]),
            ("mail me at x@y.com, or @", vec![]),
        ] {
            let result = process_input(input);
            assert_eq!(result.prompt, input);
            assert_eq!(result.mentions, mentions, "{input}");
        }
    }

    #[test]
    fn test_pieces_join_back_to_the_input() {
        for input in ["a  b\n\tc ", "\n x", "'q\n r' s", "esc\\ aped\\\n", ""] {
            let joined: String = split_shell_aware(input)
                .into_iter()
                .map(|piece| match piece {
                    Piece::Token(text) | Piece::Space(text) => text,
                })
                .collect();
            assert_eq!(joined, input);
        }
    }

    #[test]
    fn a_mention_alone_is_the_prompt() {
        let result = process_input("@code.py");
        assert_eq!(result.prompt, "@code.py");
        assert_eq!(result.mentions, ["code.py"]);
    }

    #[test]
    /// Whether it exists is the host's to say: it reads on its own machine.
    fn a_missing_file_is_still_mentioned() {
        let result = process_input("explain @nonexistent.rs");
        assert_eq!(result.prompt, "explain @nonexistent.rs");
        assert_eq!(result.mentions, ["nonexistent.rs"]);
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
    /// An `@` image is the host's to read, like any mention.
    fn an_image_mention_goes_to_the_host() {
        let result = process_input("what is this @screenshot.png");
        assert_eq!(result.prompt, "what is this @screenshot.png");
        assert_eq!(result.mentions, ["screenshot.png"]);
        assert!(result.images.is_empty());
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
        let result = process_input(&input);
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
        let result = process_input(&input);
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
        let result = process_input(&input);
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
        let result = process_input(&input);
        assert_eq!(result.prompt, "what is this");
        assert_eq!(
            result.images.len(),
            1,
            "image should be loaded from quoted path"
        );
    }

    #[test]
    fn test_at_ref_with_escaped_spaces() {
        let result = process_input("explain @my\\ file.rs");
        assert_eq!(result.prompt, "explain @my\\ file.rs");
        assert_eq!(result.mentions, ["my file.rs"]);
    }

    #[test]
    fn test_drag_and_drop_nonexistent_stays_in_prompt() {
        let input = "/tmp/nonexistent_image_12345.png what is this";
        let result = process_input(input);
        // Non-existent file stays as text in prompt
        assert!(result.prompt.contains("/tmp/nonexistent_image_12345.png"));
        assert!(result.images.is_empty());
    }

    #[test]
    fn test_non_image_absolute_path_stays_in_prompt() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("data.json"), "{}").unwrap();
        let input = format!("read {}", dir.path().join("data.json").display());
        let result = process_input(&input);
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
    /// Which paths may be read is the host's rule (the Read tool's), not
    /// the client's: the client passes the path as written.
    fn a_path_outside_the_project_is_passed_to_the_host() {
        let result = process_input("read @../../etc/hosts");
        assert_eq!(result.mentions, ["../../etc/hosts"]);
        assert!(result.prompt.contains("@../../etc/hosts"));
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
