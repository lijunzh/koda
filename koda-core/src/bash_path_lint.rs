//! Bash path lint: detect commands that escape the project root.
//!
//! Heuristic analysis — catches common accidental escapes, not adversarial inputs.
//! Dynamic targets (`cd $VAR`, `cd $(cmd)`) are intentionally ignored.

use path_clean::PathClean;
use std::path::{Path, PathBuf};

use crate::bash_safety::split_command_segments;
use crate::bash_safety::strip_env_vars;

/// Whether `resolved` is a path that is safe to access outside the project root.
///
/// Safe paths include:
/// - Temp directories: `/tmp`, `$TMPDIR` (on macOS `/tmp` → `/private/tmp`,
///   `$TMPDIR` → `/private/var/folders/.../T/`)
/// - Device files: `/dev/null`, `/dev/stdout`, `/dev/stderr`
pub fn is_safe_external_path(resolved: &Path) -> bool {
    // Device files — not real filesystem writes
    if resolved.starts_with("/dev/") {
        return true;
    }

    // Canonical /tmp (covers /private/tmp on macOS via symlink)
    let canonical_tmp = PathBuf::from("/tmp")
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("/tmp"));
    if resolved.starts_with(&canonical_tmp) || resolved.starts_with("/tmp") {
        return true;
    }

    // $TMPDIR (e.g. /var/folders/.../T/ on macOS)
    if let Ok(tmpdir) = std::env::var("TMPDIR") {
        let tmpdir_path = PathBuf::from(&tmpdir);
        let canonical_tmpdir = tmpdir_path
            .canonicalize()
            .unwrap_or_else(|_| tmpdir_path.clone());
        if resolved.starts_with(&canonical_tmpdir) || resolved.starts_with(&tmpdir_path) {
            return true;
        }
    }

    false
}

/// Result of linting a bash command for path escapes.
#[derive(Debug, Clone, Default)]
pub struct BashPathLint {
    /// Paths in the command that escape project_root.
    pub outside_paths: Vec<String>,
    /// Whether the command contains `cd ~` or bare `cd` (→ $HOME).
    pub home_escape: bool,
}

impl BashPathLint {
    /// Whether the lint found any warnings.
    pub fn has_warnings(&self) -> bool {
        !self.outside_paths.is_empty() || self.home_escape
    }
}

/// Lint a bash command for paths that escape project_root.
///
/// # Examples
///
/// ```
/// use std::path::Path;
/// use koda_core::bash_path_lint::lint_bash_paths;
///
/// let lint = lint_bash_paths("cat src/main.rs", Path::new("/project"));
/// assert!(!lint.has_warnings());
///
/// let lint = lint_bash_paths("cat /etc/passwd", Path::new("/project"));
/// assert!(lint.has_warnings());
/// ```
pub fn lint_bash_paths(command: &str, project_root: &Path) -> BashPathLint {
    let mut lint = BashPathLint::default();
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return lint;
    }

    let segments = split_command_segments(trimmed);

    for segment in &segments {
        let seg = segment.trim();

        // Check for cd targets
        if let Some(target) = extract_cd_target(seg) {
            match target {
                CdTarget::Home => lint.home_escape = true,
                CdTarget::Dynamic => {} // can't resolve, skip
                CdTarget::Path(p) => {
                    let path = Path::new(&p);
                    let resolved = if path.is_absolute() {
                        path.to_path_buf().clean()
                    } else {
                        project_root.join(&p).clean()
                    };
                    if !resolved.starts_with(project_root) && !is_safe_external_path(&resolved) {
                        lint.outside_paths.push(p);
                    }
                }
            }
        }

        // Check for absolute path arguments (not cd).
        // Strip quoted strings first so paths inside commit messages,
        // echo strings, etc. are not falsely flagged (#562).
        let unquoted = strip_quoted_strings(seg);
        for token in unquoted.split_whitespace().skip(1) {
            if token.starts_with('-') {
                continue;
            }
            if token.starts_with('/') {
                let resolved = Path::new(token).to_path_buf().clean();
                if !resolved.starts_with(project_root) && !is_safe_external_path(&resolved) {
                    lint.outside_paths.push(token.to_string());
                }
            }
            if token.contains("..") {
                let resolved = project_root.join(token).clean();
                if !resolved.starts_with(project_root) && !is_safe_external_path(&resolved) {
                    lint.outside_paths.push(token.to_string());
                }
            }
        }
    }

    lint.outside_paths.sort();
    lint.outside_paths.dedup();
    lint
}

#[derive(Debug)]
enum CdTarget {
    Home,
    Dynamic,
    Path(String),
}

