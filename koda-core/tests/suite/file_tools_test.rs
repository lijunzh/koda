//! Integration tests for file tools.
//!
//! Tests path safety, file CRUD, and directory listing.

use koda_core::tools::FileReadCache;
use koda_core::tools::file_tools;
use koda_core::tools::safe_resolve_path;
use koda_sandbox::fs::LocalFileSystem;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

/// Convenience: fresh empty read cache for tests that don't need pre-seeded state.
fn empty_cache() -> FileReadCache {
    Arc::new(Mutex::new(HashMap::new()))
}

fn fs() -> LocalFileSystem {
    LocalFileSystem::new()
}

#[test]
fn test_traversal_attack_via_dotdot() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    assert!(safe_resolve_path(root, "../../../etc/passwd").is_err());
    assert!(safe_resolve_path(root, "src/../../../etc/shadow").is_err());
    assert!(safe_resolve_path(root, "/etc/hosts").is_err());
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
        "old_str": "\"old\"", "new_str": "\"new\""
    });
    file_tools::edit_file(tmp.path(), &args, &empty_cache(), &fs())
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
        "old_str": "// TODO: remove this\n", "new_str": ""
    });
    file_tools::edit_file(tmp.path(), &args, &empty_cache(), &fs())
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
        "old_str": "foo", "new_str": "bar", "replace_all": true
    });
    let result = file_tools::edit_file(tmp.path(), &args, &empty_cache(), &fs())
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
        "old_str": "foo", "new_str": "bar"
    });
    let err = file_tools::edit_file(tmp.path(), &args, &empty_cache(), &fs())
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
        "old_str": "old", "new_str": "new", "replace_all": true
    });
    let result = file_tools::edit_file(tmp.path(), &args, &empty_cache(), &fs())
        .await
        .unwrap();
    let content = std::fs::read_to_string(tmp.path().join("one.rs")).unwrap();
    assert!(content.contains("new"));
    // Single occurrence: no "(N occurrences replaced)" label needed
    assert!(!result.contains("occurrences replaced"), "{result}");
}
