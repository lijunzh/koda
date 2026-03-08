//! Session picker dropdown — thin wrapper around the generic dropdown.
//!
//! Appears when the user types `/sessions` with no args.

use super::dropdown::DropdownItem;

/// A session item for the dropdown.
#[derive(Clone, Debug)]
pub struct SessionItem {
    pub id: String,
    pub short_id: String,
    pub created_at: String,
    pub message_count: i64,
    pub total_tokens: i64,
    pub is_current: bool,
}

impl DropdownItem for SessionItem {
    fn label(&self) -> &str {
        &self.short_id
    }
    fn description(&self) -> String {
        let mut desc = format!(
            "{}  {} msgs  {}k tok",
            self.created_at,
            self.message_count,
            self.total_tokens / 1000
        );
        if self.is_current {
            desc.push_str(" \u{25c0} current");
        }
        desc
    }
    fn matches_filter(&self, filter: &str) -> bool {
        let f = filter.to_lowercase();
        self.id.to_lowercase().contains(&f) || self.created_at.to_lowercase().contains(&f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_shows_marker() {
        let item = SessionItem {
            id: "abc12345".into(),
            short_id: "abc12345".into(),
            created_at: "2026-03-08".into(),
            message_count: 5,
            total_tokens: 12000,
            is_current: true,
        };
        assert!(item.description().contains('\u{25c0}'));
    }

    #[test]
    fn filter_by_id_and_date() {
        let item = SessionItem {
            id: "abc12345".into(),
            short_id: "abc12345".into(),
            created_at: "2026-03-08".into(),
            message_count: 5,
            total_tokens: 12000,
            is_current: false,
        };
        assert!(item.matches_filter("abc"));
        assert!(item.matches_filter("2026"));
        assert!(!item.matches_filter("xyz"));
    }
}
