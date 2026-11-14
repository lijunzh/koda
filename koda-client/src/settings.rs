//! What a session runs with: its provider, endpoint and model, chosen at
//! `session/start` and changed with `session/settings/update`, and the
//! lists a client picks them from (`model/list`, `modelProvider/list`).
//!
//! A session's provider is fixed, as a Codex thread's is: switching
//! provider means a new session. A model is an id or an alias; an alias
//! such as `claude-opus` follows its line, so the choice is kept as made
//! and resolved again on resume (latest model first, `DESIGN.md`).

use std::collections::BTreeMap;
use std::time::Duration;

use koda_core::config::{KodaConfig, ProviderType};
use koda_core::model_alias;
use koda_protocol::config::{KodaConfigValues, ModelProviderConfigValue};
use koda_protocol::model::{
    Model, ModelListParams, ModelListResponse, ModelProviderListResponse, ModelProviderSummary,
};
use koda_protocol::rpc::ErrorObject;
use koda_protocol::session::SessionSettings;
use koda_protocol::state::TrustMode;

use crate::session::{internal, invalid};

/// How long a provider gets to list its models.
const LIST_TIMEOUT: Duration = Duration::from_secs(10);

/// A provider, endpoint and model, as asked for. Each is optional: what's
/// missing comes from the session's earlier choice or the host's.
#[derive(Debug, Clone, Default)]
pub(crate) struct ModelChoice {
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
}

/// The provider `id` names, by its own id (`lm-studio`, as
/// `modelProvider/list` gives them).
pub(crate) fn provider(id: &str) -> Result<ProviderType, ErrorObject> {
    // The catalog reads names leniently and falls back for an unknown one;
    // only a provider's own id comes back unchanged. (Builds with Koda's
    // test support add `mock`.)
    let ptype = ProviderType::from_url_or_name("", Some(id));
    if ptype.to_string() == id {
        Ok(ptype)
    } else {
        Err(invalid(format!(
            "unknown provider {id:?}; modelProvider/list names them"
        )))
    }
}

/// Resolve `model` for `ptype`: an alias's id, refused if the alias is
/// another provider's, or the id as given. Returns the id and the alias.
fn resolve_model(
    model: &str,
    ptype: ProviderType,
) -> Result<(String, Option<&'static str>), ErrorObject> {
    if model == model_alias::LOCAL_ALIAS {
        return Err(invalid(
            "\"local\" names no model: list the local server's models and pick one by id",
        ));
    }
    match model_alias::resolve(model) {
        Some(resolved) if resolved.provider != ptype => Err(invalid(format!(
            "{model} is a {} model and this session uses {ptype}; a session's provider \
             doesn't change, so start a new session with that provider",
            resolved.provider
        ))),
        Some(resolved) => Ok((resolved.model_id, Some(resolved.alias))),
        None => Ok((model.to_string(), None)),
    }
}

/// Refuse `model` for `ptype` as a session would: `local`, or an alias of
/// another provider.
pub(crate) fn check_model(model: &str, ptype: ProviderType) -> Result<(), ErrorObject> {
    resolve_model(model, ptype).map(|_| ())
}

/// Fetch the live Claude ids first when `model` is an alias that follows
/// the Models API, so it resolves to its line's newest model.
async fn refresh_for(model: Option<&str>) {
    if model.is_some_and(model_alias::follows_models_api) {
        model_alias::ensure_live_claude_ids().await;
    }
}

