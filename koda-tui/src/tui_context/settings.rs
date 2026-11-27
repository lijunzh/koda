//! The TUI's settings through the client protocol (PR 6c-1b).
//!
//! The host owns what a session runs with. The TUI asks for a change with
//! `session/settings/update` or, for another provider, a new session with
//! `session/start`, as Codex's TUI does with `thread/settings/update` and
//! `thread/start`, and mirrors what `session/settings/updated` reports into
//! its own `config` for the status bar, the banner and the completer,
//! wherever it arrives. Nothing waits for that report, as Codex's TUI
//! doesn't: a session's start, resume or fork answer says what it runs
//! with, and a change made during a turn is applied, and reported, once the
//! turn ends. A session's provider never changes; switching provider opens
//! a new one, and the one left stays in `/resume`.

use koda_client::events::Outgoing;
use koda_client::protocol::model::ModelProviderSummary;
use koda_client::protocol::model::{Model, ModelListParams};
use koda_client::protocol::session::{
    SessionSettings, SessionSettingsUpdateParams, SessionStartParams, SessionUnsubscribeParams,
};
use koda_client::protocol::{ServerNotification, methods};

use super::{TuiContext, TuiSession};

/// Where a `/model` choice applies, as in Codex's picker: Enter sets the
/// default (the session and `config.toml`), `s` the session only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelScope {
    /// This session, and saved for the next start.
    Default,
    /// This session only; `config.toml` is left as it is.
    Session,
}

/// The host's own words, for an error the user reads.
fn host_error(error: koda_client::RequestError) -> anyhow::Error {
    match error {
        koda_client::RequestError::Server { error, .. } => anyhow::anyhow!(error.message),
        other => anyhow::anyhow!(other),
    }
}

impl TuiContext {
    /// Ask the host to change the session's model and/or mode. Its answer
    /// says whether it took the change; the settings it then runs with
    /// come in `session/settings/updated`, mirrored when they arrive, after
    /// the running turn if there is one.
    async fn request_settings(
        &self,
        model: Option<String>,
        mode: Option<koda_client::protocol::state::TrustMode>,
    ) -> anyhow::Result<()> {
        self.client
            .request::<methods::SessionSettingsUpdate>(SessionSettingsUpdateParams {
                session_id: self.session.id.clone(),
                model,
                effort: None,
                mode,
            })
            .await
            .map(|_| ())
            .map_err(host_error)
    }

    /// Mirror the host's settings into the TUI's own config.
    fn adopt(&mut self, settings: SessionSettings) {
        // The host's word on the mode: it may have changed what was asked.
        self.mode = settings.mode;
        self.renderer.model = settings.model.clone();
        self.settings = settings;
        self.model_names_stale = true;
    }

    /// The session's `session/settings/updated`, wherever it arrives. True
    /// when it was one, for the caller to drop.
    pub(crate) fn mirror_settings(&mut self, message: &Outgoing) -> bool {
        let Outgoing::Notification(ServerNotification::SessionSettingsUpdated(n)) = message else {
            return false;
        };
        if n.session_id != self.session.id {
            return false;
        }
        self.adopt(n.session_settings.clone());
        true
    }

    /// A session the TUI has just switched to: mirror what its start,
    /// resume or fork answer says it runs with, and give it the TUI's mode
    /// if it runs in another, which the host checks against its rules.
    pub(crate) async fn opened(&mut self, settings: Option<Box<SessionSettings>>) {
        let mode = self.mode;
        match settings {
            Some(settings) => self.adopt(*settings),
            None => crate::tui_output::warn_msg(
                &mut self.scroll_buffer,
                "Koda's host didn't say what the session runs with".to_string(),
            ),
        }
        if self.mode != mode {
            self.mode = mode;
            self.send_mode().await;
        }
    }

    /// The TUI's mode, from Shift+Tab or a command; the host applies it,
    /// after the running turn if there is one, and reports it.
    pub(crate) async fn send_mode(&mut self) {
        if let Err(e) = self.request_settings(None, Some(self.mode)).await {
            crate::tui_output::warn_msg(
                &mut self.scroll_buffer,
                format!("The mode didn't reach Koda's host: {e:#}"),
            );
        }
    }

    /// Switch to `mode`, which the host checks; back to the mode before if
    /// it refuses.
    pub(crate) async fn set_mode(
        &mut self,
        mode: koda_client::protocol::state::TrustMode,
    ) -> anyhow::Result<()> {
        let was = std::mem::replace(&mut self.mode, mode);
        self.request_settings(None, Some(mode))
            .await
            .inspect_err(|_| self.mode = was)
    }

