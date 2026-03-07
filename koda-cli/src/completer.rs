//! Tab completion for TUI input.
//!
//! Handles two completion modes:
//! - **Slash commands**: `/d` → `/diff`, `/diff commit`, `/diff review`
//! - **@file paths**: `explain @src/m` → `explain @src/main.rs`

use std::path::{Path, PathBuf};

/// All known slash commands.
const SLASH_COMMANDS: &[&str] = &[
    "/agent",
    "/compact",
    "/cost",
    "/diff",
    "/diff commit",
    "/diff review",
    "/exit",
    "/expand",
    "/help",
    "/mcp",
    "/memory",
    "/model",
    "/provider",
    "/sessions",
    "/trust",
    "/verbose",
];

/// Unified Tab-completion for slash commands and @file paths.
pub struct InputCompleter {
    /// Current completion matches.
    matches: Vec<String>,
    /// Index into `matches` for cycling.
    idx: usize,
    /// The token being completed (to detect changes).
    token: String,
    /// Project root for @file path resolution.
    project_root: PathBuf,
    /// Cached model names for `/model` completion.
    model_names: Vec<String>,
}

impl InputCompleter {
    pub fn new(project_root: PathBuf) -> Self {
        Self {
            matches: Vec::new(),
            idx: 0,
            token: String::new(),
            project_root,
            model_names: Vec::new(),
        }
    }

    /// Update the cached model names (call after provider switch or model list fetch).
    pub fn set_model_names(&mut self, names: Vec<String>) {
        self.model_names = names;
    }

    /// Attempt to complete the current input text.
    ///
    /// Returns `Some(replacement_text)` with the full input line replaced,
    /// or `None` if no completion is available.
    /// Repeated calls cycle through matches.
    pub fn complete(&mut self, current_text: &str) -> Option<String> {
        let trimmed = current_text.trim_end();

        // Slash command completion: input starts with /
        if trimmed.starts_with('/') {
            // /model <partial> → complete model names
            if let Some(partial) = trimmed.strip_prefix("/model ") {
                return self.complete_model(partial);
            }
            return self.complete_slash(trimmed);
        }

        // @file completion: find the last @token in the input
        if let Some(at_pos) = find_last_at_token(trimmed) {
            let partial = &trimmed[at_pos + 1..]; // after @
            let prefix = &trimmed[..at_pos]; // everything before @
            return self.complete_file(prefix, partial);
        }

        self.reset();
        None
    }

    /// Get the current completion candidates (for display in the TUI).
    /// Returns the raw match values (without the `/model ` or `@` prefix context).
    pub fn candidates(&self) -> &[String] {
        &self.matches
    }

    /// Get the index of the currently selected candidate.
    pub fn selected_idx(&self) -> usize {
        // idx points to the *next* one, so the current selection is idx - 1
        if self.idx == 0 && !self.matches.is_empty() {
            self.matches.len() - 1
        } else {
            self.idx.saturating_sub(1)
        }
    }

    /// Reset completion state (call on non-Tab keystrokes).
    pub fn reset(&mut self) {
        self.matches.clear();
        self.idx = 0;
        self.token.clear();
    }

    // ── Slash command completion ─────────────────────────────

    fn complete_slash(&mut self, trimmed: &str) -> Option<String> {
        // Rebuild matches if the token changed
        if trimmed != self.token && !self.matches.iter().any(|m| m == trimmed) {
            self.token = trimmed.to_string();
            self.matches = SLASH_COMMANDS
                .iter()
                .filter(|cmd| cmd.starts_with(trimmed) && **cmd != trimmed)
                .map(|s| s.to_string())
                .collect();
            self.idx = 0;
        }

        if self.matches.is_empty() {
            return None;
        }

        let result = self.matches[self.idx].clone();
        self.idx = (self.idx + 1) % self.matches.len();
        Some(result)
    }

    // ── /model name completion ──────────────────────────────

    fn complete_model(&mut self, partial: &str) -> Option<String> {
        let token_key = format!("/model {partial}");

        if token_key != self.token {
            self.token = token_key;
            self.matches = self
                .model_names
                .iter()
                .filter(|name| name.contains(partial) && name.as_str() != partial)
                .map(|name| format!("/model {name}"))
                .collect();
            self.idx = 0;
        }

        if self.matches.is_empty() {
            return None;
        }

        let result = self.matches[self.idx].clone();
        self.idx = (self.idx + 1) % self.matches.len();
        Some(result)
    }

    // ── @file path completion ────────────────────────────────

