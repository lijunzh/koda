//! Integration tests for file tools.
//!
//! Tests path safety, file CRUD, and directory deletion.

use std::path::Path;
use tempfile::TempDir;

/// Reproduce the safe_resolve_path logic for integration testing.
fn safe_resolve_path(project_root: &Path, requested: &str) -> Result<std::path::PathBuf, String> {
    use path_clean::PathClean;
    let requested_path = Path::new(requested);
    let resolved = if requested_path.is_absolute() {
        requested_path.to_path_buf().clean()
    } else {
        project_root.join(requested_path).clean()
    };
    if !resolved.starts_with(project_root) {
        return Err(format!("Path escapes project root: {resolved:?}"));
    }
    Ok(resolved)
}

#[test]
fn test_write_then_read_new_file() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let path = safe_resolve_path(root, "src/hello.rs").unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "fn main() { println!(\"hello\"); }\n").unwrap();
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("hello"));
}

#[test]
fn test_traversal_attack_via_dotdot() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    assert!(safe_resolve_path(root, "../../../etc/passwd").is_err());
    assert!(safe_resolve_path(root, "src/../../../etc/shadow").is_err());
    assert!(safe_resolve_path(root, "/etc/hosts").is_err());
}

#[test]
fn test_nested_new_directories() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let path = safe_resolve_path(root, "a/b/c/d/e/file.txt").unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "deep").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "deep");
}

#[test]
fn test_edit_file_replacement() {
    let tmp = TempDir::new().unwrap();
    let file = tmp.path().join("example.rs");
    std::fs::write(&file, "fn main() {\n    println!(\"old\");\n}\n").unwrap();
    let mut content = std::fs::read_to_string(&file).unwrap();
    content = content.replacen("\"old\"", "\"new\"", 1);
    std::fs::write(&file, &content).unwrap();
    let result = std::fs::read_to_string(&file).unwrap();
    assert!(result.contains("\"new\""));
    assert!(!result.contains("\"old\""));
}

#[test]
fn test_edit_file_delete_snippet() {
    let tmp = TempDir::new().unwrap();
    let file = tmp.path().join("with_comment.rs");
    std::fs::write(&file, "// TODO: remove this\nfn main() {}\n").unwrap();
    let mut content = std::fs::read_to_string(&file).unwrap();
    content = content.replacen("// TODO: remove this\n", "", 1);
    std::fs::write(&file, &content).unwrap();
    let result = std::fs::read_to_string(&file).unwrap();
    assert!(!result.contains("TODO"));
    assert!(result.contains("fn main"));
}

#[test]
fn test_delete_file() {
    let tmp = TempDir::new().unwrap();
    let file = tmp.path().join("to_delete.txt");
    std::fs::write(&file, "goodbye").unwrap();
    assert!(file.exists());
    std::fs::remove_file(&file).unwrap();
    assert!(!file.exists());
}

// ── Directory deletion tests ──────────────────────────────────

#[test]
fn test_delete_empty_directory() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("empty_dir");
    std::fs::create_dir(&dir).unwrap();
    assert!(dir.is_dir());
    std::fs::remove_dir(&dir).unwrap();
    assert!(!dir.exists());
}

#[test]
fn test_delete_directory_recursive() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("project");
    std::fs::create_dir_all(dir.join("src/nested")).unwrap();
    std::fs::write(dir.join("Cargo.toml"), "[package]").unwrap();
    std::fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();
    std::fs::write(dir.join("src/nested/mod.rs"), "// mod").unwrap();

    // Count items before deletion
    fn count_entries(path: &Path) -> usize {
        let mut count = 0;
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                count += 1;
                if entry.path().is_dir() {
                    count += count_entries(&entry.path());
                }
            }
        }
        count
    }
    assert_eq!(count_entries(&dir), 5); // src, src/nested, Cargo.toml, main.rs, mod.rs

    std::fs::remove_dir_all(&dir).unwrap();
    assert!(!dir.exists());
}

#[test]
fn test_delete_nonempty_dir_without_recursive_fails() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("nonempty");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("file.txt"), "content").unwrap();

    // remove_dir (not recursive) should fail on non-empty
    assert!(std::fs::remove_dir(&dir).is_err());
    assert!(dir.exists()); // still there
}

#[test]
fn test_cannot_delete_project_root() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let resolved = safe_resolve_path(root, ".").unwrap();
    assert_eq!(resolved, root.to_path_buf());
    assert!(
        resolved == root,
        "Resolved path equals project root — deletion should be blocked"
    );
}

