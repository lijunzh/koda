//! File and skill picker dropdown — thin wrapper around the generic dropdown.
//!
//! Appears when the user types `@` (skills, then files, as Codex's `@`
//! lists both) or `$` (skills) in the input. Filters live as the user
//! continues typing.

use super::dropdown::DropdownItem;

/// A file path or skill item for the dropdown.
#[derive(Clone, Debug)]
pub struct FileItem {
    /// Relative path (e.g. "src/main.rs" or "koda-cli/"), or `$name` for a
    /// skill.
    pub path: String,
    /// Whether this is a directory.
    pub is_dir: bool,
    /// A skill's description, when the item is a skill.
    pub skill: Option<String>,
    /// What an MCP resource is, when the item is one (`path` is then
    /// `server:uri`).
    pub resource: Option<String>,
}

impl FileItem {
    /// A skill, shown as `$name` with its description.
    pub fn skill(name: &str, description: &str) -> Self {
        Self {
            path: format!("${name}"),
            is_dir: false,
            skill: Some(description.to_string()),
            resource: None,
        }
    }

    /// An MCP resource, `server:uri`, with what it is.
    pub fn resource(mention: &str, about: &str) -> Self {
        Self {
            path: mention.to_string(),
            is_dir: false,
            skill: None,
            resource: Some(about.to_string()),
        }
    }
}

impl DropdownItem for FileItem {
    fn label(&self) -> &str {
        &self.path
    }
    fn description(&self) -> String {
        if let Some(description) = &self.skill {
            return format!("\u{1f4da} {description}");
        }
        if let Some(about) = &self.resource {
            return format!("\u{1f50c} {about}");
        }
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
            skill: None,
            resource: None,
        };
        assert!(!item.description().is_empty());
    }

    #[test]
    fn file_no_description() {
        let item = FileItem {
            path: "main.rs".into(),
            is_dir: false,
            skill: None,
            resource: None,
        };
        assert!(item.description().is_empty());
    }
}
