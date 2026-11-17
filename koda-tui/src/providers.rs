//! The providers Koda's host offers, from `modelProvider/list`, as the
//! TUI's `/provider` and `/key` menus read them: each one's id, name,
//! default endpoint, key variable and whether it needs or has a key. The
//! host owns the catalog, as Codex's server owns its `ModelProviderInfo`;
//! the TUI keeps the list, and asks again once a key changes.

use koda_client::protocol::methods;
use koda_client::protocol::model::{ModelProviderListParams, ModelProviderSummary};

/// The host's providers, in its order.
#[derive(Debug, Clone, Default)]
pub struct Providers {
    list: Vec<ModelProviderSummary>,
}

impl Providers {
    /// Ask the host. An empty list when it can't answer: the menus then
    /// show nothing rather than guess.
    pub async fn fetch(client: &koda_client::InProcessClient) -> Self {
        let list = client
            .request::<methods::ModelProviderList>(ModelProviderListParams {})
            .await
            .map(|answer| answer.data)
            .unwrap_or_else(|e| {
                tracing::warn!("modelProvider/list: {e}");
                Vec::new()
            });
        Self { list }
    }

    /// Every provider, in the host's order.
    pub(crate) fn all(&self) -> &[ModelProviderSummary] {
        &self.list
    }

    /// The provider `typed` names: its id or its name, in any case, with
    /// or without punctuation (`lmstudio`, `LM Studio`, `llama.cpp`).
    pub(crate) fn find(&self, typed: &str) -> Option<&ModelProviderSummary> {
        let want = squash(typed);
        self.list
            .iter()
            .find(|p| squash(&p.id) == want || squash(&p.name) == want)
    }

    /// The id of the provider a person typed (`--provider`), matched as
    /// `find` matches, since `session/start` takes ids only, as
    /// Codex's `thread/start` does. A name the list doesn't have goes as
    /// typed: the host refuses one it doesn't know, with its ids.
    pub fn id_for(&self, typed: &str) -> String {
        self.find(typed)
            .map_or_else(|| typed.to_string(), |p| p.id.clone())
    }

    /// The name a person reads for provider `id`, or the id itself.
    pub(crate) fn name<'a>(&'a self, id: &'a str) -> &'a str {
        self.list
            .iter()
            .find(|p| p.id == id)
            .map_or(id, |p| p.name.as_str())
    }
}

/// Lowercase letters and digits only.
fn squash(text: &str) -> String {
    text.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(id: &str, name: &str) -> ModelProviderSummary {
        ModelProviderSummary {
            id: id.to_string(),
            name: name.to_string(),
            base_url: String::new(),
            requires_api_key: false,
            has_api_key: false,
            env_key: String::new(),
        }
    }

    #[test]
    fn a_provider_is_found_by_its_id_or_name_loosely() {
        let providers = Providers {
            list: vec![
                provider("lm-studio", "LM Studio"),
                provider("llama-cpp", "llama.cpp"),
                provider("anthropic", "Anthropic"),
            ],
        };
        for typed in ["lm-studio", "lmstudio", "LM Studio"] {
            assert_eq!(providers.find(typed).unwrap().id, "lm-studio", "{typed}");
        }
        assert_eq!(providers.find("llamacpp").unwrap().id, "llama-cpp");
        assert_eq!(providers.find("ANTHROPIC").unwrap().id, "anthropic");
        assert!(providers.find("nope").is_none());
        assert_eq!(providers.name("anthropic"), "Anthropic");
        assert_eq!(providers.name("other"), "other");
        assert_eq!(providers.id_for("LM Studio"), "lm-studio");
        assert_eq!(providers.id_for("nope"), "nope", "the host decides");
    }
}
