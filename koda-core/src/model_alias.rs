//! Model aliases for stable, cross-provider model selection.
//!
//! Aliases map short, memorable names to exact model IDs + providers.
//!
//! The Gemini aliases point at Google's floating `-latest` names. The
//! Anthropic API has no floating alias (every id from the 4.6 generation
//! on is a pinned snapshot), so the Claude family aliases are resolved
//! at runtime from `GET /v1/models`: the newest `created_at` in each
//! `line` (`haiku` / `sonnet` / `opus`) wins. That lookup happens once
//! per process, the first time an alias is shown, chosen or restored at
//! startup (a Claude alias saved by `/model`), and only when an Anthropic
//! key is configured. The pinned ids in the alias
//! table are the offline fallback: no key, no network, a timeout or an
//! API error leaves them in force.
//!
//! ## Usage
//!
//! - `/model` picker shows aliases (user-facing)
//! - [`ensure_live_claude_ids`](crate::model_alias::ensure_live_claude_ids) before showing or resolving a Claude
//!   alias (the TUI does this; `--model` does it through
//!   [`resolve_for_flag`](crate::model_alias::resolve_for_flag), headless too)
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
    /// `fable`
    Fable,
}

impl ClaudeLine {
    /// Parse the API's `line` value.
    ///
    /// `None` for lines Koda doesn't offer (`mythos`) and for anything
    /// unknown; the API says not to infer a line from the id, and this never
    /// does.
    pub fn parse(line: &str) -> Option<Self> {
        match line {
            "haiku" => Some(Self::Haiku),
            "sonnet" => Some(Self::Sonnet),
            "opus" => Some(Self::Opus),
            "fable" => Some(Self::Fable),
            _ => None,
        }
    }

    /// The lines Koda offers, in `/model` order.
    pub const ALL: [Self; 4] = [Self::Haiku, Self::Sonnet, Self::Opus, Self::Fable];
}

/// Newest model id per Claude line, as the Models API reported it.
pub type ClaudeLineIds = HashMap<ClaudeLine, String>;

/// What one Models API answer says about the Claude lines: each line's
/// newest model, and the line of every model it listed, as the API reports
/// it (the id never decides the line).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeLines {
    /// The newest model of each line.
    pub newest: ClaudeLineIds,
    /// Every listed model's line.
    pub line_of: HashMap<String, ClaudeLine>,
}

impl From<ClaudeLineIds> for ClaudeLines {
    fn from(newest: ClaudeLineIds) -> Self {
        let line_of = newest
            .iter()
            .map(|(line, id)| (id.clone(), *line))
            .collect();
        Self { newest, line_of }
    }
}

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
    ModelAlias {
        alias: "claude-fable",
        model_id: "claude-fable-5-2",
        provider: ProviderType::Anthropic,
        line: Some(ClaudeLine::Fable),
    },
];

// ── Live Claude ids ─────────────────────────────────────────

