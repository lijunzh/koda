//! Koda's config file, `~/.config/koda/config.toml`: reading it, checking
//! it and editing it in place.
//!
//! Codex keeps its settings in `$CODEX_HOME/config.toml`
//! (`codex-rs/config`), and Koda follows it: one file a person can read and
//! edit, holding the model choice, each provider's endpoint and the MCP
//! servers. Secrets stay out of it (the keystore and the environment hold
//! them), and so does state (the database holds it).
//!
//! - **Reading** ([`ConfigFile::load`]) gives the typed [`ConfigToml`], the
//!   file's version and the keys Koda doesn't know. A file that isn't valid
//!   TOML, or has a value of the wrong type, is an error that names the line
//!   and column. A missing file is an empty config.
//! - **Unknown keys are loaded past, and reported** ([`Loaded::unknown_keys`])
//!   so a typo can't silently do nothing. Codex ignores them; Koda warns.
//! - **Editing** ([`ConfigFile::edit`]) changes keys with `toml_edit`, so
//!   comments, order and formatting survive, as Codex's `ConfigEditsBuilder`
//!   does. A batch is applied whole or not at all, refused if the result
//!   wouldn't load or adds a key Koda doesn't know, and refused if the file
//!   changed since the caller read `expected_version`.
//! - **Writes are atomic and private:** a temporary file in the same folder,
//!   renamed over the old one, created owner-only (0600 on Unix), since an
//!   MCP server's `env` can carry a token. Codex's `config.toml` ends up the
//!   same way through `write_atomically`.
//! - **Versions** are `sha256:` of the parsed values, as Codex's
//!   `version_for_toml`: reformatting or a comment doesn't change one.
//!
//! This crate knows nothing of the rest of Koda: the host maps
//! [`ConfigToml`] to the client protocol and to what sessions run with.

use std::collections::BTreeMap;
use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};
use toml_edit::{DocumentMut, Item, TableLike};

/// The file's name in Koda's config folder.
pub const FILE_NAME: &str = "config.toml";

/// The settings the file holds, in Codex's key names.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigToml {
    /// The model a new session starts with: an alias such as
    /// `claude-opus`, which follows its line, or a model id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The provider a new session starts with, such as `anthropic`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_provider: Option<String>,
    /// The reasoning effort a new session asks for, as `--reasoning-effort`
    /// gives it (Claude: `low` to `max`; other servers take it as sent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_reasoning_effort: Option<String>,
    /// The context window to assume, in tokens, as
    /// `--max-context-tokens` pins it: for a server that doesn't report
    /// its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_context_window: Option<u64>,
    /// Context used, in percent, past which a turn compacts after its
    /// final reply (1 to 100); 0 or none turns it off, as in Codex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_post_turn_compact_threshold_percent: Option<u8>,
    /// The mode a new session starts in, as `--mode` gives it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    /// Settings per provider, by provider id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub model_providers: BTreeMap<String, ModelProviderToml>,
    /// The MCP servers every new session starts, by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp_servers: BTreeMap<String, McpServerToml>,
    /// The TUI's own settings, read by the TUI from its machine's file.
    #[serde(default, skip_serializing_if = "TuiToml::is_empty")]
    pub tui: TuiToml,
    /// What sandboxed commands may do beyond Koda's defaults.
    #[serde(default, skip_serializing_if = "SandboxToml::is_empty")]
    pub sandbox: SandboxToml,
}

/// The mode a session runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Mutating actions run without asking, contained by the sandbox;
    /// destructive ones still ask.
    Auto,
    /// Every mutation asks first.
    Safe,
    /// Read-only, for a `koda exec` run that should only look.
    Plan,
}

impl Mode {
    /// The value as written in the file and given to `--mode`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Safe => "safe",
            Self::Plan => "plan",
        }
    }
}

/// One provider's settings: `[model_providers.<id>]`, in Codex's names.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelProviderToml {
    /// The provider's endpoint, used whenever a session runs on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Retries a failed request gets after the first try.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_max_retries: Option<u64>,
    /// Retries for a reply whose stream failed partway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_max_retries: Option<u64>,
    /// Milliseconds a reply may go silent before the connection counts as
    /// lost; 0 turns the limit off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_idle_timeout_ms: Option<u64>,
}

/// `[tui]`: settings the TUI reads from its own machine's file, as Codex's
/// TUI reads its `[tui]` table even against a remote host.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TuiToml {
    /// Colour code blocks and diffs; `false` for plain text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub syntax_highlight: Option<bool>,
}

