//! The host's side of `~/.config/koda/config.toml` ([`koda_config`]):
//! `config/read` and `config/batchWrite` in Codex's shapes, the warnings a
//! client gets after `initialize`, and the one-time move of the settings
//! earlier versions kept in the database.
//!
//! The file is read again for each `config/read`, `session/start` and MCP
//! reload, so a hand edit reaches the next session without a restart.
//! Codex's TUI does the same with `config.toml`. Two layers: the user's
//! file, and this run's `-c key=value` values over it (the session flags),
//! which are never written to the file.

use std::collections::BTreeMap;

use koda_config::{ConfigError, ConfigFile, Edit, Loaded};
use koda_core::config::{KodaConfig, ProviderType};
use koda_core::db::Database;
use koda_protocol::config::{
    ConfigBatchWriteParams, ConfigLayer, ConfigLayerMetadata, ConfigLayerSource, ConfigReadParams,
    ConfigReadResponse, ConfigWarningNotification, ConfigWriteErrorCode, ConfigWriteResponse, KEYS,
    KodaConfigValues, ModelProviderConfigValue, OverriddenMetadata, PROVIDERS_KEY,
    SandboxConfigValue, TuiConfigValue, WriteStatus,
};
use koda_protocol::mcp::{McpServerConfigValue, SERVERS_KEY};
use koda_protocol::rpc::{ErrorObject, codes};
use koda_protocol::state::TrustMode as ProtocolMode;
use serde_json::{Value, json};

use crate::session::internal;

/// Read the file; an unreadable or invalid file is an error naming the
/// line.
pub(crate) fn load(file: &ConfigFile) -> Result<Loaded, ErrorObject> {
    file.load().map_err(|e| internal(e.to_string()))
}

/// The file's settings in the protocol's shape.
pub(crate) fn values(loaded: &Loaded) -> KodaConfigValues {
    let config = &loaded.config;
    KodaConfigValues {
        model: config.model.clone(),
        model_provider: config.model_provider.clone(),
        model_reasoning_effort: config.model_reasoning_effort.clone(),
        model_context_window: config.model_context_window,
        model_post_turn_compact_threshold_percent: config.model_post_turn_compact_threshold_percent,
        mode: config.mode.map(protocol_mode),
        model_providers: config
            .model_providers
            .iter()
            .map(|(id, provider)| {
                let value = ModelProviderConfigValue {
                    base_url: provider.base_url.clone(),
                    request_max_retries: provider.request_max_retries,
                    stream_max_retries: provider.stream_max_retries,
                    stream_idle_timeout_ms: provider.stream_idle_timeout_ms,
                };
                (id.clone(), value)
            })
            .collect(),
        mcp_servers: servers(loaded),
        tui: TuiConfigValue {
            syntax_highlight: config.tui.syntax_highlight,
        },
        sandbox: SandboxConfigValue {
            allow_local_binding: config.sandbox.allow_local_binding,
        },
    }
}

fn protocol_mode(mode: koda_config::Mode) -> ProtocolMode {
    match mode {
        koda_config::Mode::Auto => ProtocolMode::Auto,
        koda_config::Mode::Safe => ProtocolMode::Safe,
        koda_config::Mode::Plan => ProtocolMode::Plan,
    }
}

/// The mode the file gives a new session, if any.
pub(crate) fn mode(loaded: &Loaded) -> Option<koda_core::trust::TrustMode> {
    loaded.config.mode.map(|mode| match mode {
        koda_config::Mode::Auto => koda_core::trust::TrustMode::Auto,
        koda_config::Mode::Safe => koda_core::trust::TrustMode::Safe,
        koda_config::Mode::Plan => koda_core::trust::TrustMode::Plan,
    })
}

