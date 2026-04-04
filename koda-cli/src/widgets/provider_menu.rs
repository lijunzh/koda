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
    /// Whether this is a local provider (no API key needed).
    pub local: bool,
    /// Whether this is the currently active provider.
    pub is_current: bool,
}

impl DropdownItem for ProviderItem {
    fn label(&self) -> &str {
        self.name
    }
    fn description(&self) -> String {
        let mut desc = if self.local {
            "Local, no API key".to_string()
        } else {
            String::new()
        };
        if self.is_current {
            if !desc.is_empty() {
                desc.push(' ');
            }
            desc.push_str("\u{25c0} current");
        }
        desc
    }
    fn matches_filter(&self, filter: &str) -> bool {
        let f = filter.to_lowercase();
        self.name.to_lowercase().contains(&f)
            || self.key.to_lowercase().contains(&f)
            || (self.local && "local".contains(&f))
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
            local: false,
            is_current: true,
        };
        assert!(item.description().contains('\u{25c0}'));
    }

    #[test]
    fn local_shows_description() {
        let item = ProviderItem {
            key: "ollama",
            name: "Ollama",
            local: true,
            is_current: false,
        };
        assert!(item.description().contains("Local, no API key"));
    }

    #[test]
    fn filter_matches_key_name_local() {
        let item = ProviderItem {
            key: "lmstudio",
            name: "LM Studio",
            local: true,
            is_current: false,
        };
        assert!(item.matches_filter("lm"));
        assert!(item.matches_filter("Studio"));
        assert!(item.matches_filter("local"));
        assert!(!item.matches_filter("gemini"));
    }
}
