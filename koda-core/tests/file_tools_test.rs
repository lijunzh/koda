//! Integration tests for file tools.
//!
//! Tests path safety, file CRUD, and directory listing.

use koda_core::tools::FileReadCache;
use koda_core::tools::file_tools;
use koda_core::tools::safe_resolve_path;
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

/// Convenience: fresh empty read cache for tests that don't need pre-seeded state.
fn empty_cache() -> FileReadCache {
    Arc::new(Mutex::new(HashMap::new()))
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

#[tokio::test]
async fn test_edit_file_replacement() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(
        tmp.path().join("example.rs"),
        "fn main() {\n    println!(\"old\");\n}\n",
    )
    .unwrap();
    let args = json!({
        "path": "example.rs",
        "replacements": [{"old_str": "\"old\"", "new_str": "\"new\""}]
    });
    file_tools::edit_file(tmp.path(), &args, &empty_cache())
        .await
        .unwrap();
    let result = std::fs::read_to_string(tmp.path().join("example.rs")).unwrap();
    assert!(result.contains("\"new\""));
    assert!(!result.contains("\"old\""));
}

#[tokio::test]
async fn test_edit_file_delete_snippet() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(
        tmp.path().join("with_comment.rs"),
        "// TODO: remove this\nfn main() {}\n",
    )
    .unwrap();
    let args = json!({
        "path": "with_comment.rs",
        "replacements": [{"old_str": "// TODO: remove this\n", "new_str": ""}]
    });
    file_tools::edit_file(tmp.path(), &args, &empty_cache())
        .await
        .unwrap();
    let result = std::fs::read_to_string(tmp.path().join("with_comment.rs")).unwrap();
    assert!(!result.contains("TODO"));
    assert!(result.contains("fn main"));
}

#[tokio::test]
async fn test_edit_replace_all_replaces_every_occurrence() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(
        tmp.path().join("vars.rs"),
        "let foo = 1;\nlet foo = 2;\nlet foo = 3;\n",
    )
    .unwrap();
    let args = json!({
        "path": "vars.rs",
        "replacements": [{"old_str": "foo", "new_str": "bar", "replace_all": true}]
    });
    let result = file_tools::edit_file(tmp.path(), &args, &empty_cache())
        .await
        .unwrap();
    let content = std::fs::read_to_string(tmp.path().join("vars.rs")).unwrap();
    assert!(
        !content.contains("foo"),
        "all occurrences should be replaced"
    );
    assert_eq!(content.matches("bar").count(), 3);
    assert!(result.contains("3 occurrences replaced"), "{result}");
}

#[tokio::test]
/// Multi-match without replace_all must error with line numbers, not silently
/// replace the first occurrence (changed in #814).
async fn test_edit_without_replace_all_errors_on_multi_match() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("vars.rs"), "let foo = 1;\nlet foo = 2;\n").unwrap();
    let args = json!({
        "path": "vars.rs",
        "replacements": [{"old_str": "foo", "new_str": "bar"}]
    });
    let err = file_tools::edit_file(tmp.path(), &args, &empty_cache())
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("matches 2 times"), "expected count: {msg}");
    assert!(msg.contains("lines"), "expected line numbers: {msg}");
    // File must be untouched
    let content = std::fs::read_to_string(tmp.path().join("vars.rs")).unwrap();
    assert_eq!(content, "let foo = 1;\nlet foo = 2;\n");
}

#[tokio::test]
async fn test_edit_replace_all_single_occurrence_no_count_label() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("one.rs"), "let x = old;\n").unwrap();
    let args = json!({
        "path": "one.rs",
        "replacements": [{"old_str": "old", "new_str": "new", "replace_all": true}]
    });
    let result = file_tools::edit_file(tmp.path(), &args, &empty_cache())
        .await
        .unwrap();
    let content = std::fs::read_to_string(tmp.path().join("one.rs")).unwrap();
    assert!(content.contains("new"));
    // Single occurrence: no "(N occurrences replaced)" label needed
    assert!(!result.contains("occurrences replaced"), "{result}");
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
    assert_eq!(count_entries(&dir), 5);

    std::fs::remove_dir_all(&dir).unwrap();
    assert!(!dir.exists());
}

#[test]
fn test_delete_nonempty_dir_without_recursive_fails() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("nonempty");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("file.txt"), "content").unwrap();
    assert!(std::fs::remove_dir(&dir).is_err());
    assert!(dir.exists());
}

#[test]
fn test_cannot_delete_project_root() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let resolved = safe_resolve_path(root, ".").unwrap();
    assert_eq!(resolved, root.to_path_buf());
}