/// Once-per-process cache of the live Claude line ids.
///
/// The fetch runs at most once. Its outcome, success or failure, is kept
/// for the life of the cache, so a dead network costs one capped request
/// per run rather than one per `/model`.
pub struct LiveClaudeIds {
    cell: tokio::sync::OnceCell<Option<ClaudeLines>>,
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
        self.lines().map(|lines| &lines.newest)
    }

    /// The whole answer, lines included, if the fetch succeeded.
    pub fn lines(&self) -> Option<&ClaudeLines> {
        self.cell.get().and_then(|outcome| outcome.as_ref())
    }

    /// Run `fetch` unless an outcome is already cached, and return the ids
    /// if there are any. Concurrent callers share one fetch.
    pub async fn get_or_fetch<F, Fut, T>(&self, fetch: F) -> Option<&ClaudeLineIds>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = anyhow::Result<T>>,
        T: Into<ClaudeLines>,
    {
        self.cell
            .get_or_init(|| async {
                match fetch().await {
                    Ok(answer) => {
                        let lines: ClaudeLines = answer.into();
                        tracing::info!("Claude aliases from the Models API: {:?}", lines.newest);
                        for notice in new_model_notices(&lines.newest) {
                            tracing::warn!("{notice}");
                        }
                        Some(lines)
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
            .map(|lines| &lines.newest)
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
        tokio::time::timeout(LIVE_LOOKUP_TIMEOUT, provider.claude_lines())
            .await
            .map_err(|_| anyhow::anyhow!("timed out after {LIVE_LOOKUP_TIMEOUT:?}"))?
    })
    .await
}

// ── New models ──────────────────────────────────────────────

/// Where to ask for a model Koda doesn't support yet.
const NEW_MODEL_ISSUE: &str = "https://github.com/lijunzh/koda/issues/new";

/// The newest model of each line that Koda doesn't support yet, that is,
/// one missing from the effort table ([`crate::model_effort`]), in line
/// order. Koda runs it as a current model, without a tuned effort.
pub fn unsupported_newest(ids: &ClaudeLineIds) -> Vec<&str> {
    ClaudeLine::ALL
        .iter()
        .filter_map(|line| ids.get(line))
        .filter(|id| crate::model_effort::lookup(id).is_none())
        .map(String::as_str)
        .collect()
}

/// What to tell the user about each model [`unsupported_newest`] finds,
/// with a link that opens an issue asking for it. The link carries only the
/// model id. The TUI shows these at every start until Koda supports the
/// model; `koda exec` only logs them.
pub fn new_model_notices(ids: &ClaudeLineIds) -> Vec<String> {
    unsupported_newest(ids)
        .into_iter()
        .map(|id| {
            format!(
                "Anthropic has released {id}, which this Koda doesn't support yet. It runs as \
                 a current model, without a tuned effort. Ask for support: \
                 {NEW_MODEL_ISSUE}?title=Support+{id}"
            )
        })
        .collect()
}

/// The alias to save for a model picked from a list or set as a default:
/// the alias whose line `model_id` is the newest of (or the alias's pinned
/// id, when the Models API hasn't answered). Picking the newest model means
/// "the latest", so the choice follows the line on later starts. `None` for
/// any other id, which stays pinned.
pub fn following_alias(model_id: &str) -> Option<&'static str> {
    following_alias_with(model_id, LIVE.get())
}

/// [`following_alias`] against explicit live ids.
pub fn following_alias_with(model_id: &str, live: Option<&ClaudeLineIds>) -> Option<&'static str> {
    ALIASES
        .iter()
        .filter(|a| a.line.is_some())
        .find(|a| a.resolved(live).model_id == model_id)
        .map(|a| a.alias)
}

/// When `model` is pinned to a Claude model older than its line's newest,
/// and Koda supports the newest: say so, and how to follow the line.
pub fn pinned_older_notice(model: &str) -> Option<String> {
    pinned_older_notice_with(model, LIVE.lines()?)
}

