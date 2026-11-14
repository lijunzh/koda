//! Session picker dropdown — thin wrapper around the generic dropdown.
//!
//! Appears when the user types `/sessions` with no args.

use super::dropdown::DropdownItem;

/// A session item for the dropdown: its title, or first prompt, and when it
/// was last used, as Codex's session picker shows them.
#[derive(Clone, Debug)]
pub struct SessionItem {
    pub id: String,
    pub short_id: String,
    /// When its last message was stored, formatted.
    pub updated: String,
    pub is_current: bool,
    pub title: Option<String>,
    /// The first prompt, for a session without a title.
    pub preview: String,
}

impl DropdownItem for SessionItem {
    fn label(&self) -> &str {
        &self.short_id
    }
    fn description(&self) -> String {
        let name = self.title.as_deref().unwrap_or(&self.preview);
        let name: String = name.chars().take(30).collect();
        let mut desc = format!("{name}  {}", self.updated);
        if self.is_current {
            desc.push_str(" \u{25c0} current");
        }
        desc
    }
    fn matches_filter(&self, filter: &str) -> bool {
        let f = filter.to_lowercase();
        self.id.to_lowercase().contains(&f)
            || self.updated.to_lowercase().contains(&f)
            || self.preview.to_lowercase().contains(&f)
            || self
                .title
                .as_deref()
                .is_some_and(|t| t.to_lowercase().contains(&f))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: Option<&str>, current: bool) -> SessionItem {
        SessionItem {
            id: "abc12345".into(),
            short_id: "abc12345".into(),
            updated: "2026-03-08 14:30".into(),
            is_current: current,
            title: title.map(str::to_string),
            preview: "fix the login bug".into(),
        }
    }

    #[test]
    fn current_shows_marker() {
        assert!(item(None, true).description().contains('\u{25c0}'));
    }

    #[test]
    fn a_session_without_a_title_shows_its_first_prompt() {
        let desc = item(None, false).description();
        assert!(desc.starts_with("fix the login bug"), "{desc}");
        assert!(desc.contains("2026-03-08 14:30"), "{desc}");
    }

    #[test]
    fn filter_by_id_date_title_and_prompt() {
        let item = item(Some("refactor auth module"), false);
        assert!(item.matches_filter("abc"));
        assert!(item.matches_filter("2026"));
        assert!(item.matches_filter("refactor"));
        assert!(item.matches_filter("login"));
        assert!(!item.matches_filter("xyz"));
    }
}
