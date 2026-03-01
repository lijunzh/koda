//! Clipboard utilities: copy/paste using platform commands.
//!
//! Uses `pbcopy`/`pbpaste` on macOS, `xclip` on Linux,
//! `clip`/PowerShell on Windows. No external crate needed.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Mutex;

// ── Last response storage ─────────────────────────────────────

static LAST_RESPONSE: Mutex<String> = Mutex::new(String::new());

/// Store the most recent LLM response for `/copy`.
pub fn set_last_response(text: &str) {
    *LAST_RESPONSE.lock().unwrap() = text.to_string();
}

/// Get the most recent LLM response.
pub fn get_last_response() -> String {
    LAST_RESPONSE.lock().unwrap().clone()
}

// ── Clipboard operations ──────────────────────────────────────

/// Copy text to the system clipboard.
pub fn copy_to_clipboard(text: &str) -> Result<(), String> {
    let (cmd, args) = clipboard_copy_cmd();

    let mut child = Command::new(cmd)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to run {cmd}: {e}. Is it installed?"))?;

    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(text.as_bytes())
            .map_err(|e| format!("Failed to write to clipboard: {e}"))?;
    }

    child
        .wait()
        .map_err(|e| format!("Clipboard command failed: {e}"))?;

    Ok(())
}

/// Read text from the system clipboard.
pub fn paste_from_clipboard() -> Result<String, String> {
    let (cmd, args) = clipboard_paste_cmd();

    let output = Command::new(cmd)
        .args(&args)
        .output()
        .map_err(|e| format!("Failed to run {cmd}: {e}. Is it installed?"))?;

    if output.status.success() {
        String::from_utf8(output.stdout)
            .map_err(|e| format!("Clipboard content is not valid UTF-8: {e}"))
    } else {
        Err("Clipboard paste command failed".to_string())
    }
}

/// Extract code blocks from markdown text.
pub fn extract_code_blocks(text: &str) -> Vec<(Option<String>, String)> {
    let mut blocks = Vec::new();
    let mut in_block = false;
    let mut lang = None;
    let mut current = String::new();

    for line in text.lines() {
        if line.starts_with("```") {
            if in_block {
                // Closing fence
                blocks.push((lang.take(), current.trim_end().to_string()));
                current.clear();
                in_block = false;
            } else {
                // Opening fence
                let l = line.trim_start_matches('`').trim();
                lang = if l.is_empty() {
                    None
                } else {
                    Some(l.to_string())
                };
                in_block = true;
            }
        } else if in_block {
            current.push_str(line);
            current.push('\n');
        }
    }

    blocks
}

// ── Platform detection ────────────────────────────────────────

fn clipboard_copy_cmd() -> (&'static str, Vec<&'static str>) {
    if cfg!(target_os = "macos") {
        ("pbcopy", vec![])
    } else if cfg!(target_os = "windows") {
        ("clip", vec![])
    } else {
        // Linux / BSDs — try xclip
        ("xclip", vec!["-selection", "clipboard"])
    }
}

fn clipboard_paste_cmd() -> (&'static str, Vec<&'static str>) {
    if cfg!(target_os = "macos") {
        ("pbpaste", vec![])
    } else if cfg!(target_os = "windows") {
        ("powershell", vec!["-command", "Get-Clipboard"])
    } else {
        ("xclip", vec!["-selection", "clipboard", "-o"])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_last_response_storage() {
        set_last_response("hello world");
        assert_eq!(get_last_response(), "hello world");
        set_last_response("updated");
        assert_eq!(get_last_response(), "updated");
    }

    #[test]
    fn test_extract_code_blocks_none() {
        let blocks = extract_code_blocks("No code here, just text.");
        assert!(blocks.is_empty());
    }

    #[test]
    fn test_extract_code_blocks_single() {
        let text = "Some text\n```rust\nfn main() {}\n```\nMore text";
        let blocks = extract_code_blocks(text);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].0.as_deref(), Some("rust"));
        assert_eq!(blocks[0].1, "fn main() {}");
    }

    #[test]
    fn test_extract_code_blocks_multiple() {
        let text = "```python\nprint('a')\n```\n\n```\nplain block\n```";
        let blocks = extract_code_blocks(text);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].0.as_deref(), Some("python"));
        assert_eq!(blocks[1].0, None);
    }

    #[test]
    fn test_extract_code_blocks_no_lang() {
        let text = "```\nno lang\n```";
        let blocks = extract_code_blocks(text);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].0, None);
        assert_eq!(blocks[0].1, "no lang");
    }
}