    /// `/model`: the session's model, within its provider, and with
    /// [`ModelScope::Default`] the next start's choice too.
    pub(crate) async fn change_model(&mut self, model: &str, scope: ModelScope) {
        match self.request_settings(Some(model.to_string()), None).await {
            Ok(()) => {
                // Saved as chosen: the host resolves an alias again at the
                // next start.
                if scope == ModelScope::Default {
                    let mut chosen = self.settings.clone();
                    chosen.model = model.to_string();
                    chosen.alias = None;
                    save_choice(&self.client, &chosen, &self.providers).await;
                }
                let scope = match scope {
                    ModelScope::Default => "default",
                    ModelScope::Session => "this session",
                };
                crate::tui_output::ok_msg(
                    &mut self.scroll_buffer,
                    format!("Model: {model} \u{00b7} {scope}"),
                );
            }
            Err(e) => {
                let mut why = format!("{e:#}");
                if why.contains("start a new session") {
                    why.push_str(" (/provider)");
                }
                crate::tui_output::err_msg(&mut self.scroll_buffer, why);
            }
        }
    }

    /// `model/list` for provider `provider` at `base_url`.
    pub(crate) async fn list_models(&self, provider: &str, base_url: &str) -> Vec<Model> {
        self.client
            .request::<methods::ModelList>(ModelListParams {
                model_provider: Some(provider.to_string()),
                base_url: Some(base_url.to_string()),
            })
            .await
            .map(|list| list.data)
            .unwrap_or_default()
    }

    /// `/provider`: a model of the session's provider is a model change; one
    /// of another provider, or another endpoint, opens a new session on it,
    /// as switching provider means in Codex. Without a model, a local server
    /// gets the one it has loaded (its first listed) and a cloud provider
    /// its default.
    ///
    /// Without `provider`, the host takes it from `base_url`, as
    /// `koda --base-url` does (`/provider <url>`).
    pub(crate) async fn switch_provider(
        &mut self,
        provider: Option<&ModelProviderSummary>,
        base_url: Option<String>,
        model: Option<String>,
    ) {
        let same = provider.is_none_or(|p| p.id == self.settings.model_provider)
            && base_url
                .as_deref()
                .is_none_or(|url| url == self.settings.base_url);
        if same && provider.is_some() {
            match model {
                Some(model) => self.change_model(&model, ModelScope::Default).await,
                None => crate::tui_output::dim_msg(
                    &mut self.scroll_buffer,
                    format!("Already on {}.", self.settings.model_provider),
                ),
            }
            return;
        }
        let model = match (model, provider) {
            (Some(model), _) => Some(model),
            (None, Some(p)) if !p.requires_api_key => self
                .list_models(&p.id, base_url.as_deref().unwrap_or(&p.base_url))
                .await
                .into_iter()
                .find(|m| m.alias.is_none())
                .map(|m| m.id),
            (None, _) => None,
        };
        let started = self
            .client
            .request::<methods::SessionStart>(SessionStartParams {
                model_provider: provider.map(|p| p.id.clone()),
                base_url,
                model,
                ..Default::default()
            })
            .await
            .map_err(host_error);
        let started = match started {
            Ok(started) => started,
            Err(e) => {
                crate::tui_output::err_msg(&mut self.scroll_buffer, format!("{e:#}"));
                return;
            }
        };
        let left = std::mem::replace(&mut self.host_session, started.session.id.clone());
        let _ = self
            .client
            .request::<methods::SessionUnsubscribe>(SessionUnsubscribeParams { session_id: left })
            .await;
        let instruction_sources = started.session.instruction_sources.unwrap_or_default();
        self.session = TuiSession {
            id: started.session.id,
            instruction_sources,
        };
        // A new session has no replies to `/copy` yet.
        self.renderer.forget_replies();
        self.opened(started.session.settings).await;
        // Saved by the alias the host says it follows, if any.
        save_choice(&self.client, &self.settings, &self.providers).await;
        crate::tui_output::ok_msg(
            &mut self.scroll_buffer,
            format!(
                "New session on {} ({}). The one you left is in /resume.",
                self.providers.name(&self.settings.model_provider),
                self.settings.model
            ),
        );
    }
}

/// What the command line named for a new session (`--provider`,
/// `--base-url`, `--model`): each optional, the rest left to the host's
/// saved choice (`config/read`), as Codex's TUI passes only its overrides
/// to `thread/start`. A provider by its id or a name for it, a model by
/// alias or id; the host resolves them.
#[derive(Debug, Clone, Default)]
pub struct Named {
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
}

