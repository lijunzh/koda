//! The TUI's settings through the client protocol (PR 6c-1b).
//!
//! The host owns what a session runs with. The TUI asks for a change with
//! `session/settings/update` or, for another provider, a new session with
//! `session/start`, as Codex's TUI does with `thread/settings/update` and
//! `thread/start`, and mirrors what `session/settings/updated` reports into
//! its own `config` for the status bar, the banner and the completer. A
//! session's provider never changes; switching provider opens a new one,
//! and the one left stays in `/resume`.

use std::time::Duration;

use koda_client::events::Outgoing;
use koda_client::protocol::model::{Model, ModelListParams};
use koda_client::protocol::session::{
    SessionSettings, SessionSettingsUpdateParams, SessionStartParams, SessionUnsubscribeParams,
};
use koda_client::protocol::{ServerNotification, methods};
use koda_core::config::{KodaConfig, ProviderType};

use super::{TuiContext, TuiSession};

/// How long the host gets to report settings after a change.
const SETTINGS_WAIT: Duration = Duration::from_secs(15);

/// The host's own words, for an error the user reads.
fn host_error(error: koda_client::RequestError) -> anyhow::Error {
    match error {
        koda_client::RequestError::Server { error, .. } => anyhow::anyhow!(error.message),
        other => anyhow::anyhow!(other),
    }
}

/// The protocol's name for a trust mode.
fn protocol_mode(mode: koda_core::trust::TrustMode) -> koda_client::protocol::state::TrustMode {
    use koda_client::protocol::state::TrustMode as P;
    match mode {
        koda_core::trust::TrustMode::Plan => P::Plan,
        koda_core::trust::TrustMode::Safe => P::Safe,
        koda_core::trust::TrustMode::Auto => P::Auto,
    }
}

impl TuiContext {
    /// Ask the host to change the session's model and/or take the TUI's
    /// mode, then mirror what it reports. Returns the alias the model is
    /// chosen by, if any. Only while no turn runs: during one the host
    /// applies a change after it, and nothing here waits for that.
    async fn update_settings(
        &mut self,
        model: Option<String>,
        send_mode: bool,
        show_warnings: bool,
    ) -> anyhow::Result<Option<String>> {
        let mode =
            send_mode.then(|| protocol_mode(koda_core::trust::read_trust(&self.shared_mode)));
        self.client
            .request::<methods::SessionSettingsUpdate>(SessionSettingsUpdateParams {
                session_id: self.session.id.clone(),
                model,
                effort: None,
                mode,
            })
            .await
            .map_err(host_error)?;
        let settings = self.await_settings(show_warnings).await?;
        let alias = settings.alias.clone();
        self.adopt(settings).await;
        Ok(alias)
    }

