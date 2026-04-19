//! Post-edit syntax verification (#467).
//!
//! Designed to be fast, non-blocking, and zero-config: parses a single
//! file with the appropriate tree-sitter grammar and reports any ERROR
//! or MISSING nodes. Returns `None` for valid files or unsupported
//! extensions so callers can skip silently.

use std::path::Path;

use crate::grammar::get_language;

/// Quick syntax check — parse a file and report any syntax errors.
///
/// Returns `None` if the file is syntactically valid or has an unsupported
/// extension. Returns `Some(description)` with error locations if the
/// tree-sitter parse tree contains ERROR or MISSING nodes.
pub fn syntax_check(file_path: &Path) -> Option<String> {
    let extension = file_path.extension().and_then(|s| s.to_str()).unwrap_or("");
    let language = get_language(extension).ok()?;
    let source_code = std::fs::read_to_string(file_path).ok()?;

    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).ok()?;
    let tree = parser.parse(&source_code, None)?;

    if !tree.root_node().has_error() {
        return None;
    }

    let errors = collect_syntax_errors(&tree, source_code.as_bytes());
    if errors.is_empty() {
        return None;
    }
    Some(errors)
}

/// Walk the parse tree and collect ERROR/MISSING node locations.
fn collect_syntax_errors(tree: &tree_sitter::Tree, source: &[u8]) -> String {
    let mut errors = Vec::new();
    let mut cursor = tree.root_node().walk();
    walk_errors(&mut cursor, source, &mut errors);

    // Cap at 5 errors to avoid flooding the LLM context
    let total = errors.len();
    errors.truncate(5);
    let mut out = format!("⚠ Syntax errors ({total}):\n");
    for e in &errors {
        out.push_str(e);
        out.push('\n');
    }
    if total > 5 {
        out.push_str(&format!("  ...and {} more\n", total - 5));
    }
    out
}

/// Recursively collect error descriptions from the parse tree.
fn walk_errors(cursor: &mut tree_sitter::TreeCursor, source: &[u8], errors: &mut Vec<String>) {
    loop {
        let node = cursor.node();
        if node.is_error() || node.is_missing() {
            let start = node.start_position();
            let line = start.row + 1;
            let col = start.column + 1;
            // Grab the snippet around the error (up to 60 chars)
            let snippet: String = node
                .utf8_text(source)
                .unwrap_or("")
                .chars()
                .take(60)
                .collect();
            let kind = if node.is_missing() {
                format!("missing {}", node.kind())
            } else {
                "syntax error".to_string()
            };
            errors.push(format!("  line {line}:{col}: {kind}: `{snippet}`"));
        } else if node.has_error() && cursor.goto_first_child() {
            // Recurse into children only if this subtree has errors
            walk_errors(cursor, source, errors);
            cursor.goto_parent();
        }

        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_syntax_check_valid_rust() {
        let mut tmp = tempfile::NamedTempFile::with_suffix(".rs").unwrap();
        write!(tmp, "fn main() {{ println!(\"hello\"); }}").unwrap();
        assert!(syntax_check(tmp.path()).is_none());
    }

    #[test]
    fn test_syntax_check_invalid_rust() {
        let mut tmp = tempfile::NamedTempFile::with_suffix(".rs").unwrap();
        write!(tmp, "fn main() {{ let x = ; }}").unwrap();
        let err = syntax_check(tmp.path()).expect("should report error");
        assert!(err.contains("syntax error"), "got: {err}");
        assert!(err.contains("line"), "got: {err}");
    }

    #[test]
    fn test_syntax_check_valid_python() {
        let mut tmp = tempfile::NamedTempFile::with_suffix(".py").unwrap();
        write!(tmp, "def hello():\n    return 42\n").unwrap();
        assert!(syntax_check(tmp.path()).is_none());
    }

    #[test]
    fn test_syntax_check_invalid_python() {
        let mut tmp = tempfile::NamedTempFile::with_suffix(".py").unwrap();
        write!(tmp, "def hello(\n    return 42\n").unwrap();
        let err = syntax_check(tmp.path()).expect("should report error");
        assert!(err.contains("line"), "got: {err}");
    }

    #[test]
    fn test_syntax_check_unsupported_extension() {
        let mut tmp = tempfile::NamedTempFile::with_suffix(".xyz").unwrap();
        write!(tmp, "not a real language").unwrap();
        assert!(syntax_check(tmp.path()).is_none());
    }

    #[test]
    fn test_syntax_check_nonexistent_file() {
        assert!(syntax_check(Path::new("/tmp/does_not_exist_467.rs")).is_none());
    }

    #[test]
    fn test_syntax_check_valid_typescript() {
        let mut tmp = tempfile::NamedTempFile::with_suffix(".ts").unwrap();
        write!(tmp, "const x: number = 42;").unwrap();
        assert!(syntax_check(tmp.path()).is_none());
    }

    #[test]
    fn test_syntax_check_invalid_typescript() {
        let mut tmp = tempfile::NamedTempFile::with_suffix(".ts").unwrap();
        write!(tmp, "const x: number = ;").unwrap();
        let err = syntax_check(tmp.path()).expect("should report error");
        assert!(err.contains("line"), "got: {err}");
    }
}
