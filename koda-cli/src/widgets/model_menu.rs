//! Model picker dropdown — shows curated aliases + auto-detected local models.
//!
//! Appears when the user types `/model` with no args.

use super::dropdown::DropdownItem;

/// A model item for the dropdown — either an alias or a local model.
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
    /// Whether this is the special "local" auto-detect alias.
    pub is_local: bool,
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

/// Build model items from aliases + optional local model.
pub fn build_items(
    current_model: &str,
    local_model: Option<&str>,
) -> Vec<ModelItem> {
    let mut items: Vec<ModelItem> = koda_core::model_alias::all()
        .iter()
        .map(|a| {
            let provider_name = a.provider.to_string();
            ModelItem {
                label: a.alias.to_string(),
                model_id: a.model_id.to_string(),
                provider: provider_name,
                is_current: a.model_id == current_model || a.alias == current_model,
                is_local: false,
            }
        })
        .collect();

    // Add local model (LMStudio/Ollama auto-detect)
    match local_model {
        Some(id) => items.push(ModelItem {
            label: "local".to_string(),
            model_id: id.to_string(),
            provider: "LM Studio".to_string(),
            is_current: id == current_model,
            is_local: true,
        }),
        None => items.push(ModelItem {
            label: "local".to_string(),
            model_id: "(not running)".to_string(),
            provider: "LM Studio".to_string(),
            is_current: false,
            is_local: true,
        }),
    }

    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_model_shows_marker() {
        let item = ModelItem {
            label: "claude-sonnet".into(),
            model_id: "claude-sonnet-4-6".into(),
            provider: "Anthropic".into(),
            is_current: true,
            is_local: false,
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
            is_local: false,
        };
        let desc = item.description();
        assert!(desc.contains("gemini-flash-lite-latest"));
        assert!(desc.contains("Gemini"));
    }

    #[test]
    fn filter_matches_alias_model_provider() {
        let item = ModelItem {
            label: "claude-sonnet".into(),
            model_id: "claude-sonnet-4-6".into(),
            provider: "Anthropic".into(),
            is_current: false,
            is_local: false,
        };
        assert!(item.matches_filter("sonnet"));
        assert!(item.matches_filter("anthropic"));
        assert!(item.matches_filter("4-6"));
        assert!(!item.matches_filter("gemini"));
    }

    #[test]
    fn build_items_includes_local() {
        let items = build_items("gpt-4o", None);
        let local = items.iter().find(|i| i.label == "local").unwrap();
        assert!(local.is_local);
        assert_eq!(local.model_id, "(not running)");
    }

    #[test]
    fn build_items_with_local_model() {
        let items = build_items("qwen3-8b", Some("qwen3-8b"));
        let local = items.iter().find(|i| i.label == "local").unwrap();
        assert!(local.is_current);
        assert_eq!(local.model_id, "qwen3-8b");
    }

    #[test]
    fn build_items_marks_current_by_alias() {
        let items = build_items("claude-sonnet", None);
        let current = items.iter().find(|i| i.is_current);
        assert!(current.is_some());
        assert_eq!(current.unwrap().label, "claude-sonnet");
    }

    #[test]
    fn build_items_marks_current_by_model_id() {
        let items = build_items("claude-sonnet-4-6", None);
        let current = items.iter().find(|i| i.is_current);
        assert!(current.is_some());
        assert_eq!(current.unwrap().label, "claude-sonnet");
    }
}