/// Replace content inside matched single/double quotes with spaces.
///
/// This prevents paths embedded in commit messages, echo strings, and
/// heredoc bodies from being falsely flagged as path escapes (#562).
///
/// ```text
/// git commit -m "allow /tmp and /dev/*"  →  git commit -m "                    "
/// echo 'fixed /etc/hosts'               →  echo '                '
/// ```
fn strip_quoted_strings(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\'' || c == '"' {
            result.push(c); // keep the opening quote
            // Replace everything until the matching close quote with spaces
            let mut found_close = false;
            for inner in chars.by_ref() {
                if inner == c {
                    result.push(c); // keep the closing quote
                    found_close = true;
                    break;
                }
                result.push(' ');
            }
            if !found_close {
                // Unterminated quote — already replaced content, just continue
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Extract the target of a `cd` command from a segment.
fn extract_cd_target(segment: &str) -> Option<CdTarget> {
    let seg = segment.trim();
    let seg = strip_env_vars(seg);
    let seg = seg.trim();

    if seg == "cd" {
        return Some(CdTarget::Home);
    }
    if !seg.starts_with("cd ") && !seg.starts_with("cd\t") {
        return None;
    }

    let target = seg[2..].trim();

    if target.is_empty() || target == "~" {
        return Some(CdTarget::Home);
    }
    if target.starts_with('$') || target.starts_with('`') || target.contains("$(") {
        return Some(CdTarget::Dynamic);
    }

    Some(CdTarget::Path(
        target
            .split_whitespace()
            .next()
            .unwrap_or(target)
            .to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> std::path::PathBuf {
        std::path::PathBuf::from("/home/user/project")
    }

    #[test]
    fn test_lint_safe_command() {
        let lint = lint_bash_paths("cargo test", &project());
        assert!(!lint.has_warnings());
    }

    #[test]
    fn test_lint_cd_inside_project() {
        let lint = lint_bash_paths("cd src && ls", &project());
        assert!(!lint.has_warnings());
    }

    #[test]
    fn test_lint_cd_outside_project() {
        let lint = lint_bash_paths("cd /etc && ls", &project());
        assert!(lint.has_warnings());
        assert!(lint.outside_paths.contains(&"/etc".to_string()));
    }

    #[test]
    fn test_lint_cd_home() {
        let lint = lint_bash_paths("cd ~", &project());
        assert!(lint.home_escape);
    }

    #[test]
    fn test_lint_bare_cd() {
        let lint = lint_bash_paths("cd", &project());
        assert!(lint.home_escape);
    }

    #[test]
    fn test_lint_cd_dynamic_ignored() {
        let lint = lint_bash_paths("cd $SOME_DIR", &project());
        assert!(!lint.has_warnings());
    }

    #[test]
    fn test_lint_absolute_path_arg() {
        let lint = lint_bash_paths("cp file.txt /etc/hosts", &project());
        assert!(lint.has_warnings());
        assert!(lint.outside_paths.contains(&"/etc/hosts".to_string()));
    }

    #[test]
    fn test_lint_relative_escape() {
        let lint = lint_bash_paths("cat ../../../etc/passwd", &project());
        assert!(lint.has_warnings());
    }

    #[test]
    fn test_lint_relative_inside() {
        let lint = lint_bash_paths("cat ../project/src/main.rs", &project());
        assert!(!lint.has_warnings());
    }

    #[test]
    fn test_lint_path_inside_project_absolute() {
        let lint = lint_bash_paths("ls /home/user/project/src", &project());
        assert!(!lint.has_warnings());
    }

    #[test]
    fn test_lint_empty_command() {
        let lint = lint_bash_paths("", &project());
        assert!(!lint.has_warnings());
    }

    #[test]
    fn test_lint_deduplicates() {
        let lint = lint_bash_paths("cp /etc/a /etc/b", &project());
        assert!(lint.has_warnings());
        assert_eq!(lint.outside_paths.len(), 2);
    }

    // ── Temp path allowlist (#560) ──

    #[test]
    fn test_lint_tmp_path_allowed() {
        let lint = lint_bash_paths("cat /tmp/issue-draft.md", &project());
        assert!(!lint.has_warnings());
    }

    #[test]
    fn test_lint_cd_tmp_allowed() {
        let lint = lint_bash_paths("cd /tmp && ls", &project());
        assert!(!lint.has_warnings());
    }

    #[test]
    fn test_lint_tmp_subdir_allowed() {
        let lint = lint_bash_paths("cp file.txt /tmp/koda/output.md", &project());
        assert!(!lint.has_warnings());
    }

    #[test]
    fn test_lint_dev_null_allowed() {
        let lint = lint_bash_paths("echo test > /dev/null", &project());
        assert!(!lint.has_warnings());
    }

    #[test]
    fn test_lint_etc_still_blocked() {
        let lint = lint_bash_paths("cat /etc/passwd", &project());
        assert!(lint.has_warnings());
    }

    // ── Quote-aware path lint (#562) ──

    #[test]
    fn test_lint_path_in_commit_message_ignored() {
        let lint = lint_bash_paths(
            r#"git commit -m "allow /tmp and /dev/* and /etc/hosts""#,
            &project(),
        );
        assert!(!lint.has_warnings());
    }

    #[test]
    fn test_lint_path_in_single_quotes_ignored() {
        let lint = lint_bash_paths("echo 'fixed /etc/hosts parsing'", &project());
        assert!(!lint.has_warnings());
    }

    #[test]
    fn test_lint_path_outside_quotes_still_flagged() {
        let lint = lint_bash_paths(r#"cp /etc/hosts "destination.txt""#, &project());
        assert!(lint.has_warnings());
        assert!(lint.outside_paths.contains(&"/etc/hosts".to_string()));
    }

    #[test]
    fn test_lint_merge_with_message() {
        let lint = lint_bash_paths(
            r#"git merge fix/branch -m "feat: allow /tmp, $TMPDIR, and /dev/* (#560)""#,
            &project(),
        );
        assert!(!lint.has_warnings());
    }

    #[test]
    fn test_strip_quoted_strings() {
        assert_eq!(
            strip_quoted_strings(r#"git commit -m "allow /tmp""#),
            r#"git commit -m "          ""#
        );
        assert_eq!(
            strip_quoted_strings("echo 'path /etc/hosts'"),
            "echo '               '"
        );
        // Unquoted content preserved
        assert_eq!(strip_quoted_strings("cp /etc/a /etc/b"), "cp /etc/a /etc/b");
    }
}
