//! Input processing — @file references and image loading.
//!
//! Processes user input for `@path` references, loading file contents
//! as additional context and images for multi-modal prompts.

use koda_core::providers::ImageData;
use std::path::{Path, PathBuf};

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
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
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
        // Windows absolute paths: C:\ or D:\
        || (cleaned.len() >= 3
            && cleaned.as_bytes()[0].is_ascii_alphabetic()
            && cleaned.as_bytes()[1] == b':'
            && (cleaned.as_bytes()[2] == b'\\' || cleaned.as_bytes()[2] == b'/'))
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
    if let Some(rest) = cleaned.strip_prefix("~/") {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .ok()?;
        Some(PathBuf::from(home).join(rest))
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

            let raw_path = strip_quotes(raw_path);

            // Security: reject paths that escape the project root
            let full_path = match koda_core::tools::safe_resolve_path(project_root, raw_path) {
                Ok(p) => p,
                Err(_) => {
                    tracing::warn!("@file path escapes project root: {raw_path}");
                    prompt_parts.push(token.to_string());
                    continue;
                }
            };

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
        if looks_like_file_path(token)
            && is_image_file(unquoted)
            && let Some(resolved) = resolve_bare_path(token)
            && resolved.exists()
        {
            let display = resolved.display().to_string();
            if let Some(img) = try_load_image(&resolved, &display) {
                images.push(img);
                continue;
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
        assert!(!looks_like_file_path("just-a-word"));
        assert!(!looks_like_file_path("relative.png"));
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
}