    fn complete_file(&mut self, prefix: &str, partial: &str) -> Option<String> {
        // Check if the partial is already one of our matches (user is cycling)
        let is_cycling = !self.matches.is_empty() && self.matches.iter().any(|m| m == partial);

        if !is_cycling {
            self.token = format!("@{partial}");
            self.matches = list_path_matches(&self.project_root, partial);
            self.idx = 0;
        }

        if self.matches.is_empty() {
            return None;
        }

        let path = &self.matches[self.idx];
        self.idx = (self.idx + 1) % self.matches.len();

        // Rebuild full input: prefix + @completed_path
        Some(format!("{prefix}@{path}"))
    }
}

// ── Helpers ─────────────────────────────────────────────────

/// Find the byte position of the last `@` that starts a file reference.
///
/// An `@` counts as a file reference if it's preceded by whitespace
/// or is at the start of the input (not an email address).
fn find_last_at_token(text: &str) -> Option<usize> {
    for (i, c) in text.char_indices().rev() {
        if c == '@' && (i == 0 || matches!(text.as_bytes()[i - 1], b' ' | b'\n')) {
            return Some(i);
        }
    }
    None
}

/// List filesystem paths matching a partial path relative to project_root.
///
/// Given partial `"src/m"`, lists files in `project_root/src/` starting with `m`.
/// Given partial `"src/"`, lists all files in `project_root/src/`.
/// Directories get a trailing `/` to encourage further completion.
fn list_path_matches(project_root: &Path, partial: &str) -> Vec<String> {
    let (dir_part, file_prefix) = match partial.rfind('/') {
        Some(pos) => (&partial[..=pos], &partial[pos + 1..]),
        None => ("", partial),
    };

    let search_dir = if dir_part.is_empty() {
        project_root.to_path_buf()
    } else {
        project_root.join(dir_part)
    };

    let entries = match std::fs::read_dir(&search_dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut matches: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();

            // Skip hidden files and common noise
            if name.starts_with('.') {
                return None;
            }

            let lower_prefix = file_prefix.to_lowercase();
            if !name.to_lowercase().starts_with(&lower_prefix) {
                return None;
            }

            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);

            // Skip build artifacts / deps
            if is_dir
                && matches!(
                    name.as_str(),
                    "target" | "node_modules" | "__pycache__" | ".git"
                )
            {
                return None;
            }

            let path = if is_dir {
                format!("{dir_part}{name}/")
            } else {
                format!("{dir_part}{name}")
            };
            Some(path)
        })
        .collect();

    matches.sort();
    matches
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    // ── Slash command tests ─────────────────────────────────

    #[test]
    fn test_complete_slash_d() {
        let tmp = tempdir().unwrap();
        let mut c = InputCompleter::new(tmp.path().to_path_buf());
        let first = c.complete("/d");
        assert!(first.is_some());
        assert!(first.unwrap().starts_with("/d"));
    }

    #[test]
    fn test_complete_cycles() {
        let tmp = tempdir().unwrap();
        let mut c = InputCompleter::new(tmp.path().to_path_buf());
        let a = c.complete("/d");
        let b = c.complete("/d");
        assert!(a.is_some());
        assert!(b.is_some());
    }

    #[test]
    fn test_no_match() {
        let tmp = tempdir().unwrap();
        let mut c = InputCompleter::new(tmp.path().to_path_buf());
        assert!(c.complete("/zzz").is_none());
    }

    #[test]
    fn test_non_slash_no_at_returns_none() {
        let tmp = tempdir().unwrap();
        let mut c = InputCompleter::new(tmp.path().to_path_buf());
        assert!(c.complete("hello").is_none());
    }

    #[test]
    fn test_exact_match_no_complete() {
        let tmp = tempdir().unwrap();
        let mut c = InputCompleter::new(tmp.path().to_path_buf());
        assert!(c.complete("/exit").is_none());
    }

    // ── @file completion tests ───────────────────────────────

    #[test]
    fn test_at_file_completes() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("main.rs"), "fn main() {}").unwrap();
        fs::write(tmp.path().join("mod.rs"), "").unwrap();

        let mut c = InputCompleter::new(tmp.path().to_path_buf());
        let result = c.complete("explain @m");
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(text.starts_with("explain @m"), "got: {text}");
        assert!(
            text.contains("main.rs") || text.contains("mod.rs"),
            "got: {text}"
        );
    }

    #[test]
    fn test_at_file_in_subdir() {
        let tmp = tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/lib.rs"), "").unwrap();
        fs::write(tmp.path().join("src/main.rs"), "").unwrap();

        let mut c = InputCompleter::new(tmp.path().to_path_buf());
        let result = c.complete("@src/l");
        assert_eq!(result, Some("@src/lib.rs".to_string()));
    }

    #[test]
    fn test_at_file_dir_gets_trailing_slash() {
        let tmp = tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();

        let mut c = InputCompleter::new(tmp.path().to_path_buf());
        let result = c.complete("@s");
        assert_eq!(result, Some("@src/".to_string()));
    }

    #[test]
    fn test_at_file_cycles() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("alpha.rs"), "").unwrap();
        fs::write(tmp.path().join("beta.rs"), "").unwrap();

        let mut c = InputCompleter::new(tmp.path().to_path_buf());
        // First Tab: input is "@" → returns first match
        let a = c.complete("@").unwrap();
        // Second Tab: input is now the completed text (e.g., "@alpha.rs")
        let b = c.complete(&a).unwrap();
        assert_ne!(a, b, "should cycle through different files");
        // Third Tab: should cycle back
        let c_result = c.complete(&b).unwrap();
        assert_eq!(c_result, a, "should cycle back to first");
        assert_eq!(c_result, a, "should cycle back to first");
    }

    #[test]
    fn test_at_file_skips_hidden() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join(".hidden"), "").unwrap();
        fs::write(tmp.path().join("visible.rs"), "").unwrap();

        let mut c = InputCompleter::new(tmp.path().to_path_buf());
        let result = c.complete("@");
        assert_eq!(result, Some("@visible.rs".to_string()));
    }

    #[test]
    fn test_at_file_case_insensitive() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("Makefile"), "").unwrap();
        fs::write(tmp.path().join("README.md"), "").unwrap();

        let mut c = InputCompleter::new(tmp.path().to_path_buf());
        let result = c.complete("@make");
        assert_eq!(result, Some("@Makefile".to_string()));

        c.reset();
        let result = c.complete("@read");
        assert_eq!(result, Some("@README.md".to_string()));
    }

    #[test]
    fn test_at_file_preserves_prefix_text() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("config.toml"), "").unwrap();

        let mut c = InputCompleter::new(tmp.path().to_path_buf());
        let result = c.complete("review this @c");
        assert_eq!(result, Some("review this @config.toml".to_string()));
    }

    // ── /model completion tests ──────────────────────────────

    #[test]
    fn test_model_complete() {
        let tmp = tempdir().unwrap();
        let mut c = InputCompleter::new(tmp.path().to_path_buf());
        c.set_model_names(vec![
            "gpt-4o".into(),
            "gpt-4o-mini".into(),
            "gpt-3.5-turbo".into(),
        ]);
        let result = c.complete("/model gpt-4");
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(text.starts_with("/model gpt-4"), "got: {text}");
    }

    #[test]
    fn test_model_complete_cycles() {
        let tmp = tempdir().unwrap();
        let mut c = InputCompleter::new(tmp.path().to_path_buf());
        c.set_model_names(vec!["gpt-4o".into(), "gpt-4o-mini".into()]);
        let a = c.complete("/model gpt");
        let b = c.complete("/model gpt");
        assert!(a.is_some());
        assert!(b.is_some());
        assert_ne!(a, b, "should cycle through models");
    }

    #[test]
    fn test_model_no_names_returns_none() {
        let tmp = tempdir().unwrap();
        let mut c = InputCompleter::new(tmp.path().to_path_buf());
        // No model names set
        assert!(c.complete("/model gpt").is_none());
    }

    #[test]
    fn test_model_substring_match() {
        let tmp = tempdir().unwrap();
        let mut c = InputCompleter::new(tmp.path().to_path_buf());
        c.set_model_names(vec!["claude-3-sonnet".into(), "claude-3-opus".into()]);
        let result = c.complete("/model opus");
        assert_eq!(result, Some("/model claude-3-opus".to_string()));
    }

    // ── Helper tests ────────────────────────────────────────

    #[test]
    fn test_find_last_at_token() {
        assert_eq!(find_last_at_token("@file"), Some(0));
        assert_eq!(find_last_at_token("explain @file"), Some(8));
        assert_eq!(find_last_at_token("email@domain"), None); // no space before @
        assert_eq!(find_last_at_token("a @b @c"), Some(5)); // last @
        assert_eq!(find_last_at_token("no at here"), None);
        // @ after newline (multi-line input via Shift+Enter)
        assert_eq!(find_last_at_token("line1\n@file"), Some(6));
        assert_eq!(find_last_at_token("a\nb\n@c"), Some(4));
    }

    #[test]
    fn test_at_file_after_newline() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("config.toml"), "").unwrap();

        let mut c = InputCompleter::new(tmp.path().to_path_buf());
        // Simulate multi-line input: first line + newline + @partial
        let result = c.complete("explain this\n@c");
        assert_eq!(result, Some("explain this\n@config.toml".to_string()));
    }
}