/// Fill what a session runs with from the file (with `-c` over it), for
/// what nothing above it set: a flag, or the agent's JSON. Call once the
/// session's provider is known; its `[model_providers]` entry applies.
pub(crate) fn apply_settings(config: &mut KodaConfig, loaded: &Loaded) {
    let file = &loaded.config;
    if config.model_settings.reasoning_effort.is_none() {
        config.model_settings.reasoning_effort = file.model_reasoning_effort.clone();
    }
    if config.context_override.is_none()
        && let Some(tokens) = file.model_context_window.filter(|n| *n > 0)
    {
        let tokens = tokens as usize;
        config.context_override = Some(tokens);
        config.max_context_tokens = tokens;
        config.model_settings.max_context_tokens = tokens;
    }
    if let Some(percent) = file.model_post_turn_compact_threshold_percent {
        config.post_turn_compact_percent = percent.min(100);
    }
    // Retries are capped as Codex caps them.
    let retries =
        |n: u64| n.min(u64::from(koda_core::inference_helpers::MAX_RETRIES_SETTING)) as u32;
    if let Some(provider) = file.model_providers.get(&config.provider_type.to_string()) {
        if let Some(n) = provider.request_max_retries {
            config.request_max_retries = retries(n);
        }
        if let Some(n) = provider.stream_max_retries {
            config.stream_max_retries = retries(n);
        }
        config.stream_idle_timeout_ms = provider.stream_idle_timeout_ms;
    }
    koda_core::sandbox::set_local_binding_configured(
        file.sandbox.allow_local_binding.unwrap_or(false),
    );
}

/// The endpoint the file gives provider `ptype`, if any.
pub(crate) fn endpoint(loaded: &Loaded, ptype: ProviderType) -> Option<String> {
    loaded
        .config
        .model_providers
        .get(&ptype.to_string())
        .and_then(|p| p.base_url.clone())
}

fn user_layer(file: &ConfigFile) -> ConfigLayerSource {
    ConfigLayerSource::User {
        file: file.path().display().to_string(),
    }
}

/// `config/read`.
pub(crate) fn read(
    file: &ConfigFile,
    params: &ConfigReadParams,
) -> Result<ConfigReadResponse, ErrorObject> {
    let loaded = load(file)?;
    let user = ConfigLayerMetadata {
        name: user_layer(file),
        version: loaded.version.clone(),
    };
    let flags = flags_layer(&loaded);
    // A `-c` value wins over the file's for the same key.
    let mut origins: BTreeMap<String, ConfigLayerMetadata> = loaded
        .key_paths()
        .into_iter()
        .map(|path| (path, user.clone()))
        .collect();
    for path in loaded.flag_key_paths() {
        origins.insert(path, flags.clone());
    }
    let layers = params.include_layers.then(|| {
        let mut layers = vec![ConfigLayer {
            name: user_layer(file),
            version: loaded.version.clone(),
            config: serde_json::to_value(&loaded.values).unwrap_or(Value::Null),
        }];
        if !loaded.flags.is_empty() {
            layers.push(ConfigLayer {
                name: ConfigLayerSource::SessionFlags,
                version: loaded.flags_version(),
                config: serde_json::to_value(&loaded.flags).unwrap_or(Value::Null),
            });
        }
        layers
    });
    Ok(ConfigReadResponse {
        config: values(&loaded),
        origins,
        layers,
    })
}

fn flags_layer(loaded: &Loaded) -> ConfigLayerMetadata {
    ConfigLayerMetadata {
        name: ConfigLayerSource::SessionFlags,
        version: loaded.flags_version(),
    }
}

fn write_error(code: ConfigWriteErrorCode, message: impl Into<String>) -> ErrorObject {
    let mut error = ErrorObject::new(codes::INVALID_PARAMS, message);
    error.data = Some(json!({ "configWriteErrorCode": code }));
    error
}

