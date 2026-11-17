//! Provider picker dropdown — thin wrapper around the generic dropdown.
//!
//! Used by both `/provider` (onboarding/model-list) and `/key` (API key mgmt).

use super::dropdown::DropdownItem;

/// A provider item for the dropdown.
#[derive(Clone, Debug)]
pub struct ProviderItem {
    /// The provider's id (e.g. "anthropic", "lm-studio").
    pub key: String,
    /// Display name (e.g. "Anthropic", "LM Studio").
    pub name: String,
    /// Whether this is a local provider (no API key needed).
    pub local: bool,
    /// Whether an API key is configured for this provider.
    /// Only meaningful in the `/key` picker — always `false` in `/provider`.
    pub key_set: bool,
}

impl DropdownItem for ProviderItem {
    fn label(&self) -> &str {
        &self.name
    }
    fn description(&self) -> String {
        let mut desc = if self.local {
            "Local, no API key".to_string()
        } else {
            String::new()
        };
        if self.key_set {
            if !desc.is_empty() {
                desc.push(' ');
            }
            desc.push('\u{2705}');
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
    fn key_set_shows_checkmark() {
        let item = ProviderItem {
            key: "anthropic".to_string(),
            name: "Anthropic".to_string(),
            local: false,
            key_set: true,
        };
        assert!(item.description().contains('\u{2705}'));
    }

    #[test]
    fn no_key_no_marker() {
        let item = ProviderItem {
            key: "openai".to_string(),
            name: "OpenAI".to_string(),
            local: false,
            key_set: false,
        };
        assert!(item.description().is_empty());
    }

    #[test]
    fn local_shows_description() {
        let item = ProviderItem {
            key: "ollama".to_string(),
            name: "Ollama".to_string(),
            local: true,
            key_set: false,
        };
        assert!(item.description().contains("Local, no API key"));
    }

    #[test]
    fn filter_matches_key_name_local() {
        let item = ProviderItem {
            key: "lmstudio".to_string(),
            name: "LM Studio".to_string(),
            local: true,
            key_set: false,
        };
        assert!(item.matches_filter("lm"));
        assert!(item.matches_filter("Studio"));
        assert!(item.matches_filter("local"));
        assert!(!item.matches_filter("gemini"));
    }
}