/// [`pinned_older_notice`] against an explicit Models API answer.
pub fn pinned_older_notice_with(model: &str, lines: &ClaudeLines) -> Option<String> {
    let line = lines.line_of.get(model)?;
    let newest = lines.newest.get(line)?;
    if newest == model || crate::model_effort::lookup(newest).is_none() {
        return None;
    }
    let alias = ALIASES.iter().find(|a| a.line == Some(*line))?.alias;
    Some(format!(
        "You're on {model}; {newest} is newer. /model {alias} follows the newest."
    ))
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

/// Resolve `--model` when it names an alias, as `/model`
/// lists them, so `koda --model claude-haiku` runs the newest Haiku rather
/// than sending `claude-haiku` to the API. A Claude alias asks the Models
/// API first, once per process ([`ensure_live_claude_ids`]); without a key
/// or an answer the pinned id stands.
///
/// `None`, leaving the name as given, for a model id, for `local` (only the
/// TUI's picker can choose a local model), and when `--provider` or
/// `--base-url` points at another provider than the alias's: the alias
/// means nothing there.
pub async fn resolve_for_flag(
    name: &str,
    provider: Option<&str>,
    base_url: Option<&str>,
) -> Option<ResolvedAlias> {
    let pinned = resolve_with(name, None)?;
    if pinned.needs_auto_detect() {
        return None;
    }
    let chosen = match (provider, base_url) {
        (Some(p), _) => Some(ProviderType::from_url_or_name("", Some(p))),
        (None, Some(url)) => Some(ProviderType::from_url_or_name(url, None)),
        (None, None) => None,
    };
    if chosen.is_some_and(|p| p != pinned.provider) {
        return None;
    }
    if follows_models_api(name) {
        ensure_live_claude_ids().await;
    }
    resolve(name)
}

/// True if `name` is a Claude alias that follows the Models API, so
/// resolving it needs [`ensure_live_claude_ids`] first.
pub fn follows_models_api(name: &str) -> bool {
    ALIASES.iter().any(|a| a.alias == name && a.line.is_some())
}

/// Resolve an alias saved by `/model`, for startup.
///
/// Unlike [`resolve_with`], this returns `None` when the answer would only
/// be a guess: a Claude alias the Models API didn't answer for this run,
/// `local` (its model is detected, not named) or an unknown name. The
/// saved id then stands; it is at least as new as the pinned fallback was
/// when it was chosen.
pub fn resolve_saved(name: &str, live: Option<&ClaudeLineIds>) -> Option<ResolvedAlias> {
    let alias = ALIASES.iter().find(|a| a.alias == name)?;
    if let Some(line) = alias.line
        && !live.is_some_and(|ids| ids.contains_key(&line))
    {
        return None;
    }
    Some(alias.resolved(live))
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

    // Gemini aliases, so no test here asks the Models API, even with a key set.
    #[tokio::test]
    async fn the_model_flag_resolves_an_alias_to_its_id_and_provider() {
        let r = resolve_for_flag("gemini-flash", None, None).await.unwrap();
        assert_eq!(r.provider, ProviderType::Gemini);
        assert_ne!(r.model_id, "gemini-flash");
        // Saying the alias's own provider, by name or by endpoint, is fine.
        assert!(
            resolve_for_flag("gemini-flash", Some("gemini"), None)
                .await
                .is_some()
        );
        let url = ProviderType::Gemini.default_base_url();
        assert!(
            resolve_for_flag("gemini-flash", None, Some(url))
                .await
                .is_some()
        );
    }

    #[tokio::test]
    async fn the_model_flag_leaves_ids_local_and_other_providers_alone() {
        assert!(
            resolve_for_flag("claude-haiku-5-5", None, None)
                .await
                .is_none()
        );
        assert!(
            resolve_for_flag("gemini-2.5-flash", None, None)
                .await
                .is_none()
        );
        assert!(resolve_for_flag("local", None, None).await.is_none());
        assert!(
            resolve_for_flag("gemini-flash", Some("openrouter"), None)
                .await
                .is_none()
        );
        let elsewhere = Some("http://localhost:8080/v1");
        assert!(
            resolve_for_flag("gemini-flash", None, elsewhere)
                .await
                .is_none()
        );
    }

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
    fn resolve_saved_follows_the_line_only_with_a_live_answer() {
        let ids = live(&[(ClaudeLine::Haiku, "claude-haiku-9")]);
        let r = resolve_saved("claude-haiku", Some(&ids)).unwrap();
        assert_eq!(r.model_id, "claude-haiku-9");
        assert_eq!(r.provider, ProviderType::Anthropic);

        // No live answer, or none for this line: the saved id stands.
        assert!(resolve_saved("claude-haiku", None).is_none());
        assert!(resolve_saved("claude-opus", Some(&ids)).is_none());
    }

    #[test]
    fn resolve_saved_resolves_static_aliases_and_skips_local_and_unknown() {
        assert_eq!(
            resolve_saved("gemini-flash", None).unwrap().model_id,
            "gemini-flash-latest"
        );
        assert!(resolve_saved(LOCAL_ALIAS, None).is_none());
        assert!(resolve_saved("claude-haiku-5-5-20261001", None).is_none());
    }

    #[test]
    fn follows_models_api_is_true_only_for_claude_line_aliases() {
        assert!(follows_models_api("claude-sonnet"));
        assert!(!follows_models_api("gemini-pro"));
        assert!(!follows_models_api(LOCAL_ALIAS));
        assert!(!follows_models_api("claude-sonnet-5-5"));
    }

    #[test]
    fn claude_line_parse_knows_only_offered_lines() {
        assert_eq!(ClaudeLine::parse("haiku"), Some(ClaudeLine::Haiku));
        assert_eq!(ClaudeLine::parse("sonnet"), Some(ClaudeLine::Sonnet));
        assert_eq!(ClaudeLine::parse("opus"), Some(ClaudeLine::Opus));
        assert_eq!(ClaudeLine::parse("fable"), Some(ClaudeLine::Fable));
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
                Err::<ClaudeLineIds, _>(anyhow::anyhow!("must not be called"))
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
                Err::<ClaudeLineIds, _>(anyhow::anyhow!("network down"))
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

    #[test]
    fn claude_fable_follows_the_fable_line() {
        assert_eq!(
            resolve_with("claude-fable", None).unwrap().model_id,
            "claude-fable-5-2",
            "pinned"
        );
        let live = HashMap::from([(ClaudeLine::Fable, "claude-fable-6".to_string())]);
        assert_eq!(
            resolve_with("claude-fable", Some(&live)).unwrap().model_id,
            "claude-fable-6"
        );
    }

    #[test]
    fn a_newest_model_koda_lacks_gets_a_notice_with_an_issue_link() {
        let ids = HashMap::from([
            (ClaudeLine::Haiku, "claude-haiku-5-5".to_string()),
            (ClaudeLine::Opus, "claude-opus-6".to_string()),
            (ClaudeLine::Fable, "claude-fable-5-2".to_string()),
        ]);
        assert_eq!(unsupported_newest(&ids), ["claude-opus-6"]);
        let notices = new_model_notices(&ids);
        assert_eq!(notices.len(), 1);
        assert!(notices[0].starts_with("Anthropic has released claude-opus-6"));
        assert!(
            notices[0].ends_with(
                "https://github.com/lijunzh/koda/issues/new?title=Support+claude-opus-6"
            ),
            "{}",
            notices[0]
        );

        let known = HashMap::from([(ClaudeLine::Sonnet, "claude-sonnet-5-5".to_string())]);
        assert!(new_model_notices(&known).is_empty());
    }

    #[test]
    fn a_lines_newest_model_is_saved_as_its_alias_and_others_stay_pinned() {
        let live = HashMap::from([(ClaudeLine::Opus, "claude-opus-6".to_string())]);
        assert_eq!(
            following_alias_with("claude-opus-6", Some(&live)),
            Some("claude-opus")
        );
        // Without an answer from the API, the pinned id stands in.
        assert_eq!(
            following_alias_with("claude-sonnet-5-5", None),
            Some("claude-sonnet")
        );
        // Once the line has moved on, the old id is just an id.
        assert_eq!(following_alias_with("claude-opus-5-5", Some(&live)), None);
        assert_eq!(following_alias_with("claude-opus-5", None), None);
        assert_eq!(following_alias_with("gpt-6-astra", None), None);
    }

    #[test]
    fn a_model_pinned_behind_its_line_gets_a_notice() {
        let lines = ClaudeLines {
            newest: HashMap::from([
                (ClaudeLine::Sonnet, "claude-sonnet-5-5".to_string()),
                (ClaudeLine::Opus, "claude-opus-6".to_string()),
            ]),
            line_of: HashMap::from([
                ("claude-sonnet-5-5".to_string(), ClaudeLine::Sonnet),
                ("claude-sonnet-5".to_string(), ClaudeLine::Sonnet),
                ("claude-opus-6".to_string(), ClaudeLine::Opus),
                ("claude-opus-5-5".to_string(), ClaudeLine::Opus),
            ]),
        };
        assert_eq!(
            pinned_older_notice_with("claude-sonnet-5", &lines).as_deref(),
            Some(
                "You're on claude-sonnet-5; claude-sonnet-5-5 is newer. \
                 /model claude-sonnet follows the newest."
            )
        );
        assert_eq!(
            pinned_older_notice_with("claude-sonnet-5-5", &lines),
            None,
            "newest"
        );
        assert_eq!(
            pinned_older_notice_with("ornith", &lines),
            None,
            "not listed"
        );
        // A newest model Koda doesn't support yet has its own notice; this one
        // doesn't send anyone to it.
        assert_eq!(pinned_older_notice_with("claude-opus-5-5", &lines), None);
    }
}