/// `config/batchWrite`: check the edits, then apply them to the file all
/// or none. A key Koda doesn't take, or a value that doesn't fit its key,
/// refuses the whole batch with the file unchanged. Removing a key makes a
/// new session use the host's own. Open sessions take server changes on
/// `config/mcpServer/reload`.
pub(crate) fn write(
    file: &ConfigFile,
    params: ConfigBatchWriteParams,
) -> Result<ConfigWriteResponse, ErrorObject> {
    let invalid =
        |message: String| write_error(ConfigWriteErrorCode::ConfigValidationError, message);
    let current = load(file)?;
    let mut model = current.config.model.clone();
    let mut model_provider = current.config.model_provider.clone();
    let mut edits = Vec::new();
    for edit in params.edits {
        let key_path = edit.key_path;
        let value = if let Some(name) = crate::mcp::server_key(&key_path) {
            match crate::mcp::check_edit(name, edit.value)? {
                Some(server) => serde_json::to_value(crate::mcp::to_value(&server))
                    .map_err(|e| invalid(format!("{key_path}: {e}")))?,
                None => Value::Null,
            }
        } else if let Some((id, field)) = provider_key(&key_path) {
            crate::settings::provider(id)?;
            if field == "base_url" {
                string_or_null(&key_path, edit.value).map_err(invalid)?
            } else {
                edit.value
            }
        } else if key_path == "model" || key_path == "model_provider" {
            let value = string_or_null(&key_path, edit.value).map_err(invalid)?;
            let slot = if key_path == "model" {
                &mut model
            } else {
                &mut model_provider
            };
            *slot = value.as_str().map(str::to_string);
            value
        } else if KEYS.contains(&key_path.as_str())
            || key_path.starts_with("tui.")
            || key_path.starts_with("sandbox.")
        {
            // The file's schema checks the type; a window must be positive.
            if key_path == "model_context_window" && edit.value.as_u64() == Some(0) {
                return Err(invalid(
                    "model_context_window must be a positive number of tokens".into(),
                ));
            }
            edit.value
        } else {
            return Err(unknown_key(&key_path));
        };
        edits.push(Edit { key_path, value });
    }
    // The model choice as a whole: a known provider, and an alias that
    // belongs to it.
    if let Some(id) = &model_provider {
        let ptype = crate::settings::provider(id)?;
        if let Some(model) = &model {
            crate::settings::check_model(model, ptype)?;
        }
    }
    let saved = file
        .edit(params.expected_version.as_deref(), &edits)
        .map_err(|e| match e {
            ConfigError::VersionConflict { .. } => {
                write_error(ConfigWriteErrorCode::ConfigVersionConflict, e.to_string())
            }
            ConfigError::Edit(message) => invalid(message),
            ConfigError::UnknownKey(key) => unknown_key(&key),
            other => internal(format!("couldn't save the config: {other}")),
        })?;
    // A `-c` value for a written key wins until Koda restarts, as Codex
    // reports with `okOverridden`.
    let flagged = saved.flag_key_paths();
    let overridden = edits.iter().find_map(|edit| {
        flagged.iter().find(|flag| {
            *flag == &edit.key_path || flag.starts_with(&format!("{}.", edit.key_path))
        })
    });
    let (status, overridden_metadata) = match overridden {
        Some(flag) => (
            WriteStatus::OkOverridden,
            Some(OverriddenMetadata {
                message: format!(
                    "Saved, but `-c {flag}` given to this run wins over it until Koda restarts."
                ),
                overriding_layer: flags_layer(&saved),
                effective_value: flag_value(&saved, flag),
            }),
        ),
        None => (WriteStatus::Ok, None),
    };
    Ok(ConfigWriteResponse {
        status,
        version: saved.version.clone(),
        file_path: file.path().display().to_string(),
        config: values(&saved),
        overridden_metadata,
    })
}

fn unknown_key(key_path: &str) -> ErrorObject {
    write_error(
        ConfigWriteErrorCode::ConfigSchemaUnknownKey,
        format!(
            "unknown config key {key_path:?}; Koda takes {}, {PROVIDERS_KEY}.<id>.<key>, \
             {SERVERS_KEY}.<name>, tui.<key> and sandbox.<key>",
            KEYS.join(", "),
        ),
    )
}

/// The `-c` value at the dotted `path`, as JSON.
fn flag_value(loaded: &Loaded, path: &str) -> Value {
    loaded
        .flag_value(path)
        .and_then(|value| serde_json::to_value(value).ok())
        .unwrap_or(Value::Null)
}

/// If `key_path` is `model_providers.<id>.<key>`, the id and the key.
fn provider_key(key_path: &str) -> Option<(&str, &str)> {
    let rest = key_path.strip_prefix(PROVIDERS_KEY)?.strip_prefix('.')?;
    let (id, field) = rest.split_once('.')?;
    (!id.is_empty() && !field.is_empty() && !field.contains('.')).then_some((id, field))
}

fn string_or_null(key_path: &str, value: Value) -> Result<Value, String> {
    match value {
        Value::Null | Value::String(_) => Ok(value),
        other => Err(format!("{key_path} takes a string or null, not {other}")),
    }
}

