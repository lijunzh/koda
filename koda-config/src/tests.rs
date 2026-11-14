use super::*;
use serde_json::json;

fn file_with(text: &str) -> (tempfile::TempDir, ConfigFile) {
    let dir = tempfile::tempdir().unwrap();
    let file = ConfigFile::in_dir(dir.path());
    std::fs::write(file.path(), text).unwrap();
    (dir, file)
}

fn edit(key_path: &str, value: JsonValue) -> Edit {
    Edit {
        key_path: key_path.to_string(),
        value,
    }
}

fn text(file: &ConfigFile) -> String {
    std::fs::read_to_string(file.path()).unwrap()
}

#[test]
fn a_missing_file_is_an_empty_config() {
    let dir = tempfile::tempdir().unwrap();
    let loaded = ConfigFile::in_dir(dir.path()).load().unwrap();
    assert_eq!(loaded.config, ConfigToml::default());
    assert!(!loaded.exists);
    assert!(loaded.unknown_keys.is_empty());
    assert_eq!(loaded.version, version_of(&toml::Table::new()));
}

#[test]
fn the_file_loads_into_its_typed_form() {
    let (_dir, file) = file_with(
        r#"
model = "claude-opus"
model_provider = "anthropic"

[model_providers.lmstudio]
base_url = "http://192.168.1.20:1234/v1"

[mcp_servers.notes]
command = "notes-mcp"
args = ["--quiet"]
env = { NOTES_KEY = "x" }
required = true
"#,
    );
    let loaded = file.load().unwrap();
    assert!(loaded.exists);
    assert_eq!(loaded.config.model.as_deref(), Some("claude-opus"));
    assert_eq!(loaded.config.model_provider.as_deref(), Some("anthropic"));
    assert_eq!(
        loaded.config.model_providers["lmstudio"]
            .base_url
            .as_deref(),
        Some("http://192.168.1.20:1234/v1")
    );
    let notes = &loaded.config.mcp_servers["notes"];
    assert_eq!(notes.command.as_deref(), Some("notes-mcp"));
    assert_eq!(notes.args, ["--quiet"]);
    assert_eq!(notes.env["NOTES_KEY"], "x");
    assert!(notes.required);
    assert_eq!(
        loaded.key_paths(),
        [
            "mcp_servers.notes.args",
            "mcp_servers.notes.command",
            "mcp_servers.notes.env.NOTES_KEY",
            "mcp_servers.notes.required",
            "model",
            "model_provider",
            "model_providers.lmstudio.base_url",
        ]
    );
}

#[test]
fn a_broken_file_names_the_line() {
    let (_dir, file) = file_with("model = \"x\"\nmodel_provider = \n");
    let err = file.load().unwrap_err().to_string();
    assert!(err.contains("config.toml isn't valid"), "{err}");
    assert!(err.contains("line 2"), "{err}");
}

#[test]
fn a_value_of_the_wrong_type_names_the_line() {
    let (_dir, file) = file_with("model_provider = \"anthropic\"\nmodel = 3\n");
    let err = file.load().unwrap_err().to_string();
    assert!(err.contains("line 2"), "{err}");
}

#[test]
fn unknown_keys_are_loaded_past_and_reported() {
    let (_dir, file) = file_with(
        "modle = \"claude-opus\"\nmodel = \"claude-sonnet\"\n\n[mcp_servers.notes]\ncommand = \"n\"\ntimeout = 5\n",
    );
    let loaded = file.load().unwrap();
    assert_eq!(loaded.config.model.as_deref(), Some("claude-sonnet"));
    assert_eq!(loaded.unknown_keys, ["mcp_servers.notes.timeout", "modle"]);
}

#[test]
fn an_edit_keeps_comments_and_order() {
    let (_dir, file) = file_with(
        "# My Koda settings\nmodel = \"claude-sonnet\" # the daily driver\n\n# Servers\n[mcp_servers.notes]\ncommand = \"notes-mcp\"\n",
    );
    let saved = file
        .edit(
            None,
            &[
                edit("model", json!("claude-opus")),
                edit(
                    "model_providers.anthropic.base_url",
                    json!("https://proxy.example/v1"),
                ),
            ],
        )
        .unwrap();
    let text = text(&file);
    assert!(
        text.starts_with("# My Koda settings\nmodel = \"claude-opus\" # the daily driver\n"),
        "{text}"
    );
    assert!(text.contains("# Servers\n[mcp_servers.notes]"), "{text}");
    assert!(
        text.contains("[model_providers.anthropic]\nbase_url = \"https://proxy.example/v1\""),
        "{text}"
    );
    assert!(
        !text.contains("[model_providers]\n"),
        "an empty parent header: {text}"
    );
    assert_eq!(saved, file.load().unwrap());
}

