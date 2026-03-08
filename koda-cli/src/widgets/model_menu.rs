//! Model picker dropdown — thin wrapper around the generic dropdown.
//!
//! Appears when the user types `/model` with no args.

use super::dropdown::DropdownItem;

/// A model item for the dropdown.
#[derive(Clone, Debug)]
pub struct ModelItem {
    pub id: String,
    pub is_current: bool,
}

impl DropdownItem for ModelItem {
    fn label(&self) -> &str {
        &self.id
    }
    fn description(&self) -> String {
        if self.is_current {
            "\u{25c0} current".to_string()
        } else {
            String::new()
        }
    }
    fn matches_filter(&self, filter: &str) -> bool {
        let lower = self.id.to_lowercase();
        let filter_lower = filter.to_lowercase();
        lower.contains(&filter_lower)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_model_shows_marker() {
        let item = ModelItem {
            id: "gpt-4o".into(),
            is_current: true,
        };
        assert!(item.description().contains('\u{25c0}'));
    }

    #[test]
    fn non_current_no_description() {
        let item = ModelItem {
            id: "gpt-4o".into(),
            is_current: false,
        };
        assert!(item.description().is_empty());
    }

    #[test]
    fn filter_case_insensitive() {
        let item = ModelItem {
            id: "claude-sonnet-4-20250514".into(),
            is_current: false,
        };
        assert!(item.matches_filter("sonnet"));
        assert!(item.matches_filter("SONNET"));
        assert!(item.matches_filter("Claude"));
        assert!(!item.matches_filter("opus"));
    }
}