/// What the client should hear about the file after `initialize`: keys
/// Koda doesn't know, which it loaded past, and servers it can't start.
pub(crate) fn warnings(file: &ConfigFile, loaded: &Loaded) -> Vec<ConfigWarningNotification> {
    let path = Some(file.path().display().to_string());
    let mut warnings: Vec<_> = loaded
        .unknown_keys
        .iter()
        .map(|key| ConfigWarningNotification {
            summary: format!(
                "Unknown key `{key}` in {}; Koda ignores it.",
                file.path().display()
            ),
            details: None,
            path: path.clone(),
        })
        .collect();
    for (name, value) in servers(loaded) {
        if let Err(e) =
            crate::mcp::check_edit(&name, serde_json::to_value(value).unwrap_or_default())
        {
            warnings.push(ConfigWarningNotification {
                summary: format!("MCP server `{name}` won't start: {}", e.message),
                details: None,
                path: path.clone(),
            });
        }
    }
    warnings
}

/// Move the settings earlier versions kept in the database into the file,
/// once: the saved model choice (`setting:last_provider`) and the MCP
/// servers (`mcp:<name>`). What the file already sets wins. The rows are
/// deleted once the file has them; if the file can't be written they stay,
/// and the next start tries again.
pub(crate) async fn migrate(db: &Database, file: &ConfigFile) {
    let choice = koda_core::last_provider::load_last_provider(db)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!("couldn't read the saved model choice: {e:#}");
            None
        });
    let servers = koda_core::mcp::config::load_legacy(db)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!("couldn't read the stored MCP servers: {e:#}");
            Default::default()
        });
    if choice.is_none() && servers.is_empty() {
        return;
    }
    let loaded = match file.load() {
        Ok(loaded) => loaded,
        Err(e) => {
            tracing::warn!("not moving saved settings into the config file: {e}");
            return;
        }
    };
    let mut edits = Vec::new();
    let set = |edits: &mut Vec<Edit>, key_path: String, value: Value| {
        edits.push(Edit { key_path, value });
    };
    if let Some(choice) = &choice {
        let ptype = ProviderType::from_url_or_name("", Some(&choice.provider_type));
        let id = ptype.to_string();
        if loaded.config.model_provider.is_none() && loaded.config.model.is_none() {
            set(&mut edits, "model_provider".into(), json!(id));
            let model = choice.alias.clone().unwrap_or_else(|| choice.model.clone());
            set(&mut edits, "model".into(), json!(model));
        }
        if choice.base_url != ptype.default_base_url() && endpoint(&loaded, ptype).is_none() {
            set(
                &mut edits,
                format!("{PROVIDERS_KEY}.{id}.base_url"),
                json!(choice.base_url),
            );
        }
    }
    let mut names: Vec<_> = servers.keys().collect();
    names.sort();
    for name in names {
        if !loaded.config.mcp_servers.contains_key(name) {
            let value =
                serde_json::to_value(crate::mcp::to_value(&servers[name])).unwrap_or_default();
            set(&mut edits, format!("{SERVERS_KEY}.{name}"), value);
        }
    }
    if !edits.is_empty()
        && let Err(e) = file.edit(Some(&loaded.version), &edits)
    {
        tracing::warn!("couldn't move saved settings into the config file: {e}");
        return;
    }
    if choice.is_some()
        && let Err(e) = koda_core::last_provider::delete_last_provider(db).await
    {
        tracing::warn!("couldn't delete the moved model choice: {e:#}");
    }
    if !servers.is_empty()
        && let Err(e) = koda_core::mcp::config::delete_legacy(db).await
    {
        tracing::warn!("couldn't delete the moved MCP servers: {e:#}");
    }
    tracing::info!(
        "moved the saved model choice and {} MCP server(s) into {}",
        servers.len(),
        file.path().display()
    );
}