#[test]
fn a_server_is_written_as_its_own_table() {
    let dir = tempfile::tempdir().unwrap();
    let file = ConfigFile::in_dir(&dir.path().join("koda"));
    let server = json!({
        "command": "notes-mcp",
        "args": ["--quiet"],
        "env": {"NOTES_KEY": "x"},
        "startup_timeout_sec": 10,
    });
    let saved = file
        .edit(None, &[edit("mcp_servers.notes", server)])
        .unwrap();
    let text = text(&file);
    assert!(
        text.contains("[mcp_servers.notes]\ncommand = \"notes-mcp\""),
        "{text}"
    );
    assert!(text.contains("env = { NOTES_KEY = \"x\" }"), "{text}");
    let notes = &saved.config.mcp_servers["notes"];
    assert_eq!(notes.args, ["--quiet"]);
    assert_eq!(notes.startup_timeout_sec, Some(10));
}

#[test]
fn null_removes_a_key_and_the_tables_it_leaves_empty() {
    let (_dir, file) = file_with(
        "model = \"m\"\n\n[model_providers.lmstudio]\nbase_url = \"http://x\"\n\n[mcp_servers.a]\ncommand = \"a\"\n\n[mcp_servers.b]\ncommand = \"b\"\n",
    );
    file.edit(
        None,
        &[
            edit("model_providers.lmstudio.base_url", JsonValue::Null),
            edit("mcp_servers.a", JsonValue::Null),
            edit("model_provider", JsonValue::Null),
        ],
    )
    .unwrap();
    assert_eq!(
        text(&file),
        "model = \"m\"\n\n[mcp_servers.b]\ncommand = \"b\"\n"
    );
}

#[test]
fn a_stale_version_is_refused_and_nothing_is_written() {
    let (_dir, file) = file_with("model = \"a\"\n");
    let read = file.load().unwrap();
    std::fs::write(file.path(), "model = \"b\"\n").unwrap();
    let err = file
        .edit(Some(&read.version), &[edit("model", json!("c"))])
        .unwrap_err();
    assert!(
        matches!(err, ConfigError::VersionConflict { .. }),
        "{err:?}"
    );
    assert_eq!(text(&file), "model = \"b\"\n");

    let now = file.load().unwrap();
    file.edit(Some(&now.version), &[edit("model", json!("c"))])
        .unwrap();
    assert_eq!(text(&file), "model = \"c\"\n");
}

#[test]
fn comments_and_layout_dont_change_the_version() {
    let (_dir, a) = file_with("model = \"m\"\n[model_providers.x]\nbase_url = \"u\"\n");
    let (_dir2, b) = file_with(
        "# hello\nmodel_providers = { x = { base_url = \"u\" } }\nmodel = \"m\" # same values\n",
    );
    assert_eq!(a.load().unwrap().version, b.load().unwrap().version);
}

#[test]
fn an_edit_that_wouldnt_load_or_adds_an_unknown_key_is_refused() {
    let (_dir, file) = file_with("model = \"a\"\nlegacy = 1\n");
    let wrong_type = file.edit(None, &[edit("model", json!(3))]).unwrap_err();
    assert!(matches!(wrong_type, ConfigError::Edit(_)), "{wrong_type:?}");
    let unknown = file
        .edit(
            None,
            &[edit("model", json!("b")), edit("modle", json!("c"))],
        )
        .unwrap_err();
    assert_eq!(unknown.to_string(), "unknown config key `modle`");
    assert_eq!(
        text(&file),
        "model = \"a\"\nlegacy = 1\n",
        "nothing written"
    );
    // A key that was already unknown doesn't block other edits.
    file.edit(None, &[edit("model", json!("b"))]).unwrap();
    assert_eq!(text(&file), "model = \"b\"\nlegacy = 1\n");
}

