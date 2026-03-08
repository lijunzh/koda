//! File picker dropdown — thin wrapper around the generic dropdown.
//!
//! Appears when the user types `@` in the input. Filters live as
//! the user continues typing the path.

use super::dropdown::DropdownItem;

/// A file path item for the dropdown.
#[derive(Clone, Debug)]
pub struct FileItem {
    /// Relative path (e.g. "src/main.rs" or "koda-cli/").
    pub path: String,
    /// Whether this is a directory.
    pub is_dir: bool,
}

impl DropdownItem for FileItem {
    fn label(&self) -> &str {
        &self.path
    }
    fn description(&self) -> String {
        if self.is_dir {
            "\u{1f4c1}".to_string() // 📁
        } else {
            String::new()
        }
    }
    fn matches_filter(&self, _filter: &str) -> bool {
        // Filtering is done externally via list_path_matches
        // (fuzzy scoring). All items in the dropdown already match.
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_shows_icon() {
        let item = FileItem {
            path: "src/".into(),
            is_dir: true,
        };
        assert!(!item.description().is_empty());
    }

    #[test]
    fn file_no_description() {
        let item = FileItem {
            path: "main.rs".into(),
            is_dir: false,
        };
        assert!(item.description().is_empty());
    }
}
