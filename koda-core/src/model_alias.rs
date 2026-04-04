//! Hardcoded model aliases for stable, cross-provider model selection.
//!
//! Aliases map short, memorable names to exact model IDs + providers.
//! Updated per release — we only alias models we've tested with Koda.
//!
//! ## Usage
//!
//! - `/model` picker shows aliases (user-facing)
//! - Sub-agent definitions reference aliases: `model: Some("gemini-flash-lite")`
//! - `resolve("claude-sonnet")` → `Some(ResolvedAlias { model_id, provider })`
//! - `resolve("local")` → `Some(ResolvedAlias { model_id: "auto-detect", provider: LMStudio })`

use crate::config::ProviderType;

/// A hardcoded model alias.
#[derive(Debug, Clone, Copy)]
pub struct ModelAlias {
    /// Short stable name (e.g. `"gemini-flash-lite"`).
    pub alias: &'static str,
    /// Exact model ID sent to the provider API.
    pub model_id: &'static str,
    /// Which provider serves this model.
    pub provider: ProviderType,
}

/// The "local" alias uses auto-detection — model ID is discovered at runtime.
pub const LOCAL_ALIAS: &str = "local";

/// All known aliases. Order determines display order in `/model` picker.
static ALIASES: &[ModelAlias] = &[
    // ── Gemini (version-less → auto-resolves to latest) ─
    ModelAlias {
        alias: "gemini-flash-lite",
        model_id: "gemini-flash-lite-latest",
        provider: ProviderType::Gemini,
    },
    ModelAlias {
        alias: "gemini-flash",
        model_id: "gemini-flash-latest",
        provider: ProviderType::Gemini,
    },
    ModelAlias {
        alias: "gemini-pro",
        model_id: "gemini-pro-latest",
        provider: ProviderType::Gemini,
    },
    // ── Anthropic ────────────────────────────────────────
    ModelAlias {
        alias: "claude-haiku",
        model_id: "claude-haiku-3.5-20241022",
        provider: ProviderType::Anthropic,
    },
    ModelAlias {
        alias: "claude-sonnet",
        model_id: "claude-sonnet-4-20250514",
        provider: ProviderType::Anthropic,
    },
    ModelAlias {
        alias: "claude-opus",
        model_id: "claude-opus-4-20250514",
        provider: ProviderType::Anthropic,
    },
];

/// Resolve an alias to its model ID and provider.
///
/// Returns `None` if `name` is not a known alias (treat as literal model ID).
/// For `"local"`, returns a sentinel — caller must auto-detect via LMStudio API.
pub fn resolve(name: &str) -> Option<ResolvedAlias> {
    if name == LOCAL_ALIAS {
        return Some(ResolvedAlias {
            alias: LOCAL_ALIAS,
            model_id: "auto-detect",
            provider: ProviderType::LMStudio,
        });
    }
    ALIASES.iter().find(|a| a.alias == name).map(|a| ResolvedAlias {
        alias: a.alias,
        model_id: a.model_id,
        provider: a.provider,
    })
}

/// Result of resolving an alias.
#[derive(Debug, Clone)]
pub struct ResolvedAlias {
    /// The alias that was resolved.
    pub alias: &'static str,
    /// Exact model ID for the provider API (or `"auto-detect"` for local).
    pub model_id: &'static str,
    /// Provider that serves this model.
    pub provider: ProviderType,
}

impl ResolvedAlias {
    /// True if this alias requires runtime auto-detection (LMStudio/Ollama).
    pub fn needs_auto_detect(&self) -> bool {
        self.model_id == "auto-detect"
    }
}

/// All aliases in display order (for the `/model` picker).
pub fn all() -> &'static [ModelAlias] {
    ALIASES
}

/// All alias names (for tab completion).
pub fn alias_names() -> Vec<&'static str> {
    let mut names: Vec<&str> = ALIASES.iter().map(|a| a.alias).collect();
    names.push(LOCAL_ALIAS);
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_known_alias() {
        let r = resolve("claude-sonnet").unwrap();
        assert_eq!(r.model_id, "claude-sonnet-4-20250514");
        assert_eq!(r.provider, ProviderType::Anthropic);
        assert!(!r.needs_auto_detect());
    }

    #[test]
    fn resolve_local_alias() {
        let r = resolve("local").unwrap();
        assert!(r.needs_auto_detect());
        assert_eq!(r.provider, ProviderType::LMStudio);
    }

    #[test]
    fn resolve_unknown_returns_none() {
        assert!(resolve("not-a-real-model").is_none());
    }

    #[test]
    fn resolve_literal_model_id_returns_none() {
        // Literal model IDs should NOT resolve as aliases
        assert!(resolve("claude-sonnet-4-20250514").is_none());
        assert!(resolve("gemini-2.5-pro").is_none());
    }

    #[test]
    fn all_aliases_non_empty() {
        assert!(!all().is_empty());
    }

    #[test]
    fn alias_names_includes_local() {
        let names = alias_names();
        assert!(names.contains(&"local"));
        assert!(names.contains(&"gemini-flash-lite"));
    }

    #[test]
    fn no_duplicate_aliases() {
        let names = alias_names();
        let mut seen = std::collections::HashSet::new();
        for name in &names {
            assert!(seen.insert(name), "duplicate alias: {name}");
        }
    }

    #[test]
    fn no_duplicate_model_ids() {
        let mut seen = std::collections::HashSet::new();
        for a in all() {
            assert!(seen.insert(a.model_id), "duplicate model_id: {}", a.model_id);
        }
    }
}