#[test]
fn a_bad_key_path_is_refused() {
    let (_dir, file) = file_with("");
    for path in ["", "a..b", ".a", "a."] {
        assert!(
            matches!(
                file.edit(None, &[edit(path, json!("x"))]),
                Err(ConfigError::Edit(_))
            ),
            "{path:?}"
        );
    }
    let (_dir2, not_a_table) = file_with("model = \"a\"\n");
    let err = not_a_table
        .edit(None, &[edit("model.inner", json!("x"))])
        .unwrap_err();
    assert_eq!(err.to_string(), "`model` isn't a table");
}

#[test]
fn the_server_key_order_matches_the_struct() {
    let server = McpServerToml {
        command: Some("c".into()),
        args: vec!["a".into()],
        env: BTreeMap::from([("K".into(), "v".into())]),
        cwd: Some("d".into()),
        url: Some("u".into()),
        bearer_token_env_var: Some("T".into()),
        http_headers: BTreeMap::from([("H".into(), "v".into())]),
        startup_timeout_sec: Some(1),
        tool_timeout_sec: Some(2),
        required: true,
        enabled_tools: Some(vec![]),
        disabled_tools: Some(vec![]),
    };
    // Serializing a struct writes its fields in declaration order; read the
    // keys back in that order (a `serde_json::Map` would sort them).
    struct Keys(Vec<String>);
    impl<'de> Deserialize<'de> for Keys {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct V;
            impl<'de> serde::de::Visitor<'de> for V {
                type Value = Keys;
                fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                    f.write_str("a map")
                }
                fn visit_map<A: serde::de::MapAccess<'de>>(
                    self,
                    mut map: A,
                ) -> Result<Keys, A::Error> {
                    let mut keys = Vec::new();
                    while let Some((key, _)) = map.next_entry::<String, serde::de::IgnoredAny>()? {
                        keys.push(key);
                    }
                    Ok(Keys(keys))
                }
            }
            d.deserialize_map(V)
        }
    }
    let Keys(keys) = serde_json::from_str(&serde_json::to_string(&server).unwrap()).unwrap();
    assert_eq!(keys, SERVER_KEY_ORDER);
}

#[test]
fn replacing_a_server_keeps_its_place_and_header_comment() {
    let (_dir, file) = file_with(
        "[mcp_servers.a]\ncommand = \"a\"\n\n# The notes server\n[mcp_servers.b]\ncommand = \"b\"\n\n[mcp_servers.c]\ncommand = \"c\"\n",
    );
    file.edit(
        None,
        &[edit(
            "mcp_servers.b",
            json!({"url": "https://b", "command": null}),
        )],
    )
    .unwrap();
    assert_eq!(
        text(&file),
        "[mcp_servers.a]\ncommand = \"a\"\n\n# The notes server\n[mcp_servers.b]\nurl = \"https://b\"\n\n[mcp_servers.c]\ncommand = \"c\"\n"
    );
}

#[cfg(unix)]
#[test]
fn the_file_is_written_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let mode =
        |file: &ConfigFile| std::fs::metadata(file.path()).unwrap().permissions().mode() & 0o777;
    let dir = tempfile::tempdir().unwrap();
    let file = ConfigFile::in_dir(dir.path());
    file.edit(None, &[edit("model", json!("a"))]).unwrap();
    assert_eq!(mode(&file), 0o600);

    std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o644)).unwrap();
    file.edit(None, &[edit("model", json!("b"))]).unwrap();
    assert_eq!(mode(&file), 0o600);
}

#[test]
fn servers_written_inline_by_hand_take_an_edit() {
    let (_dir, file) = file_with("mcp_servers = { a = { command = \"a\" } }\n");
    let saved = file
        .edit(
            None,
            &[
                edit("mcp_servers.b", json!({"command": "b"})),
                edit("mcp_servers.a", JsonValue::Null),
            ],
        )
        .unwrap();
    assert_eq!(saved.config.mcp_servers.keys().collect::<Vec<_>>(), ["b"]);
}