impl TuiToml {
    fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// `[sandbox]`: what sandboxed commands may do beyond Koda's defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxToml {
    /// macOS: let commands listen on any local port and connect to any
    /// port on localhost, not only the servers Koda started. Codex keeps
    /// the same switch in its permission profiles' `network` table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_local_binding: Option<bool>,
}

impl SandboxToml {
    fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// One `-c key=value` from the command line: a dotted key and its value,
/// as Codex's `CliConfigOverrides` takes them. They sit over the file for
/// this run and are never written to it.
#[derive(Debug, Clone, PartialEq)]
pub struct Override {
    /// The dotted key path, such as `model_providers.anthropic.base_url`.
    pub key_path: String,
    /// The value.
    pub value: toml::Value,
}

/// Parse `key=value`. The value is read as TOML (`5`, `true`, `"text"`,
/// `["a"]`), and taken as a plain string when it isn't valid TOML, so
/// `-c model=claude-opus` needs no quotes, as in Codex.
pub fn parse_override(raw: &str) -> Result<Override, String> {
    let (key, value) = raw
        .split_once('=')
        .ok_or_else(|| format!("-c {raw:?}: expected key=value"))?;
    let key = key.trim();
    if key.is_empty() || key.split('.').any(str::is_empty) {
        return Err(format!("-c {raw:?}: {key:?} isn't a config key path"));
    }
    let value = value.trim();
    let parsed = toml::from_str::<toml::Table>(&format!("value = {value}"))
        .ok()
        .and_then(|mut t| t.remove("value"))
        .unwrap_or_else(|| toml::Value::String(value.to_string()));
    Ok(Override {
        key_path: key.to_string(),
        value: parsed,
    })
}

/// One MCP server: `[mcp_servers.<name>]`, in Codex's shape. A `command`
/// for a server Koda starts, or a `url` for one it reaches over streamable
/// HTTP. Whether a server makes sense (one transport, a valid name) is the
/// host's to check.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerToml {
    /// The command that starts the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Its arguments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Environment variables set for it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// The directory it starts in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// The server's URL, for streamable HTTP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The environment variable that holds the bearer token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token_env_var: Option<String>,
    /// Headers sent with every HTTP request.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub http_headers: BTreeMap<String, String>,
    /// Seconds the server has to start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_timeout_sec: Option<u64>,
    /// Seconds a tool call has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_timeout_sec: Option<u64>,
    /// A session doesn't start without it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
    /// Only these tools are offered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_tools: Option<Vec<String>>,
    /// These tools are never offered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_tools: Option<Vec<String>>,
}

/// The file as read, with the `-c` values over it.
#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    /// The settings in force: the file's, with `-c` values over them.
    pub config: ConfigToml,
    /// The `-c` values, as a table: the session-flags layer.
    pub flags: toml::Table,
    /// Every value in the file, Koda's keys or not.
    pub values: toml::Table,
    /// `sha256:` of [`Self::values`]; pass it back to [`ConfigFile::edit`].
    pub version: String,
    /// Keys Koda doesn't know, dotted (`mcp_servers.notes.timeout`).
    pub unknown_keys: Vec<String>,
    /// Whether the file exists.
    pub exists: bool,
}

impl Loaded {
    /// Every value's dotted key path, as Codex's `config/read` reports
    /// origins: tables are walked, arrays are values.
    pub fn key_paths(&self) -> Vec<String> {
        let mut paths = Vec::new();
        push_key_paths(&self.values, "", &mut paths);
        paths
    }

    /// The dotted key paths `-c` sets.
    pub fn flag_key_paths(&self) -> Vec<String> {
        let mut paths = Vec::new();
        push_key_paths(&self.flags, "", &mut paths);
        paths
    }

    /// `sha256:` of the `-c` values.
    pub fn flags_version(&self) -> String {
        version_of(&self.flags)
    }

    /// The `-c` value at the dotted `key_path`, if `-c` set one.
    pub fn flag_value(&self, key_path: &str) -> Option<&toml::Value> {
        let mut parts = key_path.split('.');
        let mut value = self.flags.get(parts.next()?)?;
        for part in parts {
            value = value.get(part)?;
        }
        Some(value)
    }
}

