//! The TUI's settings through the client protocol (PR 6c-1b).
//!
//! The host owns what a session runs with. The TUI asks for a change with
//! `session/settings/update` or, for another provider, a new session with
//! `session/start`, as Codex's TUI does with `thread/settings/update` and
//! `thread/start`, and mirrors what `session/settings/updated` reports into
//! its own `config` for the status bar, the banner and the completer. A
//! session's provider never changes; switching provider opens a new one,
//! and the one left stays in `/resume`.

use std::collections::VecDeque;
use std::time::Duration;

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

/// How long the host gets to report settings after a change.
const SETTINGS_WAIT: Duration = Duration::from_secs(15);

/// The host's own words, for an error the user reads.
fn host_error(error: koda_client::RequestError) -> anyhow::Error {
    match error {
        koda_client::RequestError::Server { error, .. } => anyhow::anyhow!(error.message),
        other => anyhow::anyhow!(other),
    }
}

/// How the host took a settings change.
pub(crate) enum Applied {
    /// Applied and reported: the alias the model is chosen by, if any.
    Now { alias: Option<String> },
    /// A turn runs on the session, Koda's own on its goal say: the host
    /// applies the change once it ends, and the report is mirrored then.
    AfterTurn,
}

/// What [`reported`] found.
enum Reported {
    /// The session's settings, after the change.
    Now(SessionSettings),
    /// A turn started first: the change waits for its end.
    AfterTurn,
}

/// Whether `message` is the session `session_id`'s settings report.
fn is_report(message: &Outgoing, session_id: &str) -> bool {
    matches!(
        message,
        Outgoing::Notification(ServerNotification::SessionSettingsUpdated(n))
            if n.session_id == session_id
    )
}

/// Wait for the host's report of `session_id`'s settings after a change
/// it has accepted. Every other message is set aside in `held`, in order,
/// for the TUI's readers; warnings the host sends meanwhile, such as an
/// assumed context window, go in `warnings`.
///
/// With `due`, a report for an earlier change not waited on comes first,
/// unless `held` has it already: that one is set aside too. With
/// `at_turn`, a `turn/started` for the session ends the wait: the host
/// applies the change once that turn ends, and the report comes then.
async fn reported(
    client: &mut koda_client::InProcessClient,
    held: &mut VecDeque<Outgoing>,
    session_id: &str,
    mut due: bool,
    at_turn: bool,
    warnings: &mut Vec<String>,
) -> anyhow::Result<Reported> {
    due &= !held.iter().any(|m| is_report(m, session_id));
    let deadline = tokio::time::Instant::now() + SETTINGS_WAIT;
    loop {
        let message = match tokio::time::timeout_at(deadline, client.next_event()).await {
            Ok(Some(message)) => message,
            Ok(None) => anyhow::bail!("Koda's host stopped"),
            Err(_) => anyhow::bail!("Koda's host didn't report the session's settings"),
        };
        match message {
            Outgoing::Notification(ServerNotification::SessionSettingsUpdated(n))
                if n.session_id == session_id && !due =>
            {
                // Newer than any report set aside, which it replaces.
                held.retain(|m| !is_report(m, session_id));
                return Ok(Reported::Now(n.session_settings));
            }
            Outgoing::Notification(ServerNotification::Warning(w))
                if w.session_id.as_deref().is_none_or(|s| s == session_id) =>
            {
                warnings.push(w.message);
            }
            // Sent after `initialize`, so still queued at start-up.
            Outgoing::Notification(ServerNotification::ConfigWarning(w)) => {
                warnings.push(w.summary);
            }
            message => {
                if is_report(&message, session_id) {
                    due = false;
                }
                let started = super::host_messages::turn_started(&message, session_id).is_some();
                held.push_back(message);
                if started && at_turn {
                    return Ok(Reported::AfterTurn);
                }
            }
        }
    }
}

impl TuiContext {
    /// Ask the host to change the session's model and/or take the TUI's
    /// mode, then mirror what it reports. While a turn runs on the session
    /// the host applies the change after it, and nothing here waits for
    /// that: the report is mirrored when it comes.
    async fn update_settings(
        &mut self,
        model: Option<String>,
        send_mode: bool,
        show_warnings: bool,
    ) -> anyhow::Result<Applied> {
        let mode = send_mode.then_some(self.mode);
        self.client
            .request::<methods::SessionSettingsUpdate>(SessionSettingsUpdateParams {
                session_id: self.session.id.clone(),
                model,
                effort: None,
                mode,
            })
            .await
            .map_err(host_error)?;
        let mut warnings = Vec::new();
        let due = self.settings_due.contains(&self.session.id);
        let reported = reported(
            &mut self.client,
            &mut self.held,
            &self.session.id,
            due,
            true,
            &mut warnings,
        )
        .await;
        if show_warnings {
            for warning in warnings {
                crate::tui_output::warn_msg(&mut self.scroll_buffer, warning);
            }
        }
        match reported? {
            Reported::Now(settings) => {
                self.settings_due.remove(&self.session.id);
                let alias = settings.alias.clone();
                self.adopt(settings);
                Ok(Applied::Now { alias })
            }
            Reported::AfterTurn => {
                self.settings_due.insert(self.session.id.clone());
                Ok(Applied::AfterTurn)
            }
        }
    }

