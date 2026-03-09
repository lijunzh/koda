//! Command history persistence.
//!
//! Extracted from `tui_app.rs`. Handles loading and saving
//! the user's command history to disk. See #209.

use std::path::PathBuf;

const MAX_HISTORY: usize = 500;

pub(crate) fn history_file_path() -> PathBuf {
    let config_dir = std::env::var("XDG_CONFIG_HOME")
        .or_else(|_| std::env::var("HOME").map(|h| format!("{h}/.config")))
        .or_else(|_| std::env::var("USERPROFILE").map(|h| format!("{h}/.config")))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(config_dir).join("koda").join("history")
}

pub(crate) fn load_history() -> Vec<String> {
    let path = history_file_path();
    match std::fs::read_to_string(&path) {
        Ok(content) => content
            .lines()
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect(),
        Err(_) => Vec::new(),
    }
}

pub(crate) fn save_history(history: &[String]) {
    let path = history_file_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let start = history.len().saturating_sub(MAX_HISTORY);
    let content = history[start..].join("\n");
    let _ = std::fs::write(&path, content);
}
