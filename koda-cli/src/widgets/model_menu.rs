//! Model picker dropdown: the models `model/list` gives for a provider,
//! aliases by their name.
//!
//! Appears when the user types `/model` with no args, and after a provider
//! is picked in `/provider`.

use super::dropdown::DropdownItem;

/// A model item for the dropdown: an alias or a model id.
#[derive(Clone, Debug)]
pub struct ModelItem {
    /// Display name (alias name or local model ID).
    pub label: String,
    /// Resolved model ID (e.g. "gemini-2.0-flash-lite").
    pub model_id: String,
    /// Provider name for display (e.g. "Gemini").
    pub provider: String,
    /// Whether this is the currently active model.
    pub is_current: bool,
}

impl DropdownItem for ModelItem {
    fn label(&self) -> &str {
        &self.label
    }
    fn description(&self) -> String {
        let mut parts = Vec::new();
        if self.label != self.model_id {
            parts.push(self.model_id.clone());
        }
        parts.push(self.provider.clone());
        if self.is_current {
            parts.push("\u{25c0} current".to_string());
        }
        parts.join("  ")
    }
    fn matches_filter(&self, filter: &str) -> bool {
        let f = filter.to_lowercase();
        self.label.to_lowercase().contains(&f)
            || self.model_id.to_lowercase().contains(&f)
            || self.provider.to_lowercase().contains(&f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_model_shows_marker() {
        let item = ModelItem {
            label: "claude-sonnet".into(),
            model_id: "claude-sonnet-5".into(),
            provider: "Anthropic".into(),
            is_current: true,
        };
        assert!(item.description().contains('\u{25c0}'));
    }

    #[test]
    fn alias_shows_model_id_and_provider() {
        let item = ModelItem {
            label: "gemini-flash-lite".into(),
            model_id: "gemini-flash-lite-latest".into(),
            provider: "Gemini".into(),
            is_current: false,
        };
        let desc = item.description();
        assert!(desc.contains("gemini-flash-lite-latest"));
        assert!(desc.contains("Gemini"));
    }

    #[test]
    fn filter_matches_alias_model_provider() {
        let item = ModelItem {
            label: "claude-sonnet".into(),
            model_id: "claude-sonnet-5".into(),
            provider: "Anthropic".into(),
            is_current: false,
        };
        assert!(item.matches_filter("sonnet"));
        assert!(item.matches_filter("anthropic"));
        assert!(item.matches_filter("sonnet-5"));
        assert!(!item.matches_filter("gemini"));
    }
}