/// Save the session's provider and model as the choice a new session
/// starts from (`config/batchWrite` into config.toml); the model by the
/// alias it follows, when the host says it follows one, so it follows its
/// line. The endpoint is saved as the provider's
/// `[model_providers.<id>] base_url` when it isn't the provider's default,
/// and removed when it is, so the file stays short.
pub(crate) async fn save_choice(
    client: &koda_client::InProcessClient,
    settings: &SessionSettings,
    providers: &crate::providers::Providers,
) {
    use koda_client::protocol::config::{ConfigBatchWriteParams, ConfigEdit, MergeStrategy};
    use serde_json::Value;
    let edit = |key: String, value: Value| ConfigEdit {
        key_path: key,
        value,
        merge_strategy: MergeStrategy::Replace,
    };
    let id = &settings.model_provider;
    let default = providers.find(id).map(|p| p.base_url.as_str());
    let endpoint = if default == Some(settings.base_url.as_str()) {
        Value::Null
    } else {
        Value::String(settings.base_url.clone())
    };
    let model = settings.alias.as_deref().unwrap_or(&settings.model);
    let saved = client
        .request::<methods::ConfigBatchWrite>(ConfigBatchWriteParams {
            edits: vec![
                edit("model_provider".into(), Value::String(id.clone())),
                edit(format!("model_providers.{id}.base_url"), endpoint),
                edit("model".into(), Value::String(model.to_string())),
            ],
            expected_version: None,
        })
        .await;
    if let Err(e) = saved {
        tracing::warn!("couldn't save the model choice: {e}");
    }
}

/// Save the view new sessions start in, `[tui] view_mode`; `None` removes
/// it, for the default view.
pub(crate) async fn save_view_mode(
    client: &koda_client::InProcessClient,
    mode: Option<koda_config::ViewMode>,
) {
    use koda_client::protocol::config::{ConfigBatchWriteParams, ConfigEdit, MergeStrategy};
    let value = mode
        .and_then(|mode| serde_json::to_value(mode).ok())
        .unwrap_or(serde_json::Value::Null);
    let saved = client
        .request::<methods::ConfigBatchWrite>(ConfigBatchWriteParams {
            edits: vec![ConfigEdit {
                key_path: "tui.view_mode".into(),
                value,
                merge_strategy: MergeStrategy::Replace,
            }],
            expected_version: None,
        })
        .await;
    if let Err(e) = saved {
        tracing::warn!("couldn't save the view: {e}");
    }
}

/// Store provider `provider`'s API key through the host
/// (`modelProvider/apiKey/set`), which also makes it the one this run uses.
/// Returns the host's error.
pub(crate) async fn set_api_key(
    client: &koda_client::InProcessClient,
    provider: &str,
    key: &str,
) -> anyhow::Result<()> {
    use koda_client::protocol::config::ModelProviderApiKeySetParams;
    client
        .request::<methods::ModelProviderApiKeySet>(ModelProviderApiKeySetParams {
            provider_id: provider.to_string(),
            api_key: key.to_string(),
        })
        .await
        .map(|_| ())
        .map_err(host_error)
}

/// Whether `WebSearch` has a Brave Search key, from the host
/// (`webSearch/apiKey/read`). `None` when the host can't answer: `/key`
/// then leaves Brave out rather than guess.
pub(crate) async fn web_search_key(
    client: &koda_client::InProcessClient,
) -> Option<koda_client::protocol::config::WebSearchApiKeyReadResponse> {
    use koda_client::protocol::config::WebSearchApiKeyReadParams;
    client
        .request::<methods::WebSearchApiKeyRead>(WebSearchApiKeyReadParams {})
        .await
        .inspect_err(|e| tracing::warn!("webSearch/apiKey/read: {e}"))
        .ok()
}

/// Store the Brave Search key through the host (`webSearch/apiKey/set`),
/// which also makes it the one this run's searches use. Returns the
/// host's error.
pub(crate) async fn set_web_search_key(
    client: &koda_client::InProcessClient,
    key: &str,
) -> anyhow::Result<()> {
    use koda_client::protocol::config::WebSearchApiKeySetParams;
    client
        .request::<methods::WebSearchApiKeySet>(WebSearchApiKeySetParams {
            api_key: key.to_string(),
        })
        .await
        .map(|_| ())
        .map_err(host_error)
}