/// Apply `choice` to `config`, for a session that starts or resumes. A
/// provider can be named, or picked by an alias (`claude-opus` picks
/// Anthropic); a provider without a model gets its default, which is kept
/// as its line's alias when it is that line's newest. Without a named
/// endpoint, the config file's `[model_providers.<id>] base_url` for the
/// provider is used (`endpoints`), whatever the saved provider is. Returns
/// the alias the model was chosen by, if any.
pub(crate) async fn apply_start(
    config: &mut KodaConfig,
    choice: &ModelChoice,
    endpoints: &BTreeMap<String, ModelProviderConfigValue>,
) -> Result<Option<&'static str>, ErrorObject> {
    refresh_for(choice.model.as_deref()).await;
    let by_alias = choice
        .model
        .as_deref()
        .filter(|m| *m != model_alias::LOCAL_ALIAS)
        .and_then(model_alias::resolve);
    let ptype = match (&choice.provider, &by_alias) {
        (Some(id), _) => provider(id)?,
        (None, Some(resolved)) => resolved.provider,
        (None, None) => config.provider_type,
    };
    // A named endpoint, else the one the config file gives this provider,
    // else the host's for its own provider or the provider's default.
    let configured = endpoints
        .get(&ptype.to_string())
        .and_then(|p| p.base_url.clone());
    let base_url = match (&choice.base_url, configured) {
        (Some(url), _) => url.clone(),
        (None, Some(url)) => url,
        (None, None) if ptype == config.provider_type => config.base_url.clone(),
        (None, None) => ptype.default_base_url().to_string(),
    };
    let (model, alias) = match &choice.model {
        Some(model) => resolve_model(model, ptype)?,
        None if ptype == config.provider_type => (
            config.model.clone(),
            model_alias::following_alias(&config.model),
        ),
        None => {
            let default = ptype.default_model().to_string();
            match model_alias::following_alias(&default) {
                Some(alias) => {
                    refresh_for(Some(alias)).await;
                    resolve_model(alias, ptype)?
                }
                None => (default, None),
            }
        }
    };
    // Nothing changed: the window the host was given stands.
    if ptype == config.provider_type && base_url == config.base_url && model == config.model {
        return Ok(alias);
    }
    config.provider_type = ptype;
    config.base_url = base_url;
    config.model = model.clone();
    config.model_settings.model = model;
    config.recalculate_model_derived();
    Ok(alias)
}