/// The servers in the file, in the protocol's shape.
pub(crate) fn servers(loaded: &Loaded) -> BTreeMap<String, McpServerConfigValue> {
    loaded
        .config
        .mcp_servers
        .iter()
        .filter_map(|(name, server)| {
            let value = serde_json::to_value(server).ok()?;
            match serde_json::from_value(value) {
                Ok(value) => Some((name.clone(), value)),
                Err(e) => {
                    tracing::warn!(server = %name, "skipping MCP server: {e}");
                    None
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use koda_protocol::config::ConfigEdit;
    use koda_protocol::config::MergeStrategy;

    async fn scratch() -> (tempfile::TempDir, Database, ConfigFile) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::init(dir.path()).await.unwrap();
        let file = ConfigFile::in_dir(dir.path());
        (dir, db, file)
    }

    fn batch(edits: &[(&str, Value)], expected_version: Option<String>) -> ConfigBatchWriteParams {
        ConfigBatchWriteParams {
            edits: edits
                .iter()
                .map(|(key, value)| ConfigEdit {
                    key_path: key.to_string(),
                    value: value.clone(),
                    merge_strategy: MergeStrategy::Replace,
                })
                .collect(),
            expected_version,
        }
    }

    fn code(error: &ErrorObject) -> Value {
        error.data.as_ref().unwrap()["configWriteErrorCode"].clone()
    }

    #[tokio::test]
    async fn saved_settings_move_into_the_file_once() {
        let (_dir, db, file) = scratch().await;
        db.kv_set(
            "setting:last_provider",
            r#"{"provider_type":"lm-studio","base_url":"http://192.168.1.20:1234/v1","model":"qwen-coder","alias":null}"#,
        )
        .await
        .unwrap();
        db.kv_set(
            "mcp:notes",
            r#"{"transport":"stdio","command":"notes-mcp","args":["--quiet"]}"#,
        )
        .await
        .unwrap();

        migrate(&db, &file).await;
        let config = file.load().unwrap().config;
        assert_eq!(config.model_provider.as_deref(), Some("lm-studio"));
        assert_eq!(config.model.as_deref(), Some("qwen-coder"));
        assert_eq!(
            config.model_providers["lm-studio"].base_url.as_deref(),
            Some("http://192.168.1.20:1234/v1")
        );
        assert_eq!(
            config.mcp_servers["notes"].command.as_deref(),
            Some("notes-mcp")
        );
        assert_eq!(config.mcp_servers["notes"].args, ["--quiet"]);
        // The rows are gone, so a second start moves nothing.
        assert!(db.kv_get("setting:last_provider").await.unwrap().is_none());
        assert!(db.kv_list_prefix("mcp:").await.unwrap().is_empty());
        let before = std::fs::read_to_string(file.path()).unwrap();
        migrate(&db, &file).await;
        assert_eq!(std::fs::read_to_string(file.path()).unwrap(), before);
    }

    #[tokio::test]
    async fn what_the_file_sets_wins_and_an_alias_stays_an_alias() {
        let (_dir, db, file) = scratch().await;
        std::fs::write(file.path(), "# mine\nmodel = \"claude-sonnet\"\n").unwrap();
        db.kv_set(
            "setting:last_provider",
            r#"{"provider_type":"anthropic","base_url":"https://api.anthropic.com","model":"claude-opus-5-5","alias":"claude-opus"}"#,
        )
        .await
        .unwrap();
        migrate(&db, &file).await;
        // The file named a model, so the saved choice doesn't replace it, and
        // the default endpoint isn't written.
        assert_eq!(
            std::fs::read_to_string(file.path()).unwrap(),
            "# mine\nmodel = \"claude-sonnet\"\n"
        );
        assert!(db.kv_get("setting:last_provider").await.unwrap().is_none());

        let (_dir, db, file) = scratch().await;
        db.kv_set(
            "setting:last_provider",
            r#"{"provider_type":"anthropic","base_url":"https://api.anthropic.com","model":"claude-opus-5-5","alias":"claude-opus"}"#,
        )
        .await
        .unwrap();
        migrate(&db, &file).await;
        assert_eq!(
            std::fs::read_to_string(file.path()).unwrap(),
            "model_provider = \"anthropic\"\nmodel = \"claude-opus\"\n"
        );
    }

    #[tokio::test]
    async fn a_broken_file_keeps_the_rows_for_next_time() {
        let (_dir, db, file) = scratch().await;
        std::fs::write(file.path(), "model = \n").unwrap();
        db.kv_set("mcp:notes", r#"{"transport":"stdio","command":"n"}"#)
            .await
            .unwrap();
        migrate(&db, &file).await;
        assert_eq!(db.kv_list_prefix("mcp:").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn writes_check_keys_values_and_versions() {
        let (_dir, _db, file) = scratch().await;
        let unknown = write(&file, batch(&[("theme", json!("dark"))], None)).unwrap_err();
        assert_eq!(code(&unknown), "configSchemaUnknownKey");
        let wrong = write(&file, batch(&[("model", json!(5))], None)).unwrap_err();
        assert_eq!(code(&wrong), "configValidationError");
        let provider = write(
            &file,
            batch(
                &[("model_providers.nope.base_url", json!("http://x"))],
                None,
            ),
        )
        .unwrap_err();
        assert!(
            provider.message.contains("unknown provider"),
            "{provider:?}"
        );
        // An alias of another provider than the one saved is refused, and
        // nothing is written.
        let mismatch = write(
            &file,
            batch(
                &[
                    ("model_provider", json!("lm-studio")),
                    ("model", json!("claude-opus")),
                ],
                None,
            ),
        )
        .unwrap_err();
        assert!(mismatch.message.contains("claude-opus"), "{mismatch:?}");
        assert!(!file.path().exists());

        let written = write(
            &file,
            batch(
                &[
                    ("model_provider", json!("anthropic")),
                    ("model", json!("claude-opus")),
                    (
                        "model_providers.lm-studio.base_url",
                        json!("http://10.0.0.2:1234/v1"),
                    ),
                ],
                None,
            ),
        )
        .unwrap();
        assert_eq!(written.config.model.as_deref(), Some("claude-opus"));
        assert_eq!(written.file_path, file.path().display().to_string());
        let stale = write(
            &file,
            batch(
                &[("model", json!("claude-sonnet"))],
                Some("sha256:old".into()),
            ),
        )
        .unwrap_err();
        assert_eq!(code(&stale), "configVersionConflict");
        write(
            &file,
            batch(&[("model", json!("claude-sonnet"))], Some(written.version)),
        )
        .unwrap();
        // `null` removes an endpoint, and the table it leaves empty.
        write(
            &file,
            batch(&[("model_providers.lm-studio.base_url", Value::Null)], None),
        )
        .unwrap();
        assert!(
            !std::fs::read_to_string(file.path())
                .unwrap()
                .contains("model_providers")
        );
    }

    #[tokio::test]
    async fn reads_say_where_each_value_comes_from() {
        let (_dir, _db, file) = scratch().await;
        std::fs::write(
            file.path(),
            "model = \"m\"\nextra = 1\n\n[model_providers.lm-studio]\nbase_url = \"http://x\"\n",
        )
        .unwrap();
        let read = read(&file, &ConfigReadParams::default()).unwrap();
        assert_eq!(
            read.origins.keys().collect::<Vec<_>>(),
            ["extra", "model", "model_providers.lm-studio.base_url"]
        );
        let origin = &read.origins["model"];
        assert_eq!(
            origin.name,
            ConfigLayerSource::User {
                file: file.path().display().to_string()
            }
        );
        assert!(origin.version.starts_with("sha256:"));
        assert_eq!(read.layers, None);
        assert_eq!(
            read.config.model_providers["lm-studio"].base_url.as_deref(),
            Some("http://x")
        );

        let layered = read_layers(&file);
        assert_eq!(layered[0].config["extra"], 1);
    }

    fn read_layers(file: &ConfigFile) -> Vec<ConfigLayer> {
        read(
            file,
            &ConfigReadParams {
                include_layers: true,
            },
        )
        .unwrap()
        .layers
        .unwrap()
    }

    #[tokio::test]
    async fn unknown_keys_and_servers_that_cant_start_are_warned_about() {
        let (_dir, _db, file) = scratch().await;
        std::fs::write(
            file.path(),
            "modle = \"m\"\n\n[mcp_servers.both]\ncommand = \"a\"\nurl = \"http://b\"\n",
        )
        .unwrap();
        let loaded = file.load().unwrap();
        let summaries: Vec<String> = warnings(&file, &loaded)
            .into_iter()
            .map(|w| w.summary)
            .collect();
        assert_eq!(summaries.len(), 2, "{summaries:?}");
        assert!(
            summaries[0].starts_with("Unknown key `modle` in "),
            "{summaries:?}"
        );
        assert!(
            summaries[1].starts_with("MCP server `both` won't start"),
            "{summaries:?}"
        );
    }

    /// The file fills what a session runs with where nothing above it set
    /// a value; a flag or the agent's JSON keeps its own.
    #[tokio::test]
    async fn the_file_fills_what_nothing_named() {
        let (_dir, _db, file) = scratch().await;
        std::fs::write(
            file.path(),
            "model_reasoning_effort = \"high\"\nmodel_context_window = 50000\n\
             model_post_turn_compact_threshold_percent = 85\n\n\
             [model_providers.lm-studio]\nrequest_max_retries = 7\nstream_max_retries = 2\n\
             stream_idle_timeout_ms = 90000\n\n[model_providers.anthropic]\nrequest_max_retries = 1\n",
        )
        .unwrap();
        let loaded = file.load().unwrap();

        let mut config = KodaConfig::default_for_testing(ProviderType::LMStudio);
        apply_settings(&mut config, &loaded);
        assert_eq!(
            config.model_settings.reasoning_effort.as_deref(),
            Some("high")
        );
        assert_eq!(config.context_override, Some(50_000));
        assert_eq!(config.max_context_tokens, 50_000);
        assert_eq!(config.post_turn_compact_percent, 85);
        // The session's provider's table, not another's.
        assert_eq!(config.request_max_retries, 7);
        assert_eq!(config.stream_max_retries, 2);
        assert_eq!(config.stream_idle_timeout_ms, Some(90_000));

        // A flag (or the agent's JSON) keeps its value.
        let mut named = KodaConfig::default_for_testing(ProviderType::LMStudio);
        named.model_settings.reasoning_effort = Some("low".into());
        named.context_override = Some(8_192);
        apply_settings(&mut named, &loaded);
        assert_eq!(
            named.model_settings.reasoning_effort.as_deref(),
            Some("low")
        );
        assert_eq!(named.context_override, Some(8_192));
    }

    /// A `-c` value wins over the file's, `config/read` names it as the
    /// session flags' value, and a write it overrides says so.
    #[tokio::test]
    async fn flags_sit_over_the_file_in_reads_and_writes() {
        let (dir, _db, _) = scratch().await;
        std::fs::write(
            dir.path().join("config.toml"),
            "model = \"claude-sonnet\"\n",
        )
        .unwrap();
        let file = ConfigFile::in_dir(dir.path()).with_overrides(vec![
            koda_config::parse_override("model=claude-opus").unwrap(),
        ]);
        let read = read(
            &file,
            &ConfigReadParams {
                include_layers: true,
            },
        )
        .unwrap();
        assert_eq!(read.config.model.as_deref(), Some("claude-opus"));
        assert_eq!(read.origins["model"].name, ConfigLayerSource::SessionFlags);
        let layers = read.layers.unwrap();
        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0].config["model"], "claude-sonnet");
        assert_eq!(layers[1].name, ConfigLayerSource::SessionFlags);

        let written = write(&file, batch(&[("model", json!("claude-haiku"))], None)).unwrap();
        assert_eq!(written.status, WriteStatus::OkOverridden);
        let meta = written.overridden_metadata.unwrap();
        assert_eq!(meta.effective_value, "claude-opus");
        assert!(
            std::fs::read_to_string(file.path())
                .unwrap()
                .contains("claude-haiku")
        );

        let other = write(
            &file,
            batch(&[("model_reasoning_effort", json!("high"))], None),
        )
        .unwrap();
        assert_eq!(other.status, WriteStatus::Ok);
        assert_eq!(other.overridden_metadata, None);
    }

    #[tokio::test]
    async fn the_new_keys_are_written_and_checked() {
        let (_dir, _db, file) = scratch().await;
        write(
            &file,
            batch(
                &[
                    ("model_context_window", json!(65536)),
                    ("mode", json!("safe")),
                    ("model_providers.anthropic.request_max_retries", json!(6)),
                    ("tui.syntax_highlight", json!(false)),
                    ("sandbox.allow_local_binding", json!(true)),
                ],
                None,
            ),
        )
        .unwrap();
        let config = file.load().unwrap().config;
        assert_eq!(config.model_context_window, Some(65_536));
        assert_eq!(config.mode, Some(koda_config::Mode::Safe));
        assert_eq!(
            config.model_providers["anthropic"].request_max_retries,
            Some(6)
        );
        assert_eq!(config.tui.syntax_highlight, Some(false));
        assert_eq!(config.sandbox.allow_local_binding, Some(true));

        let zero = write(&file, batch(&[("model_context_window", json!(0))], None)).unwrap_err();
        assert_eq!(code(&zero), "configValidationError");
        let bad_mode = write(&file, batch(&[("mode", json!("yolo"))], None)).unwrap_err();
        assert_eq!(code(&bad_mode), "configValidationError");
        let unknown = write(&file, batch(&[("tui.theme", json!("dark"))], None)).unwrap_err();
        assert_eq!(code(&unknown), "configSchemaUnknownKey");
        let provider_field = write(
            &file,
            batch(&[("model_providers.anthropic.colour", json!(1))], None),
        )
        .unwrap_err();
        assert_eq!(code(&provider_field), "configSchemaUnknownKey");
    }
}