    /// Mirror the host's settings into the TUI's own config.
    fn adopt(&mut self, settings: SessionSettings) {
        // The host's word on the mode: it may have changed what was asked.
        self.mode = settings.mode;
        self.renderer.model = settings.model.clone();
        self.settings = settings;
        self.model_names_stale = true;
    }

    /// The session's `session/settings/updated`, wherever it arrives: a
    /// change made during a turn is reported once the turn has ended, and
    /// nothing waits for it. True when it was one, for the caller to drop.
    pub(crate) fn mirror_settings(&mut self, message: &Outgoing) -> bool {
        let Outgoing::Notification(ServerNotification::SessionSettingsUpdated(n)) = message else {
            return false;
        };
        if n.session_id != self.session.id {
            return false;
        }
        self.settings_due.remove(&n.session_id);
        self.adopt(n.session_settings.clone());
        true
    }

    /// The main session's settings, as they were on leaving it for a side
    /// conversation, and the TUI's mode sent to it while its turn runs:
    /// the host applies the mode once the turn ends, and the report is
    /// mirrored then.
    pub(crate) async fn return_to_running(&mut self, settings: SessionSettings) {
        let mode = self.mode;
        self.adopt(settings);
        self.mode = mode;
        let sent = self
            .client
            .request::<methods::SessionSettingsUpdate>(SessionSettingsUpdateParams {
                session_id: self.session.id.clone(),
                mode: Some(mode),
                ..Default::default()
            })
            .await;
        match sent {
            Ok(_) => {
                self.settings_due.insert(self.session.id.clone());
            }
            Err(e) => crate::tui_output::warn_msg(
                &mut self.scroll_buffer,
                format!("The mode didn't reach Koda's host: {:#}", host_error(e)),
            ),
        }
    }

    /// After opening a session: give the host the TUI's mode, which the
    /// host checks against its rules, and mirror the session's settings,
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

    /// `/model`: the session's model, within its provider, and with
    /// [`ModelScope::Default`] the next start's choice too.
    pub(crate) async fn change_model(&mut self, model: &str, scope: ModelScope) {
        match self
            .update_settings(Some(model.to_string()), false, true)
            .await
        {
            Ok(Applied::AfterTurn) => {
                // Checked and accepted: saved as asked, as the host
                // resolves it again at the next start.
                if scope == ModelScope::Default {
                    let mut chosen = self.settings.clone();
                    chosen.model = model.to_string();
                    chosen.alias = None;
                    save_choice(&self.client, &chosen, &self.providers).await;
                }
                crate::tui_output::ok_msg(
                    &mut self.scroll_buffer,
                    format!("Model: {model}, once Koda's running turn ends"),
                );
            }
            Ok(Applied::Now { alias }) => {
                if scope == ModelScope::Default {
                    save_choice(&self.client, &self.settings, &self.providers).await;
                }
                let shown = match &alias {
                    Some(alias) => format!("{alias} ({})", self.settings.model),
                    None => self.settings.model.clone(),
                };
                let scope = match scope {
                    ModelScope::Default => "default",
                    ModelScope::Session => "this session",
                };
                crate::tui_output::ok_msg(
                    &mut self.scroll_buffer,
                    format!("Model: {shown} \u{00b7} {scope}"),
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
        if let Err(e) = self.sync_settings(true).await {
            crate::tui_output::warn_msg(&mut self.scroll_buffer, format!("{e:#}"));
        }
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

/// Give a just-opened session the TUI's `mode`, which the host checks
/// against its rules, and return what the session runs with, with any
/// warnings the host sent meanwhile (an assumed context window, say).
/// Everything else the host sends meanwhile goes in `held`, in order, for
/// the TUI's readers.
pub(crate) async fn fetch_settings(
    client: &mut koda_client::InProcessClient,
    held: &mut VecDeque<Outgoing>,
    session_id: &str,
    mode: koda_client::protocol::state::TrustMode,
) -> anyhow::Result<(SessionSettings, Vec<String>)> {
    client
        .request::<methods::SessionSettingsUpdate>(SessionSettingsUpdateParams {
            session_id: session_id.to_string(),
            model: None,
            effort: None,
            mode: Some(mode),
        })
        .await
        .map_err(host_error)?;
    let mut warnings = Vec::new();
    // The TUI can't start without them: a turn starting meanwhile is set
    // aside with the rest, and followed once the TUI has started.
    match reported(client, held, session_id, false, false, &mut warnings).await? {
        Reported::Now(settings) => Ok((settings, warnings)),
        Reported::AfterTurn => anyhow::bail!("Koda's host didn't report the session's settings"),
    }
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