/// What `session/settings/update` would change, checked without applying
/// it: an alias of another provider, or an effort the model doesn't take
/// on Anthropic, is refused. Returns the model's id and alias when it
/// changes.
pub(crate) fn check_update(
    config: &KodaConfig,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<Option<(String, Option<&'static str>)>, ErrorObject> {
    let next = model
        .map(|m| resolve_model(m, config.provider_type))
        .transpose()?;
    if let Some(level) = effort
        && config.provider_type == ProviderType::Anthropic
    {
        let target = next.as_ref().map_or(config.model.as_str(), |(id, _)| id);
        koda_core::model_effort::check_typed(target, level).map_err(invalid)?;
    }
    Ok(next)
}

/// Resolve an update's model again, with the live Claude ids fetched, for
/// applying it once it has passed [`check_update`].
pub(crate) async fn resolve_update(
    config: &KodaConfig,
    model: &str,
) -> Result<(String, Option<&'static str>), ErrorObject> {
    refresh_for(Some(model)).await;
    resolve_model(model, config.provider_type)
}

/// The settings a session reports.
pub(crate) fn report(
    config: &KodaConfig,
    alias: Option<&str>,
    mode: koda_core::trust::TrustMode,
) -> SessionSettings {
    SessionSettings {
        model: config.model.clone(),
        alias: alias.map(str::to_string),
        model_provider: config.provider_type.to_string(),
        base_url: config.base_url.clone(),
        context_window: config.max_context_tokens as u64,
        effort: koda_core::model_effort::sent(
            &config.provider_type.to_string(),
            &config.model,
            config.model_settings.reasoning_effort.as_deref(),
        ),
        mode: crate::host::protocol_mode(mode),
    }
}

/// `model/list`: the provider's aliases, then the models it lists that no
/// alias names. For Anthropic that is the newest model of each line. A
/// provider that can't be reached lists only its aliases.
pub(crate) async fn list_models(
    host: &KodaConfig,
    params: ModelListParams,
) -> Result<ModelListResponse, ErrorObject> {
    let ptype = match &params.model_provider {
        Some(id) => provider(id)?,
        None => host.provider_type,
    };
    if ptype == ProviderType::Anthropic {
        model_alias::ensure_live_claude_ids().await;
    }
    let mut data: Vec<Model> = model_alias::all()
        .into_iter()
        .filter(|a| a.provider == ptype && a.alias != model_alias::LOCAL_ALIAS)
        .map(|a| Model {
            id: a.model_id,
            alias: Some(a.alias.to_string()),
        })
        .collect();
    let mut config = host.clone();
    config.provider_type = ptype;
    config.base_url = match params.base_url {
        Some(url) => url,
        None if ptype == host.provider_type => host.base_url.clone(),
        None => ptype.default_base_url().to_string(),
    };
    let listed = tokio::time::timeout(
        LIST_TIMEOUT,
        koda_core::providers::create_provider(&config).list_models(),
    )
    .await;
    match listed {
        Ok(Ok(models)) => {
            for model in models {
                if !data.iter().any(|m| m.id == model.id) {
                    data.push(Model {
                        id: model.id,
                        alias: None,
                    });
                }
            }
        }
        Ok(Err(e)) => tracing::info!("model/list: {ptype} listed nothing: {e:#}"),
        Err(_) => tracing::info!("model/list: {ptype} didn't answer in {LIST_TIMEOUT:?}"),
    }
    Ok(ModelListResponse { data })
}

/// `modelProvider/list`: every provider Koda offers, with whether a key is
/// set for it (from the environment, where stored keys are put at start).
pub(crate) fn list_providers(host: &KodaConfig) -> ModelProviderListResponse {
    ModelProviderListResponse {
        data: ProviderType::OFFERED
            .iter()
            .map(|p| ModelProviderSummary {
                id: p.to_string(),
                name: p.display_name().to_string(),
                base_url: p.default_base_url().to_string(),
                requires_api_key: p.requires_api_key(),
                has_api_key: koda_core::runtime_env::get(p.env_key_name())
                    .is_some_and(|k| !k.is_empty()),
            })
            .collect(),
        default_provider_id: host.provider_type.to_string(),
    }
}

/// The protocol's trust mode for the session's mode.
pub(crate) fn mode_of(mode: TrustMode) -> koda_core::trust::TrustMode {
    match mode {
        TrustMode::Plan => koda_core::trust::TrustMode::Plan,
        TrustMode::Safe => koda_core::trust::TrustMode::Safe,
        TrustMode::Auto => koda_core::trust::TrustMode::Auto,
    }
}

// ── The saved choice (config) ───────────────────────────────────────────

/// A new session's choice with the config file's filling in what the
/// client didn't name: what's named always wins, and a named provider or
/// endpoint takes the saved model only when it's the saved provider, since
/// a model id belongs to its provider. Endpoints are per provider and
/// applied in [`apply_start`], once the provider is known.
pub(crate) fn with_saved(mut choice: ModelChoice, saved: &KodaConfigValues) -> ModelChoice {
    let Some(saved_provider) = saved.model_provider.clone() else {
        return choice;
    };
    let named = match (&choice.provider, &choice.base_url) {
        (Some(id), _) => Some(id.clone()),
        (None, Some(url)) => Some(ProviderType::from_url_or_name(url, None).to_string()),
        (None, None) => None,
    };
    match named {
        Some(named) if named != saved_provider => return choice,
        Some(_) => {}
        None => choice.provider = Some(saved_provider),
    }
    choice.model = choice.model.or_else(|| saved.model.clone());
    choice
}

// ── API keys ────────────────────────────────────────────────────────────

/// `modelProvider/apiKey/set`: store the key and put it where this host's
/// sessions read it. A key in the environment Koda started with is
/// replaced for this run, as the TUI's key wizard did.
pub(crate) async fn set_api_key(
    db: &koda_core::db::Database,
    provider_id: &str,
    key: &str,
) -> Result<(), ErrorObject> {
    let ptype = provider(provider_id)?;
    if !ptype.requires_api_key() {
        return Err(invalid(format!("{ptype} takes no API key")));
    }
    if key.trim().is_empty() {
        return Err(invalid("the key is empty"));
    }
    let env = ptype.env_key_name();
    koda_core::keystore::set_key(db, env, key.trim())
        .await
        .map_err(|e| internal(format!("couldn't store the key: {e:#}")))?;
    koda_core::runtime_env::set(env, key.trim());
    Ok(())
}

/// `modelProvider/apiKey/remove`: forget the stored key. It leaves this
/// run's environment only when it came from the store; one from the shell
/// Koda started in stays.
pub(crate) async fn remove_api_key(
    db: &koda_core::db::Database,
    provider_id: &str,
) -> Result<(), ErrorObject> {
    let ptype = provider(provider_id)?;
    let env = ptype.env_key_name();
    let stored = koda_core::keystore::get_key(db, env)
        .await
        .map_err(|e| internal(format!("couldn't read the key: {e:#}")))?;
    koda_core::keystore::remove_key(db, env)
        .await
        .map_err(|e| internal(format!("couldn't remove the key: {e:#}")))?;
    if stored.is_some() && stored == koda_core::runtime_env::get(env) {
        koda_core::runtime_env::remove(env);
    }
    Ok(())
}

/// A local server's model when the choice is `auto-detect`: its first
/// listed one. The id stays when the server lists none or can't be reached.
pub(crate) async fn detect_model(config: &mut KodaConfig) {
    if config.model != "auto-detect" {
        return;
    }
    let listed = tokio::time::timeout(
        LIST_TIMEOUT,
        koda_core::providers::create_provider(config).list_models(),
    )
    .await;
    if let Ok(Ok(models)) = listed
        && let Some(first) = models.first()
    {
        config.model = first.id.clone();
        config.model_settings.model = first.id.clone();
        config.recalculate_model_derived();
        tracing::info!("auto-detected model: {}", config.model);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved_lm_studio() -> KodaConfigValues {
        KodaConfigValues {
            model: Some("saved-model".into()),
            model_provider: Some("lm-studio".into()),
            model_providers: BTreeMap::from([(
                "lm-studio".into(),
                ModelProviderConfigValue {
                    base_url: Some("http://127.0.0.1:1234/v1".into()),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        }
    }

    fn choice(provider: Option<&str>, base_url: Option<&str>, model: Option<&str>) -> ModelChoice {
        ModelChoice {
            provider: provider.map(str::to_string),
            base_url: base_url.map(str::to_string),
            model: model.map(str::to_string),
        }
    }

    // The model rules the TUI kept since #1408 (`apply_last_provider`), now
    // the host's. Endpoints are per provider since config.toml (CF-1b).

    #[test]
    fn nothing_named_restores_the_saved_choice() {
        let got = with_saved(ModelChoice::default(), &saved_lm_studio());
        assert_eq!(got.provider.as_deref(), Some("lm-studio"));
        assert_eq!(got.base_url, None, "endpoints apply in apply_start");
        assert_eq!(got.model.as_deref(), Some("saved-model"));
    }

    #[test]
    fn a_named_model_keeps_the_saved_provider() {
        let got = with_saved(choice(None, None, Some("named")), &saved_lm_studio());
        assert_eq!(got.provider.as_deref(), Some("lm-studio"));
        assert_eq!(got.model.as_deref(), Some("named"));
    }

    #[test]
    fn another_named_provider_takes_nothing_saved() {
        let got = with_saved(choice(Some("anthropic"), None, None), &saved_lm_studio());
        assert_eq!(got.provider.as_deref(), Some("anthropic"));
        assert_eq!(got.base_url, None);
        assert_eq!(got.model, None);
    }

    #[test]
    fn the_saved_provider_named_keeps_its_saved_model() {
        let got = with_saved(choice(Some("lm-studio"), None, None), &saved_lm_studio());
        assert_eq!(got.model.as_deref(), Some("saved-model"));
    }

    #[test]
    fn a_named_endpoint_is_kept_and_takes_the_saved_model_for_its_provider() {
        // localhost:1234 reads as LM Studio, the saved provider.
        let got = with_saved(
            choice(None, Some("http://localhost:1234/v1"), None),
            &saved_lm_studio(),
        );
        assert_eq!(got.base_url.as_deref(), Some("http://localhost:1234/v1"));
        assert_eq!(got.model.as_deref(), Some("saved-model"));
        // An Anthropic URL is another provider: nothing saved applies.
        let got = with_saved(
            choice(None, Some("https://api.anthropic.com"), None),
            &saved_lm_studio(),
        );
        assert_eq!(got.model, None);
    }

    /// A provider's endpoint from the config file applies whenever a new
    /// session runs on it and names none, whichever provider is saved.
    #[tokio::test]
    async fn a_configured_endpoint_applies_to_its_provider() {
        let endpoints = saved_lm_studio().model_providers;
        let host = KodaConfig::default_for_testing(ProviderType::Anthropic);

        let mut config = host.clone();
        apply_start(
            &mut config,
            &choice(Some("lm-studio"), None, Some("m")),
            &endpoints,
        )
        .await
        .unwrap();
        assert_eq!(config.base_url, "http://127.0.0.1:1234/v1");

        // A named endpoint wins.
        let mut config = host.clone();
        apply_start(
            &mut config,
            &choice(
                Some("lm-studio"),
                Some("http://10.0.0.2:1234/v1"),
                Some("m"),
            ),
            &endpoints,
        )
        .await
        .unwrap();
        assert_eq!(config.base_url, "http://10.0.0.2:1234/v1");

        // Another provider with none configured gets its default.
        let mut config = host.clone();
        apply_start(
            &mut config,
            &choice(Some("ollama"), None, Some("m")),
            &endpoints,
        )
        .await
        .unwrap();
        assert_eq!(config.base_url, ProviderType::Ollama.default_base_url());
    }

    #[test]
    fn nothing_saved_leaves_the_choice_alone() {
        let named = choice(None, None, Some("m"));
        let got = with_saved(named.clone(), &KodaConfigValues::default());
        assert_eq!(got.model, named.model);
        assert_eq!(got.provider, None);
    }
}
