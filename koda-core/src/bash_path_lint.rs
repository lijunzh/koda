//! Bash path lint: detect commands that escape the project root.
//!
//! Heuristic analysis — catches common accidental escapes, not adversarial inputs.
//! Dynamic targets (`cd $VAR`, `cd $(cmd)`) are intentionally ignored.

use path_clean::PathClean;
use std::path::Path;

use crate::bash_safety::split_command_segments;
use crate::bash_safety::strip_env_vars;

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
                    if !resolved.starts_with(project_root) {
                        lint.outside_paths.push(p);
                    }
                }
            }
        }

        // Check for absolute path arguments (not cd)
        for token in seg.split_whitespace().skip(1) {
            if token.starts_with('-') {
                continue;
            }
            if token.starts_with('/') {
                let resolved = Path::new(token).to_path_buf().clean();
                if !resolved.starts_with(project_root) {
                    lint.outside_paths.push(token.to_string());
                }
            }
            if token.contains("..") {
                let resolved = project_root.join(token).clean();
                if !resolved.starts_with(project_root) {
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
        let lint = lint_bash_paths("cd /tmp && ls", &project());
        assert!(lint.has_warnings());
        assert!(lint.outside_paths.contains(&"/tmp".to_string()));
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
        let lint = lint_bash_paths("cp /tmp/a /tmp/b", &project());
        assert!(lint.has_warnings());
        assert_eq!(lint.outside_paths.len(), 2);
    }
}
