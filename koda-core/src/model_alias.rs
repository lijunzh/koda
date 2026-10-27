//! Model aliases for stable, cross-provider model selection.
//!
//! Aliases map short, memorable names to exact model IDs + providers.
//!
//! The Gemini aliases point at Google's floating `-latest` names. The
//! Anthropic API has no floating alias (every id from the 4.6 generation
//! on is a pinned snapshot), so the Claude family aliases are resolved
//! at runtime from `GET /v1/models`: the newest `created_at` in each
//! `line` (`haiku` / `sonnet` / `opus`) wins. That lookup happens once
//! per process, the first time an alias is shown or chosen, and only
//! when an Anthropic key is configured. The pinned ids in the alias
//! table are the offline fallback: no key, no network, a timeout or an
//! API error leaves them in force.
//!
//! ## Usage
//!
//! - `/model` picker shows aliases (user-facing)
//! - [`ensure_live_claude_ids`](crate::model_alias::ensure_live_claude_ids) before showing or resolving a Claude
//!   alias (the TUI does this; headless mode never resolves aliases)
//! - `resolve("claude-sonnet")` → `Some(ResolvedAlias { model_id, provider })`
//! - `resolve("local")` → `Some(ResolvedAlias { model_id: "auto-detect", provider: LMStudio })`

use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use crate::config::ProviderType;

/// A Claude model line, as the Anthropic Models API reports it in `line`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClaudeLine {
    /// `haiku`
    Haiku,
    /// `sonnet`
    Sonnet,
    /// `opus`
    Opus,
}

impl ClaudeLine {
    /// Parse the API's `line` value.
    ///
    /// `None` for lines Koda has no alias for (`fable`, `mythos`) and for
    /// anything unknown; the API says not to infer a line from the id, and
    /// this never does.
    pub fn parse(line: &str) -> Option<Self> {
        match line {
            "haiku" => Some(Self::Haiku),
            "sonnet" => Some(Self::Sonnet),
            "opus" => Some(Self::Opus),
            _ => None,
        }
    }
}

/// Newest model id per Claude line, as the Models API reported it.
pub type ClaudeLineIds = HashMap<ClaudeLine, String>;

/// A hardcoded model alias.
#[derive(Debug, Clone, Copy)]
pub struct ModelAlias {
    /// Short stable name (e.g. `"gemini-flash-lite"`).
    pub alias: &'static str,
    /// Exact model ID sent to the provider API. For an alias with a
    /// [`line`](Self::line), this is the pinned offline fallback.
    pub model_id: &'static str,
    /// Which provider serves this model.
    pub provider: ProviderType,
    /// The Claude line this alias follows; resolved live when the Models
    /// API has answered this run.
    pub line: Option<ClaudeLine>,
}

impl ModelAlias {
    fn resolved(&self, live: Option<&ClaudeLineIds>) -> ResolvedAlias {
        let model_id = self
            .line
            .and_then(|line| live.and_then(|ids| ids.get(&line)))
            .cloned()
            .unwrap_or_else(|| self.model_id.to_string());
        ResolvedAlias {
            alias: self.alias,
            model_id,
            provider: self.provider,
        }
    }
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
        line: None,
    },
    ModelAlias {
        alias: "gemini-flash",
        model_id: "gemini-flash-latest",
        provider: ProviderType::Gemini,
        line: None,
    },
    ModelAlias {
        alias: "gemini-pro",
        model_id: "gemini-pro-latest",
        provider: ProviderType::Gemini,
        line: None,
    },
    // ── Anthropic (resolved live by `line`; these ids are the offline
    // fallback and move by hand to the latest family) ──
    ModelAlias {
        alias: "claude-haiku",
        model_id: "claude-haiku-5-5",
        provider: ProviderType::Anthropic,
        line: Some(ClaudeLine::Haiku),
    },
    ModelAlias {
        alias: "claude-sonnet",
        model_id: "claude-sonnet-5-5",
        provider: ProviderType::Anthropic,
        line: Some(ClaudeLine::Sonnet),
    },
    ModelAlias {
        alias: "claude-opus",
        model_id: "claude-opus-5-5",
        provider: ProviderType::Anthropic,
        line: Some(ClaudeLine::Opus),
    },
];

// ── Live Claude ids ─────────────────────────────────────────

/// Once-per-process cache of the live Claude line ids.
///
/// The fetch runs at most once. Its outcome, success or failure, is kept
/// for the life of the cache, so a dead network costs one capped request
/// per run rather than one per `/model`.
pub struct LiveClaudeIds {
    cell: tokio::sync::OnceCell<Option<ClaudeLineIds>>,
}

impl LiveClaudeIds {
    /// An empty cache; nothing has been fetched.
    pub const fn new() -> Self {
        Self {
            cell: tokio::sync::OnceCell::const_new(),
        }
    }

    /// The ids, if the fetch ran this run and succeeded.
    pub fn get(&self) -> Option<&ClaudeLineIds> {
        self.cell.get().and_then(|outcome| outcome.as_ref())
    }

