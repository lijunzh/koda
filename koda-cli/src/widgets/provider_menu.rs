//! Provider picker dropdown — thin wrapper around the generic dropdown.
//!
//! Appears when the user types `/provider` with no args.
//! After selection, the provider setup flow continues with API key input.

use super::dropdown::DropdownItem;

/// A provider item for the dropdown.
#[derive(Clone, Debug)]
pub struct ProviderItem {
    /// Internal key (e.g. "anthropic", "openai").
    pub key: &'static str,
    /// Display name (e.g. "Anthropic", "OpenAI").
    pub name: &'static str,
    /// Short description (e.g. "Claude Sonnet, Opus").
    pub description: &'static str,
    /// Whether this is the currently active provider.
    pub is_current: bool,
}

impl DropdownItem for ProviderItem {
    fn label(&self) -> &str {
        self.name
    }
    fn description(&self) -> String {
        let mut desc = self.description.to_string();
        if self.is_current {
            desc.push_str(" \u{25c0} current");
        }
        desc
    }
    fn matches_filter(&self, filter: &str) -> bool {
        let f = filter.to_lowercase();
        self.name.to_lowercase().contains(&f)
            || self.key.to_lowercase().contains(&f)
            || self.description.to_lowercase().contains(&f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_shows_marker() {
        let item = ProviderItem {
            key: "anthropic",
            name: "Anthropic",
            description: "Claude Sonnet, Opus",
            is_current: true,
        };
        assert!(item.description().contains('\u{25c0}'));
    }

    #[test]
    fn filter_matches_key_name_desc() {
        let item = ProviderItem {
            key: "anthropic",
            name: "Anthropic",
            description: "Claude Sonnet, Opus",
            is_current: false,
        };
        assert!(item.matches_filter("anth"));
        assert!(item.matches_filter("Claude"));
        assert!(item.matches_filter("opus"));
        assert!(!item.matches_filter("gemini"));
    }
}