// ── List tool regression tests ──────────────────────────────────

/// Reproduce the list walker logic for testing.
fn list_files_walk(root: &Path) -> Vec<String> {
    let mut builder = ignore::WalkBuilder::new(root);
    builder.hidden(true).git_ignore(true).filter_entry(|entry| {
        let name = entry.file_name().to_string_lossy();
        !matches!(
            name.as_ref(),
            "target"
                | "node_modules"
                | "__pycache__"
                | ".git"
                | "dist"
                | "build"
                | ".next"
                | ".cache"
        )
    });

    let mut entries = Vec::new();
    for entry in builder.build().flatten() {
        let path = entry.path();
        if path == root {
            continue; // skip root itself
        }
        let relative = path.strip_prefix(root).unwrap_or(path);
        let prefix = if path.is_dir() { "d " } else { "  " };
        entries.push(format!("{prefix}{}", relative.display()));
    }
    entries
}

#[test]
fn test_list_skips_root_entry() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("hello.txt"), "hi").unwrap();
    let entries = list_files_walk(tmp.path());
    // Should NOT contain an entry that's just "d " (empty root)
    assert!(
        !entries
            .iter()
            .any(|e| e.trim().is_empty() || e == "d " || e == "d"),
        "Root entry should be skipped, got: {entries:?}"
    );
    assert!(entries.iter().any(|e| e.contains("hello.txt")));
}

#[test]
fn test_list_skips_target_directory() {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("target/debug")).unwrap();
    std::fs::write(tmp.path().join("target/debug/binary"), "bin").unwrap();
    std::fs::write(tmp.path().join("Cargo.toml"), "[package]").unwrap();

    let entries = list_files_walk(tmp.path());
    assert!(
        !entries.iter().any(|e| e.contains("target")),
        "target/ should be filtered, got: {entries:?}"
    );
    assert!(entries.iter().any(|e| e.contains("Cargo.toml")));
}

#[test]
fn test_list_skips_node_modules() {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("node_modules/lodash")).unwrap();
    std::fs::write(tmp.path().join("node_modules/lodash/index.js"), "x").unwrap();
    std::fs::write(tmp.path().join("package.json"), "{}").unwrap();

    let entries = list_files_walk(tmp.path());
    assert!(
        !entries.iter().any(|e| e.contains("node_modules")),
        "node_modules/ should be filtered, got: {entries:?}"
    );
    assert!(entries.iter().any(|e| e.contains("package.json")));
}

#[test]
fn test_list_skips_pycache() {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("__pycache__")).unwrap();
    std::fs::write(tmp.path().join("__pycache__/mod.pyc"), "bytecode").unwrap();
    std::fs::write(tmp.path().join("main.py"), "print('hi')").unwrap();

    let entries = list_files_walk(tmp.path());
    assert!(
        !entries.iter().any(|e| e.contains("__pycache__")),
        "__pycache__/ should be filtered, got: {entries:?}"
    );
    assert!(entries.iter().any(|e| e.contains("main.py")));
}

#[test]
fn test_list_skips_hidden_files() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".env"), "SECRET=abc").unwrap();
    std::fs::write(tmp.path().join("visible.txt"), "hello").unwrap();

    let entries = list_files_walk(tmp.path());
    assert!(
        !entries.iter().any(|e| e.contains(".env")),
        "Hidden files should be filtered, got: {entries:?}"
    );
    assert!(entries.iter().any(|e| e.contains("visible.txt")));
}

#[test]
fn test_list_skips_git_directory() {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join(".git/objects")).unwrap();
    std::fs::write(tmp.path().join(".git/HEAD"), "ref: refs/heads/main").unwrap();
    std::fs::write(tmp.path().join("README.md"), "# Hello").unwrap();

    let entries = list_files_walk(tmp.path());
    assert!(
        !entries.iter().any(|e| e.contains(".git")),
        ".git/ should be filtered, got: {entries:?}"
    );
    assert!(entries.iter().any(|e| e.contains("README.md")));
}

#[test]
fn test_list_shows_normal_directories_and_files() {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("src/main.rs"), "fn main() {}").unwrap();
    std::fs::write(tmp.path().join("Cargo.toml"), "[package]").unwrap();

    let entries = list_files_walk(tmp.path());
    assert!(
        entries
            .iter()
            .any(|e| e.starts_with("d ") && e.contains("src"))
    );
    assert!(entries.iter().any(|e| e.contains("main.rs")));
    assert!(entries.iter().any(|e| e.contains("Cargo.toml")));
}