    /// The next `session/settings/updated` for this session. A warning the
    /// host sends meanwhile, such as an assumed context window, is shown
    /// when `show_warnings`.
    async fn await_settings(&mut self, show_warnings: bool) -> anyhow::Result<SessionSettings> {
        let deadline = tokio::time::Instant::now() + SETTINGS_WAIT;
        loop {
            let next = tokio::time::timeout_at(deadline, self.client.next_event()).await;
            match next {
                Ok(Some(Outgoing::Notification(ServerNotification::SessionSettingsUpdated(n))))
                    if n.session_id == self.session.id =>
                {
                    return Ok(n.settings);
                }
                Ok(Some(Outgoing::Notification(ServerNotification::Warning(w)))) => {
                    if show_warnings {
                        crate::tui_output::warn_msg(&mut self.scroll_buffer, w.message);
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) => anyhow::bail!("Koda's host stopped"),
                Err(_) => anyhow::bail!("Koda's host didn't report the session's settings"),
            }
        }
    }

    /// Mirror the host's settings into the TUI's own config.
    async fn adopt(&mut self, settings: SessionSettings) {
        mirror(&mut self.config, &settings);
        self.renderer.model = settings.model;
        // Still used to list models for the completer.
        *self.provider.write().await = koda_core::providers::create_provider(&self.config);
    }

    /// After opening a session: give the host the TUI's mode, which it has
    /// checked against the sandbox, and mirror the session's settings,
    /// which a resumed session brings back from its last run. The host's
    /// warnings are shown when `show_warnings`; at startup the banner has
    /// said the same.
    pub(crate) async fn sync_settings(&mut self, show_warnings: bool) -> anyhow::Result<()> {
        self.update_settings(None, true, show_warnings)
            .await
            .map(|_| ())
    }

    /// The new mode, from Shift+Tab while no turn runs.
    pub(crate) async fn send_mode(&mut self) {
        if let Err(e) = self.update_settings(None, true, true).await {
            crate::tui_output::warn_msg(
                &mut self.scroll_buffer,
                format!("The mode didn't reach Koda's host: {e:#}"),
            );
        }
    }

    /// `/model`: the session's model, within its provider.
    pub(crate) async fn change_model(&mut self, model: &str) {
        match self
            .update_settings(Some(model.to_string()), false, true)
            .await
        {
            Ok(alias) => {
                save_choice(&self.client, &self.config, alias.as_deref()).await;
                let shown = match &alias {
                    Some(alias) => format!("{alias} ({})", self.config.model),
                    None => self.config.model.clone(),
                };
                crate::tui_output::ok_msg(&mut self.scroll_buffer, format!("Model: {shown}"));
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

    /// `model/list` for `ptype` at `base_url`.
    pub(crate) async fn list_models(&self, ptype: ProviderType, base_url: &str) -> Vec<Model> {
        self.client
            .request::<methods::ModelList>(ModelListParams {
                model_provider: Some(ptype.to_string()),
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
    pub(crate) async fn switch_provider(
        &mut self,
        ptype: ProviderType,
        base_url: String,
        model: Option<String>,
    ) {
        let same = ptype == self.config.provider_type && base_url == self.config.base_url;
        if same {
            match model {
                Some(model) => self.change_model(&model).await,
                None => crate::tui_output::dim_msg(
                    &mut self.scroll_buffer,
                    format!("Already on {ptype}."),
                ),
            }
            return;
        }
        let model = match model {
            Some(model) => Some(model),
            None if !ptype.requires_api_key() => self
                .list_models(ptype, &base_url)
                .await
                .into_iter()
                .find(|m| m.alias.is_none())
                .map(|m| m.id),
            None => None,
        };
        let started = self
            .client
            .request::<methods::SessionStart>(SessionStartParams {
                model_provider: Some(ptype.to_string()),
                base_url: Some(base_url),
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
        let id = started.session.id;
        let bg_agents = self
            .client
            .session_handles(&id)
            .map(|handles| handles.bg_agents)
            .unwrap_or_else(koda_core::child_agent::new_shared);
        self.session = TuiSession {
            id,
            db: self.session.db.clone(),
            bg_agents,
        };
        if let Err(e) = self.sync_settings(true).await {
            crate::tui_output::warn_msg(&mut self.scroll_buffer, format!("{e:#}"));
        }
        let alias = koda_core::model_alias::following_alias(&self.config.model);
        save_choice(&self.client, &self.config, alias).await;
        crate::tui_output::ok_msg(
            &mut self.scroll_buffer,
            format!(
                "New session on {} ({}). The one you left is in /resume.",
                self.config.provider_type.display_name(),
                self.config.model
            ),
        );
    }
}

/// What the command line named for a new session: each optional, the rest
/// left to the host's saved choice (`config/read`), as Codex's TUI passes
/// only its overrides to `thread/start`.
#[derive(Debug, Clone, Default)]
pub(crate) struct Named {
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
}

impl Named {
    /// What `overrides` says the command line gave, read from `config`.
    pub(crate) fn from_flags(config: &KodaConfig, overrides: super::ProviderOverrides) -> Self {
        Self {
            provider: overrides.provider.then(|| config.provider_type.to_string()),
            base_url: overrides.base_url.then(|| config.base_url.clone()),
            model: overrides.model.then(|| config.model.clone()),
        }
    }
}

/// Give a just-opened session the TUI's `mode`, which it has checked
/// against the sandbox, and return what the session runs with, with any
/// warnings the host sent meanwhile (an assumed context window, say).
pub(crate) async fn fetch_settings(
    client: &mut koda_client::InProcessClient,
    session_id: &str,
    mode: koda_core::trust::TrustMode,
) -> anyhow::Result<(SessionSettings, Vec<String>)> {
    client
        .request::<methods::SessionSettingsUpdate>(SessionSettingsUpdateParams {
            session_id: session_id.to_string(),
            model: None,
            effort: None,
            mode: Some(protocol_mode(mode)),
        })
        .await
        .map_err(host_error)?;
    let deadline = tokio::time::Instant::now() + SETTINGS_WAIT;
    let mut warnings = Vec::new();
    loop {
        match tokio::time::timeout_at(deadline, client.next_event()).await {
            Ok(Some(Outgoing::Notification(ServerNotification::SessionSettingsUpdated(n))))
                if n.session_id == session_id =>
            {
                return Ok((n.settings, warnings));
            }
            Ok(Some(Outgoing::Notification(ServerNotification::Warning(w)))) => {
                warnings.push(w.message);
            }
            // Sent after `initialize`, so still queued here.
            Ok(Some(Outgoing::Notification(ServerNotification::ConfigWarning(w)))) => {
                warnings.push(w.summary);
            }
            Ok(Some(_)) => {}
            Ok(None) => anyhow::bail!("Koda's host stopped"),
            Err(_) => anyhow::bail!("Koda's host didn't report the session's settings"),
        }
    }
}

/// Copy the host's settings into the TUI's config, which the status bar,
/// the banner and the completer read.
pub(crate) fn mirror(config: &mut KodaConfig, settings: &SessionSettings) {
    let window = settings.context_window as usize;
    config.provider_type = ProviderType::from_url_or_name("", Some(&settings.model_provider));
    config.base_url = settings.base_url.clone();
    config.model = settings.model.clone();
    config.model_settings.model = settings.model.clone();
    config.max_context_tokens = window;
    config.model_settings.max_context_tokens = window;
}

/// Save `config`'s provider and model as the choice a new session starts
/// from (`config/batchWrite` into config.toml); the model by `alias` when
/// it was chosen by one, so it follows its line. The endpoint is saved as
/// the provider's `[model_providers.<id>] base_url` when it isn't the
/// provider's default, and removed when it is, so the file stays short.
pub(crate) async fn save_choice(
    client: &koda_client::InProcessClient,
    config: &KodaConfig,
    alias: Option<&str>,
) {
    use koda_client::protocol::config::{ConfigBatchWriteParams, ConfigEdit, MergeStrategy};
    use serde_json::Value;
    let edit = |key: String, value: Value| ConfigEdit {
        key_path: key,
        value,
        merge_strategy: MergeStrategy::Replace,
    };
    let ptype = config.provider_type;
    let endpoint = if config.base_url == ptype.default_base_url() {
        Value::Null
    } else {
        Value::String(config.base_url.clone())
    };
    let saved = client
        .request::<methods::ConfigBatchWrite>(ConfigBatchWriteParams {
            edits: vec![
                edit("model_provider".into(), Value::String(ptype.to_string())),
                edit(format!("model_providers.{ptype}.base_url"), endpoint),
                edit(
                    "model".into(),
                    Value::String(alias.unwrap_or(&config.model).to_string()),
                ),
            ],
            expected_version: None,
        })
        .await;
    if let Err(e) = saved {
        tracing::warn!("couldn't save the model choice: {e}");
    }
}

/// Store `ptype`'s API key through the host (`modelProvider/apiKey/set`),
/// which also makes it the one this run uses. Returns the host's error.
pub(crate) async fn set_api_key(
    client: &koda_client::InProcessClient,
    ptype: ProviderType,
    key: &str,
) -> anyhow::Result<()> {
    use koda_client::protocol::config::ModelProviderApiKeySetParams;
    client
        .request::<methods::ModelProviderApiKeySet>(ModelProviderApiKeySetParams {
            provider_id: ptype.to_string(),
            api_key: key.to_string(),
        })
        .await
        .map(|_| ())
        .map_err(host_error)
}