    /// Run `fetch` unless an outcome is already cached, and return the ids
    /// if there are any. Concurrent callers share one fetch.
    pub async fn get_or_fetch<F, Fut>(&self, fetch: F) -> Option<&ClaudeLineIds>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = anyhow::Result<ClaudeLineIds>>,
    {
        self.cell
            .get_or_init(|| async {
                match fetch().await {
                    Ok(ids) => {
                        tracing::info!("Claude aliases from the Models API: {ids:?}");
                        Some(ids)
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Anthropic Models API unavailable; Claude aliases use the pinned ids: {e:#}"
                        );
                        None
                    }
                }
            })
            .await
            .as_ref()
    }
}

impl Default for LiveClaudeIds {
    fn default() -> Self {
        Self::new()
    }
}

static LIVE: LiveClaudeIds = LiveClaudeIds::new();

/// Cap on the one `/v1/models` request per process. The provider's own
/// connect timeout is 30 s, which is too long to stall the `/model` picker.
const LIVE_LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Resolve the Claude line aliases against the Models API, once per process.
///
/// Returns the live ids when an Anthropic key is configured and the API
/// answered, now or earlier this run. Returns `None`, leaving the pinned
/// ids in force, when no key is set (nothing is cached then, so a key set
/// later with `/key` still triggers the lookup) or when the one request
/// failed or timed out.
pub async fn ensure_live_claude_ids() -> Option<&'static ClaudeLineIds> {
    let ptype = ProviderType::Anthropic;
    let key = crate::runtime_env::get(ptype.env_key_name()).filter(|k| !k.is_empty())?;
    LIVE.get_or_fetch(|| async move {
        let provider = crate::providers::anthropic::AnthropicProvider::new(
            key,
            Some(ptype.default_base_url()),
        );
        tokio::time::timeout(LIVE_LOOKUP_TIMEOUT, provider.latest_claude_ids())
            .await
            .map_err(|_| anyhow::anyhow!("timed out after {LIVE_LOOKUP_TIMEOUT:?}"))?
    })
    .await
}

// ── Resolution ──────────────────────────────────────────────

/// Resolve an alias to its model ID and provider.
///
/// Returns `None` if `name` is not a known alias (treat as literal model ID).
/// For `"local"`, returns a sentinel — caller must auto-detect via LMStudio API.
/// A Claude alias gets the live id when [`ensure_live_claude_ids`] has
/// succeeded this run, otherwise its pinned id.
///
/// ```
/// use koda_core::model_alias::resolve;
/// use koda_core::config::ProviderType;
///
/// let r = resolve("gemini-flash").unwrap();
/// assert_eq!(r.provider, ProviderType::Gemini);
///
/// assert!(resolve("not-an-alias").is_none());
/// ```
pub fn resolve(name: &str) -> Option<ResolvedAlias> {
    resolve_with(name, LIVE.get())
}

/// [`resolve`] against an explicit set of live ids; `None` means pinned
/// ids only.
pub fn resolve_with(name: &str, live: Option<&ClaudeLineIds>) -> Option<ResolvedAlias> {
    if name == LOCAL_ALIAS {
        return Some(ResolvedAlias {
            alias: LOCAL_ALIAS,
            model_id: "auto-detect".to_string(),
            provider: ProviderType::LMStudio,
        });
    }
    ALIASES
        .iter()
        .find(|a| a.alias == name)
        .map(|a| a.resolved(live))
}

/// Result of resolving an alias.
#[derive(Debug, Clone)]
pub struct ResolvedAlias {
    /// The alias that was resolved.
    pub alias: &'static str,
    /// Exact model ID for the provider API (or `"auto-detect"` for local).
    pub model_id: String,
    /// Provider that serves this model.
    pub provider: ProviderType,
}

impl ResolvedAlias {
    /// True if this alias requires runtime auto-detection (LMStudio/Ollama).
    pub fn needs_auto_detect(&self) -> bool {
        self.model_id == "auto-detect"
    }
}

/// All aliases in display order (for the `/model` picker), with the live
/// Claude ids when [`ensure_live_claude_ids`] has succeeded this run.
pub fn all() -> Vec<ResolvedAlias> {
    all_with(LIVE.get())
}

/// [`all`] against an explicit set of live ids; `None` means pinned ids only.
pub fn all_with(live: Option<&ClaudeLineIds>) -> Vec<ResolvedAlias> {
    ALIASES.iter().map(|a| a.resolved(live)).collect()
}