/// One change: a dotted key path and its new value, `null` to remove it.
/// An object becomes a table; objects inside it become inline tables.
#[derive(Debug, Clone, PartialEq)]
pub struct Edit {
    /// The dotted key path, such as `model_providers.anthropic.base_url`.
    pub key_path: String,
    /// The value; `null` removes the key.
    pub value: JsonValue,
}

/// Why the file couldn't be read or changed.
#[derive(Debug)]
pub enum ConfigError {
    /// It couldn't be read or written.
    Io {
        /// The file.
        path: PathBuf,
        /// What went wrong.
        error: std::io::Error,
    },
    /// It isn't valid, or a value has the wrong type. The message names
    /// the line and column.
    Parse {
        /// The file.
        path: PathBuf,
        /// What's wrong, and where.
        message: String,
    },
    /// An edit was refused; the file is unchanged.
    Edit(String),
    /// An edit names a key Koda doesn't know; the file is unchanged.
    UnknownKey(String),
    /// A `-c` value names a key Koda doesn't know, or has the wrong type.
    Override(String),
    /// The file changed since the caller read it; nothing was written.
    VersionConflict {
        /// The version the caller read.
        expected: String,
        /// The version on disk now.
        actual: String,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, error } => write!(f, "{}: {error}", path.display()),
            Self::Parse { path, message } => {
                write!(f, "{} isn't valid: {}", path.display(), message.trim_end())
            }
            Self::Edit(message) | Self::Override(message) => f.write_str(message),
            Self::UnknownKey(key) => write!(f, "unknown config key `{key}`"),
            Self::VersionConflict { .. } => {
                f.write_str("the config file changed since it was read; read it again and retry")
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { error, .. } => Some(error),
            _ => None,
        }
    }
}

/// The config file at one path, with this run's `-c` values.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigFile {
    path: PathBuf,
    overrides: Vec<Override>,
}

impl ConfigFile {
    /// The file at `path`, which needn't exist yet.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            overrides: Vec::new(),
        }
    }

    /// Lay this run's `-c` values over the file. Later ones win. Loading
    /// refuses a key Koda doesn't know there, as Codex does; edits still
    /// write only the file.
    pub fn with_overrides(mut self, overrides: Vec<Override>) -> Self {
        self.overrides = overrides;
        self
    }

    /// The file `config.toml` in `config_dir`.
    pub fn in_dir(config_dir: &Path) -> Self {
        Self::new(config_dir.join(FILE_NAME))
    }

    /// Where the file is.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the file. A missing file is an empty config.
    pub fn load(&self) -> Result<Loaded, ConfigError> {
        match self.read_text()? {
            Some(text) => self.parse(&text, true),
            None => self.parse("", false),
        }
    }

    /// Apply `edits` in order and save the file, all or nothing. Refused,
    /// with the file unchanged, when `expected_version` is given and the
    /// file has changed since, when the result wouldn't load, or when an
    /// edit adds a key Koda doesn't know. Returns the file as saved.
    pub fn edit(
        &self,
        expected_version: Option<&str>,
        edits: &[Edit],
    ) -> Result<Loaded, ConfigError> {
        let text = self.read_text()?;
        let before = self.parse(text.as_deref().unwrap_or(""), text.is_some())?;
        if let Some(expected) = expected_version
            && expected != before.version
        {
            return Err(ConfigError::VersionConflict {
                expected: expected.to_string(),
                actual: before.version,
            });
        }
        let mut doc: DocumentMut = text
            .as_deref()
            .unwrap_or("")
            .parse()
            .map_err(|e: toml_edit::TomlError| self.parse_error(e.to_string()))?;
        for edit in edits {
            apply(&mut doc, edit)?;
        }
        let new_text = doc.to_string();
        let after = self
            .parse(&new_text, true)
            .map_err(|e| ConfigError::Edit(format!("the change would leave {e}")))?;
        if let Some(added) = after
            .unknown_keys
            .iter()
            .find(|key| !before.unknown_keys.contains(key))
        {
            return Err(ConfigError::UnknownKey(added.clone()));
        }
        self.write_atomically(&new_text)?;
        Ok(after)
    }

    fn read_text(&self) -> Result<Option<String>, ConfigError> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => Ok(Some(text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(ConfigError::Io {
                path: self.path.clone(),
                error,
            }),
        }
    }

    fn parse_error(&self, message: String) -> ConfigError {
        ConfigError::Parse {
            path: self.path.clone(),
            message,
        }
    }

    fn parse(&self, text: &str, exists: bool) -> Result<Loaded, ConfigError> {
        let values: toml::Table =
            toml::from_str(text).map_err(|e| self.parse_error(e.to_string()))?;
        let mut unknown_keys = Vec::new();
        let deserializer =
            toml::Deserializer::parse(text).map_err(|e| self.parse_error(e.to_string()))?;
        let mut config: ConfigToml = serde_ignored::deserialize(deserializer, |path| {
            unknown_keys.push(path.to_string());
        })
        .map_err(|e| self.parse_error(e.to_string()))?;
        let mut flags = toml::Table::new();
        if !self.overrides.is_empty() {
            for o in &self.overrides {
                set_path(&mut flags, &o.key_path, o.value.clone())
                    .map_err(ConfigError::Override)?;
            }
            // The flags alone first, so a bad one is named as a flag.
            let mut unknown = Vec::new();
            serde_ignored::deserialize(toml::Value::Table(flags.clone()), |path| {
                unknown.push(path.to_string());
            })
            .map(|_: ConfigToml| ())
            .map_err(|e| ConfigError::Override(format!("-c: {e}")))?;
            if let Some(key) = unknown.first() {
                return Err(ConfigError::Override(format!(
                    "unknown configuration field `{key}` in -c/--config"
                )));
            }
            let mut merged = values.clone();
            merge(&mut merged, &flags);
            config = toml::Value::Table(merged)
                .try_into()
                .map_err(|e| ConfigError::Override(format!("-c: {e}")))?;
        }
        Ok(Loaded {
            config,
            flags,
            version: version_of(&values),
            values,
            unknown_keys,
            exists,
        })
    }

    fn write_atomically(&self, text: &str) -> Result<(), ConfigError> {
        let io = |error| ConfigError::Io {
            path: self.path.clone(),
            error,
        };
        let parent = match self.path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        std::fs::create_dir_all(parent).map_err(io)?;
        // `NamedTempFile` creates the file owner-only (0600) on Unix, so the
        // renamed file is too, whatever mode the old one had.
        let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(io)?;
        tmp.write_all(text.as_bytes()).map_err(io)?;
        tmp.as_file().sync_all().map_err(io)?;
        tmp.persist(&self.path).map_err(|e| io(e.error))?;
        Ok(())
    }
}