#[test]
fn the_settings_keys_load() {
    let (_dir, file) = file_with(
        r#"
model_reasoning_effort = "high"
model_context_window = 131072
model_post_turn_compact_threshold_percent = 80
mode = "safe"

[model_providers.anthropic]
request_max_retries = 6
stream_max_retries = 3
stream_idle_timeout_ms = 600000

[tui]
syntax_highlight = false

[sandbox]
allow_local_binding = true
"#,
    );
    let config = file.load().unwrap().config;
    assert_eq!(config.model_reasoning_effort.as_deref(), Some("high"));
    assert_eq!(config.model_context_window, Some(131_072));
    assert_eq!(config.model_post_turn_compact_threshold_percent, Some(80));
    assert_eq!(config.mode, Some(Mode::Safe));
    let anthropic = &config.model_providers["anthropic"];
    assert_eq!(anthropic.request_max_retries, Some(6));
    assert_eq!(anthropic.stream_max_retries, Some(3));
    assert_eq!(anthropic.stream_idle_timeout_ms, Some(600_000));
    assert_eq!(config.tui.syntax_highlight, Some(false));
    assert_eq!(config.sandbox.allow_local_binding, Some(true));

    let (_dir2, bad) = file_with("mode = \"yolo\"\n");
    let err = bad.load().unwrap_err().to_string();
    assert!(err.contains("line 1"), "{err}");
}

#[test]
fn overrides_parse_as_toml_or_as_text() {
    let o = parse_override("model_context_window=65536").unwrap();
    assert_eq!(o.value, toml::Value::Integer(65_536));
    let o = parse_override("model=claude-opus").unwrap();
    assert_eq!(o.value, toml::Value::String("claude-opus".into()));
    let o = parse_override("model=\"claude-opus\"").unwrap();
    assert_eq!(o.value, toml::Value::String("claude-opus".into()));
    let o = parse_override("sandbox.allow_local_binding=true").unwrap();
    assert_eq!(o.key_path, "sandbox.allow_local_binding");
    assert_eq!(o.value, toml::Value::Boolean(true));
    assert!(parse_override("model").is_err());
    assert!(parse_override("=x").is_err());
    assert!(parse_override("a..b=x").is_err());
}

#[test]
fn overrides_sit_over_the_file_and_are_never_written() {
    let (_dir, file) = file_with(
        "# mine\nmodel = \"claude-sonnet\"\n\n[model_providers.lm-studio]\nbase_url = \"http://a\"\n",
    );
    let file = file.with_overrides(vec![
        parse_override("model=claude-opus").unwrap(),
        parse_override("model_providers.lm-studio.request_max_retries=9").unwrap(),
    ]);
    let loaded = file.load().unwrap();
    assert_eq!(loaded.config.model.as_deref(), Some("claude-opus"));
    let lm = &loaded.config.model_providers["lm-studio"];
    assert_eq!(
        lm.base_url.as_deref(),
        Some("http://a"),
        "the file's sibling stays"
    );
    assert_eq!(lm.request_max_retries, Some(9));
    assert_eq!(
        loaded.flag_key_paths(),
        ["model", "model_providers.lm-studio.request_max_retries"]
    );
    // The file's own version and values are the file's alone.
    assert_eq!(loaded.values["model"].as_str(), Some("claude-sonnet"));

    // An edit writes the file only; the flags still win in what it returns.
    let saved = file
        .edit(None, &[edit("model", json!("claude-haiku"))])
        .unwrap();
    assert_eq!(saved.config.model.as_deref(), Some("claude-opus"));
    assert!(text(&file).contains("model = \"claude-haiku\""));
    assert!(!text(&file).contains("request_max_retries"));
}

#[test]
fn an_unknown_or_mistyped_override_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let unknown = ConfigFile::in_dir(dir.path())
        .with_overrides(vec![parse_override("modle=x").unwrap()])
        .load()
        .unwrap_err();
    assert!(matches!(unknown, ConfigError::Override(_)), "{unknown:?}");
    assert!(unknown.to_string().contains("`modle`"), "{unknown}");

    let mistyped = ConfigFile::in_dir(dir.path())
        .with_overrides(vec![parse_override("model_context_window=lots").unwrap()])
        .load()
        .unwrap_err();
    assert!(matches!(mistyped, ConfigError::Override(_)), "{mistyped:?}");
}