/// The pinned alias table: the offline fallback, in display order.
pub fn pinned() -> &'static [ModelAlias] {
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn live(pairs: &[(ClaudeLine, &str)]) -> ClaudeLineIds {
        pairs
            .iter()
            .map(|(line, id)| (*line, id.to_string()))
            .collect()
    }

    #[test]
    fn resolve_known_alias() {
        let r = resolve("claude-sonnet").unwrap();
        assert_eq!(r.model_id, "claude-sonnet-5-5");
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
        assert!(resolve("claude-sonnet-5-5").is_none());
        assert!(resolve("gemini-2.5-pro").is_none());
    }

    #[test]
    fn resolve_with_live_ids_follows_the_line() {
        let ids = live(&[
            (ClaudeLine::Sonnet, "claude-sonnet-7"),
            (ClaudeLine::Opus, "claude-opus-7"),
        ]);
        let sonnet = resolve_with("claude-sonnet", Some(&ids)).unwrap();
        assert_eq!(sonnet.model_id, "claude-sonnet-7");
        assert_eq!(sonnet.provider, ProviderType::Anthropic);
        assert_eq!(
            resolve_with("claude-opus", Some(&ids)).unwrap().model_id,
            "claude-opus-7"
        );
        // A line the API did not report keeps its pinned id.
        assert_eq!(
            resolve_with("claude-haiku", Some(&ids)).unwrap().model_id,
            "claude-haiku-5-5"
        );
        // Non-Claude aliases are untouched.
        assert_eq!(
            resolve_with("gemini-flash", Some(&ids)).unwrap().model_id,
            "gemini-flash-latest"
        );
        assert!(
            resolve_with("local", Some(&ids))
                .unwrap()
                .needs_auto_detect()
        );
    }

    #[test]
    fn resolve_with_no_live_ids_is_the_pinned_table() {
        for a in pinned() {
            assert_eq!(resolve_with(a.alias, None).unwrap().model_id, a.model_id);
        }
    }

    #[test]
    fn all_with_live_ids_shows_live_claude_ids() {
        let ids = live(&[(ClaudeLine::Haiku, "claude-haiku-9")]);
        let items = all_with(Some(&ids));
        assert_eq!(items.len(), pinned().len());
        let haiku = items.iter().find(|a| a.alias == "claude-haiku").unwrap();
        assert_eq!(haiku.model_id, "claude-haiku-9");
        let gemini = items.iter().find(|a| a.alias == "gemini-pro").unwrap();
        assert_eq!(gemini.model_id, "gemini-pro-latest");
    }

    #[test]
    fn claude_line_parse_knows_only_aliased_lines() {
        assert_eq!(ClaudeLine::parse("haiku"), Some(ClaudeLine::Haiku));
        assert_eq!(ClaudeLine::parse("sonnet"), Some(ClaudeLine::Sonnet));
        assert_eq!(ClaudeLine::parse("opus"), Some(ClaudeLine::Opus));
        assert_eq!(ClaudeLine::parse("fable"), None);
        assert_eq!(ClaudeLine::parse("mythos"), None);
        assert_eq!(ClaudeLine::parse("Sonnet"), None);
        assert_eq!(ClaudeLine::parse(""), None);
    }

    #[test]
    fn every_claude_alias_has_a_line_and_nothing_else_does() {
        for a in pinned() {
            assert_eq!(
                a.line.is_some(),
                a.provider == ProviderType::Anthropic,
                "alias {}",
                a.alias
            );
        }
    }

    #[tokio::test]
    async fn live_cache_fetches_once_and_keeps_success() {
        let cache = LiveClaudeIds::new();
        assert!(cache.get().is_none());
        let calls = Arc::new(AtomicUsize::new(0));

        let c = calls.clone();
        let first = cache
            .get_or_fetch(|| async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(live(&[(ClaudeLine::Sonnet, "claude-sonnet-7")]))
            })
            .await
            .expect("first fetch succeeds");
        assert_eq!(first[&ClaudeLine::Sonnet], "claude-sonnet-7");

        // A second fetch is never run: the first outcome is kept.
        let c = calls.clone();
        let second = cache
            .get_or_fetch(|| async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err(anyhow::anyhow!("must not be called"))
            })
            .await
            .expect("cached ids");
        assert_eq!(second[&ClaudeLine::Sonnet], "claude-sonnet-7");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(cache.get().unwrap()[&ClaudeLine::Sonnet], "claude-sonnet-7");
    }

    #[tokio::test]
    async fn live_cache_keeps_a_failure_for_the_run() {
        let cache = LiveClaudeIds::new();
        let calls = Arc::new(AtomicUsize::new(0));

        let c = calls.clone();
        let first = cache
            .get_or_fetch(|| async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err(anyhow::anyhow!("network down"))
            })
            .await;
        assert!(first.is_none());

        // No retry, even with a fetch that would succeed.
        let c = calls.clone();
        let second = cache
            .get_or_fetch(|| async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(live(&[(ClaudeLine::Opus, "claude-opus-7")]))
            })
            .await;
        assert!(second.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Resolution falls back to the pinned table.
        assert_eq!(
            resolve_with("claude-opus", cache.get()).unwrap().model_id,
            "claude-opus-5-5"
        );
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
        for a in pinned() {
            assert!(
                seen.insert(a.model_id),
                "duplicate model_id: {}",
                a.model_id
            );
        }
    }
}