/// `sha256:` of the values as canonical JSON (keys sorted), as Codex's
/// `version_for_toml`.
fn version_of(values: &toml::Table) -> String {
    let canonical = serde_json::to_vec(values).unwrap_or_default();
    let digest = Sha256::digest(&canonical);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

/// Set `value` at the dotted `key_path` in `table`, making tables on the
/// way.
fn set_path(table: &mut toml::Table, key_path: &str, value: toml::Value) -> Result<(), String> {
    let parts: Vec<&str> = key_path.split('.').collect();
    let (last, parents) = parts.split_last().expect("split never yields nothing");
    let mut table = table;
    for (depth, part) in parents.iter().enumerate() {
        let entry = table
            .entry(part.to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        table = entry
            .as_table_mut()
            .ok_or_else(|| format!("-c: `{}` isn't a table", parents[..=depth].join(".")))?;
    }
    table.insert(last.to_string(), value);
    Ok(())
}

/// Lay `over` onto `base`: tables merge key by key, anything else replaces.
fn merge(base: &mut toml::Table, over: &toml::Table) {
    for (key, value) in over {
        match (base.get_mut(key), value) {
            (Some(toml::Value::Table(base)), toml::Value::Table(over)) => merge(base, over),
            _ => {
                base.insert(key.clone(), value.clone());
            }
        }
    }
}

fn push_key_paths(table: &toml::Table, prefix: &str, out: &mut Vec<String>) {
    for (key, value) in table {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match value {
            toml::Value::Table(inner) => push_key_paths(inner, &path, out),
            _ => out.push(path),
        }
    }
}

fn split_path(key_path: &str) -> Result<Vec<&str>, ConfigError> {
    let parts: Vec<&str> = key_path.split('.').collect();
    if parts.iter().any(|p| p.is_empty()) {
        return Err(ConfigError::Edit(format!(
            "`{key_path}` isn't a config key path"
        )));
    }
    Ok(parts)
}

fn apply(doc: &mut DocumentMut, edit: &Edit) -> Result<(), ConfigError> {
    let parts = split_path(&edit.key_path)?;
    let (last, parents) = parts.split_last().expect("split never yields nothing");
    // A server's keys in the order `McpServerToml` declares them, `command`
    // first, rather than JSON's alphabetical order.
    let order: &[&str] = if parents == [SERVERS_KEY] {
        &SERVER_KEY_ORDER
    } else {
        &[]
    };
    match to_item(&edit.value, order) {
        Some(item) => {
            let mut table: &mut dyn TableLike = doc.as_table_mut();
            for (depth, part) in parents.iter().enumerate() {
                let entry = table.entry(part).or_insert_with(|| {
                    let mut new = toml_edit::Table::new();
                    // `[model_providers]` holds only `[model_providers.x]`
                    // headers; don't print an empty header for it.
                    new.set_implicit(true);
                    Item::Table(new)
                });
                table = entry.as_table_like_mut().ok_or_else(|| {
                    ConfigError::Edit(format!("`{}` isn't a table", parents[..=depth].join(".")))
                })?;
            }
            replace(table, last, item);
        }
        None => remove(doc.as_table_mut(), parents, last),
    }
    Ok(())
}

/// Set `key` to `item`, keeping what a person wrote around the old one: a
/// value's comments, and a table's header comments and place in the file.
fn replace(table: &mut dyn TableLike, key: &str, mut item: Item) {
    match (table.get_mut(key), &mut item) {
        (Some(Item::Value(old)), Item::Value(new)) => {
            *new.decor_mut() = old.decor().clone();
            *old = new.clone();
        }
        (Some(Item::Table(old)), Item::Table(new)) => {
            *new.decor_mut() = old.decor().clone();
            if let Some(position) = old.position() {
                new.set_position(Some(position));
            }
            *old = new.clone();
        }
        _ => {
            table.insert(key, item);
        }
    }
}

/// Remove `parents.last`, then any parent table the removal left empty, so
/// no bare `[model_providers.x]` header is left behind.
fn remove(table: &mut dyn TableLike, parents: &[&str], last: &str) {
    match parents.split_first() {
        None => {
            table.remove(last);
        }
        Some((first, rest)) => {
            let Some(inner) = table.get_mut(first).and_then(Item::as_table_like_mut) else {
                return;
            };
            remove(inner, rest, last);
            if inner.is_empty() {
                table.remove(first);
            }
        }
    }
}

/// The key that holds MCP servers: `[mcp_servers.<name>]`.
const SERVERS_KEY: &str = "mcp_servers";

/// [`McpServerToml`]'s fields in declaration order.
const SERVER_KEY_ORDER: [&str; 12] = [
    "command",
    "args",
    "env",
    "cwd",
    "url",
    "bearer_token_env_var",
    "http_headers",
    "startup_timeout_sec",
    "tool_timeout_sec",
    "required",
    "enabled_tools",
    "disabled_tools",
];

/// A JSON value as TOML: `None` for `null`. An object becomes a table, its
/// keys in `order` first, then the rest; objects inside it become inline
/// tables.
fn to_item(value: &JsonValue, order: &[&str]) -> Option<Item> {
    match value {
        JsonValue::Object(map) => {
            let rank = |key: &str| order.iter().position(|k| *k == key).unwrap_or(order.len());
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by_key(|(key, _)| rank(key));
            let mut table = toml_edit::Table::new();
            for (key, value) in entries {
                if let Some(value) = to_value(value) {
                    table.insert(key, Item::Value(value));
                }
            }
            Some(Item::Table(table))
        }
        other => to_value(other).map(Item::Value),
    }
}

fn to_value(value: &JsonValue) -> Option<toml_edit::Value> {
    Some(match value {
        JsonValue::Null => return None,
        JsonValue::Bool(b) => (*b).into(),
        JsonValue::Number(n) => match n.as_i64() {
            Some(i) => i.into(),
            None => n.as_f64()?.into(),
        },
        JsonValue::String(s) => s.as_str().into(),
        JsonValue::Array(items) => {
            let mut array = toml_edit::Array::new();
            for item in items.iter().filter_map(to_value) {
                array.push(item);
            }
            array.into()
        }
        JsonValue::Object(map) => {
            let mut inline = toml_edit::InlineTable::new();
            for (key, value) in map {
                if let Some(value) = to_value(value) {
                    inline.insert(key, value);
                }
            }
            inline.into()
        }
    })
}

#[cfg(test)]
mod tests;
