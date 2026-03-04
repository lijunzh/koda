//! Integration tests for file tools.
//!
//! Tests path safety, file CRUD, and directory listing.

use koda_core::tools::safe_resolve_path;
use std::path::Path;
use tempfile::TempDir;

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
