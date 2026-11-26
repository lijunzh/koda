//! Koda's protocol against Codex's app-server protocol, field by field.
//!
//! Koda follows Codex's protocol, saying "session" where Codex says
//! "thread". This walks every method, notification and server request Koda
//! has, pairs it with Codex's of the same name, and compares their params
//! and results down through the types they reach: each field Koda has must
//! be one Codex has under the same name, each value a string enum of Koda's
//! can take must be one Codex's takes, spelled the same, or a deviation
//! recorded in `codex-deviations.toml` with its reason. A recorded deviation that no
//! longer differs is reported too, so the file stays true. What Codex has
//! and Koda lacks isn't checked: Koda doesn't aim to have all of it.
//!
//! It reads the schema Codex generates with its experimental methods, which
//! Codex's checked-in schema leaves out, so it runs on demand:
//!
//! ```text
//! scripts/check_codex_parity.sh
//! ```
//!
//! which runs `codex app-server generate-json-schema --experimental` and
//! then this test with `CODEX_SCHEMA_DIR` set. Without it the test passes
//! without checking, so `cargo test` never needs Codex.
//!
//! The approach is the oracle check of Anthropic's "Migrating our Messages
//! API to Rust" (Nov 2026): the reference system decides what is correct,
//! each difference is reported by where it is, and every accepted difference
//! is a written rule.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::{Map, Value};

/// The recorded deviations: a reason for each.
#[derive(Debug, Default, serde::Deserialize)]
struct Deviations {
    #[serde(default)]
    methods: BTreeMap<String, String>,
    #[serde(default)]
    fields: BTreeMap<String, String>,
    #[serde(default)]
    variants: BTreeMap<String, String>,
    #[serde(default)]
    values: BTreeMap<String, String>,
}

/// One schema's definitions, by name.
struct Defs<'a> {
    defs: Map<String, Value>,
    /// The `$ref` prefix this schema uses, for reading names out of refs.
    _schema: &'a str,
}

impl Defs<'_> {
    fn get(&self, name: &str) -> Option<&Value> {
        self.defs.get(name)
    }

    /// `schema` with every `$ref` followed, and every `allOf` of one part,
    /// which Codex wraps a referenced type in to describe it.
    fn resolve<'s>(&'s self, mut schema: &'s Value) -> &'s Value {
        for _ in 0..10 {
            if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
                match self.get(ref_name(reference)) {
                    Some(found) => {
                        schema = found;
                        continue;
                    }
                    None => return schema,
                }
            }
            match schema
                .get("allOf")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
            {
                Some([only]) if schema.get("properties").is_none() => schema = only,
                _ => return schema,
            }
        }
        schema
    }

    /// The properties `schema` has, through `allOf`, and through every
    /// alternative of a `oneOf` or `anyOf` that isn't tagged (a flattened
    /// union, such as Codex's image reference).
    fn properties(&self, schema: &Value) -> Map<String, Value> {
        let mut out = Map::new();
        self.collect_properties(schema, &mut out, 0);
        out
    }

    fn collect_properties(&self, schema: &Value, out: &mut Map<String, Value>, depth: usize) {
        if depth > 6 {
            return;
        }
        let schema = self.resolve(schema);
        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            for (name, value) in properties {
                out.entry(name.clone()).or_insert_with(|| value.clone());
            }
        }
        for key in ["allOf"] {
            for part in schema
                .get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                self.collect_properties(part, out, depth + 1);
            }
        }
        if self.variants(schema).is_empty() {
            for key in ["oneOf", "anyOf"] {
                for part in schema
                    .get(key)
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    self.collect_properties(part, out, depth + 1);
                }
            }
        }
    }

    /// A tagged union's alternatives, by the value of their `type`, which
    /// Koda writes as `const` and Codex as a one-value `enum`.
    fn variants<'s>(&'s self, schema: &'s Value) -> BTreeMap<String, &'s Value> {
        let schema = self.resolve(schema);
        let mut out = BTreeMap::new();
        for key in ["oneOf", "anyOf"] {
            for part in schema
                .get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let part = self.resolve(part);
                let tag = &part["properties"]["type"];
                let value = tag
                    .get("const")
                    .or_else(|| tag.get("enum").and_then(|e| e.get(0)))
                    .and_then(Value::as_str);
                if let Some(value) = value {
                    out.insert(value.to_string(), part);
                }
            }
        }
        out
    }

    /// The values a string enum takes: a plain `enum`, a `const`, or the
    /// `const`s and `enum`s of a `oneOf` or `anyOf` (where schemars puts an
    /// enum whose values have doc comments). `None` when `schema` isn't one.
    /// A union of strings and objects (Codex's `AskForApproval`) gives its
    /// strings.
    fn string_values(&self, schema: &Value) -> Option<BTreeSet<String>> {
        let schema = self.resolve(schema);
        let strings = |values: &Value| -> Option<BTreeSet<String>> {
            let values = values.as_array()?;
            values
                .iter()
                .map(|v| v.as_str().map(str::to_string))
                .collect()
        };
        if let Some(values) = schema.get("enum") {
            return strings(values);
        }
        if let Some(value) = schema.get("const").and_then(Value::as_str) {
            return Some(BTreeSet::from([value.to_string()]));
        }
        let mut out = BTreeSet::new();
        for key in ["oneOf", "anyOf"] {
            for part in schema
                .get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let part = self.resolve(part);
                if part.get("properties").is_some() {
                    continue;
                }
                if let Some(values) = part.get("enum").and_then(strings) {
                    out.extend(values);
                } else if let Some(value) = part.get("const").and_then(Value::as_str) {
                    out.insert(value.to_string());
                }
            }
        }
        (!out.is_empty()).then_some(out)
    }
}

/// The type name a `$ref` names: its last segment.
fn ref_name(reference: &str) -> &str {
    reference.rsplit('/').next().unwrap_or(reference)
}

/// The type a schema names, if it names one.
fn type_name(schema: &Value) -> Option<String> {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        return Some(ref_name(reference).to_string());
    }
    for key in ["allOf", "anyOf"] {
        for part in schema
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(name) = type_name(part) {
                return Some(name);
            }
        }
    }
    None
}

/// The schema a value holds: an array's items, or the one non-null
/// alternative of an optional value.
fn inner(schema: &Value) -> &Value {
    if let Some(items) = schema.get("items") {
        return inner(items);
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(parts) = schema.get(key).and_then(Value::as_array) {
            let not_null: Vec<&Value> = parts
                .iter()
                .filter(|part| part.get("type").and_then(Value::as_str) != Some("null"))
                .collect();
            if let [only] = not_null.as_slice() {
                return inner(only);
            }
        }
    }
    schema
}

/// Codex's name for a Koda name: "session" is Codex's "thread".
fn codex_name(name: &str) -> String {
    name.replace("session", "thread")
        .replace("Session", "Thread")
}

/// What differs, each as `kind`, `where`, with what each side has where
/// that helps (an enum's values).
#[derive(Default)]
struct Walk {
    differences: BTreeMap<(String, String), String>,
    seen: BTreeSet<(String, String)>,
}

impl Walk {
    fn compare(
        &mut self,
        koda: &Defs,
        codex: &Defs,
        k: &Value,
        c: &Value,
        here: &str,
        depth: usize,
    ) {
        if depth > 5 {
            return;
        }
        let name = type_name(k).unwrap_or_else(|| here.to_string());
        let pair = (name.clone(), type_name(c).unwrap_or_default());
        if !self.seen.insert(pair) {
            return;
        }
        // A string enum: each value Koda's takes must be one Codex's takes.
        // Values only Codex's takes aren't checked, as fields.
        if let (Some(ours), Some(theirs)) = (koda.string_values(k), codex.string_values(c)) {
            for value in &ours {
                if !theirs.contains(value) && !theirs.contains(&codex_name(value)) {
                    self.differences.insert(
                        ("values".into(), format!("{name}.{value}")),
                        format!(
                            "Koda: {}; Codex: {}",
                            ours.iter().cloned().collect::<Vec<_>>().join(", "),
                            theirs.iter().cloned().collect::<Vec<_>>().join(", "),
                        ),
                    );
                }
            }
            return;
        }
        let koda_variants = koda.variants(k);
        if !koda_variants.is_empty() {
            // The fields a union has beside its variants (a stored item's
            // times), then each variant by its tag.
            let ours = koda.resolve(k).get("properties").and_then(Value::as_object);
            let theirs = codex
                .resolve(c)
                .get("properties")
                .and_then(Value::as_object);
            for field in ours.into_iter().flatten().map(|(field, _)| field) {
                let matched = theirs
                    .is_some_and(|t| t.contains_key(&codex_name(field)) || t.contains_key(field));
                if !matched {
                    self.differences
                        .insert(("fields".into(), format!("{name}.{field}")), String::new());
                }
            }
            let codex_variants = codex.variants(c);
            for (tag, variant) in koda_variants {
                match codex_variants.get(&tag) {
                    Some(theirs) => self.compare(
                        koda,
                        codex,
                        variant,
                        theirs,
                        &format!("{name}.{tag}"),
                        depth + 1,
                    ),
                    None => {
                        self.differences
                            .insert(("variants".into(), format!("{name}.{tag}")), String::new());
                    }
                }
            }
            return;
        }
        let ours = koda.properties(k);
        if ours.is_empty() {
            return;
        }
        let theirs = codex.properties(c);
        for (field, schema) in &ours {
            let matched = theirs.get(&codex_name(field)).or_else(|| theirs.get(field));
            match matched {
                Some(their_schema) => self.compare(
                    koda,
                    codex,
                    inner(schema),
                    inner(their_schema),
                    &format!("{name}.{field}"),
                    depth + 1,
                ),
                None => {
                    self.differences
                        .insert(("fields".into(), format!("{name}.{field}")), String::new());
                }
            }
        }
    }
}

/// Codex's methods of one kind (`ClientRequest`, `ServerNotification`,
/// `ServerRequest`), each with its params schema.
fn codex_methods(codex: &Defs, kind: &str) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    let Some(union) = codex.get(kind) else {
        return out;
    };
    for part in union["oneOf"].as_array().into_iter().flatten() {
        let method = part["properties"]["method"]["enum"][0].as_str();
        if let Some(method) = method {
            out.insert(method.to_string(), part["properties"]["params"].clone());
        }
    }
    out
}

/// Every difference between Koda's protocol and Codex's in `codex_dir`.
fn differences(codex_dir: &Path) -> BTreeMap<(String, String), String> {
    let bundle: Value = serde_json::from_str(
        &std::fs::read_to_string(codex_dir.join("codex_app_server_protocol.schemas.json"))
            .expect("Codex's schema bundle"),
    )
    .unwrap();
    differences_between(&koda_protocol::schema(), &bundle)
}

/// Every difference between `koda_schema`, as `koda_protocol::schema`
/// gives it, and Codex's schema `bundle`.
fn differences_between(koda_schema: &Value, bundle: &Value) -> BTreeMap<(String, String), String> {
    let koda = Defs {
        defs: koda_schema["$defs"]
            .as_object()
            .cloned()
            .unwrap_or_default(),
        _schema: "koda",
    };
    // Codex keeps its v2 types under `definitions.v2`.
    let mut defs = bundle["definitions"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    if let Some(v2) = defs.get("v2").and_then(Value::as_object).cloned() {
        defs.extend(v2);
    }
    let codex = Defs {
        defs,
        _schema: "codex",
    };
    let mut walk = Walk::default();
    for (group, kind) in [
        ("methods", "ClientRequest"),
        ("notifications", "ServerNotification"),
        ("serverRequests", "ServerRequest"),
    ] {
        let theirs = codex_methods(&codex, kind);
        for (method, spec) in koda_schema[group].as_object().into_iter().flatten() {
            let Some(params) = theirs.get(&codex_name(method)) else {
                walk.differences
                    .insert(("methods".into(), method.clone()), String::new());
                continue;
            };
            walk.compare(&koda, &codex, &spec["params"], params, method, 0);
            // A response is the params' type with `Response` for `Params`.
            if let (Some(result), Some(params_type)) = (spec.get("result"), type_name(params)) {
                let response = params_type.replace("Params", "Response");
                if codex.get(&response).is_some() {
                    let theirs = serde_json::json!({ "$ref": format!("#/definitions/{response}") });
                    walk.compare(&koda, &codex, result, &theirs, method, 0);
                }
            }
        }
    }
    walk.differences
}

#[test]
fn koda_s_protocol_is_codex_s_or_a_recorded_deviation() {
    let Some(codex_dir) = std::env::var_os("CODEX_SCHEMA_DIR") else {
        eprintln!("CODEX_SCHEMA_DIR isn't set: run scripts/check_codex_parity.sh");
        return;
    };
    let deviations: Deviations = toml::from_str(
        &std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/codex-deviations.toml"
        ))
        .unwrap(),
    )
    .unwrap();
    let found = differences(Path::new(&codex_dir));
    let recorded = |kind: &str| match kind {
        "methods" => &deviations.methods,
        "fields" => &deviations.fields,
        "values" => &deviations.values,
        _ => &deviations.variants,
    };
    let unexplained: Vec<String> = found
        .iter()
        .filter(|((kind, at), _)| !recorded(kind).contains_key(at))
        .map(|((kind, at), detail)| match detail.as_str() {
            "" => format!("{kind}: {at}"),
            detail => format!("{kind}: {at} ({detail})"),
        })
        .collect();
    let mut stale = Vec::new();
    for (kind, table) in [
        ("methods", &deviations.methods),
        ("fields", &deviations.fields),
        ("variants", &deviations.variants),
        ("values", &deviations.values),
    ] {
        for at in table.keys() {
            if !found.contains_key(&(kind.to_string(), at.clone())) {
                stale.push(format!("{kind}: {at}"));
            }
        }
    }
    assert!(
        unexplained.is_empty() && stale.is_empty(),
        "Koda's protocol differs from Codex's where no deviation is recorded:\n  {}\n\
         Recorded deviations that no longer differ:\n  {}\n\
         Rename to Codex's name or spelling, or record the deviation and its reason in \
         koda-protocol/codex-deviations.toml.",
        unexplained.join("\n  "),
        stale.join("\n  "),
    );
}

/// The walk itself, on two small schemas: a field under Codex's name
/// (`sessionId` for `threadId`) matches, one under another name doesn't,
/// a tagged union is compared by its tags, and a method Codex lacks is
/// reported.
#[test]
fn a_renamed_field_or_a_missing_variant_is_a_difference() {
    let koda = serde_json::json!({
        "methods": {
            "session/read": {
                "params": {"$ref": "#/$defs/SessionReadParams"},
                "result": {"$ref": "#/$defs/SessionReadResponse"}
            },
            "koda/only": {"params": {"$ref": "#/$defs/SessionReadParams"}}
        },
        "notifications": {},
        "serverRequests": {},
        "$defs": {
            "SessionReadParams": {"properties": {
                "sessionId": {"type": "string"},
                "withTurns": {"type": "boolean"}
            }},
            "SessionReadResponse": {"properties": {
                "status": {"$ref": "#/$defs/SessionStatus"}
            }},
            "SessionStatus": {"oneOf": [
                {"properties": {"type": {"const": "idle"}}},
                {"properties": {"type": {"const": "sleeping"}}}
            ]}
        }
    });
    let codex = serde_json::json!({"definitions": {
        "ClientRequest": {"oneOf": [{"properties": {
            "method": {"enum": ["thread/read"]},
            "params": {"$ref": "#/definitions/v2/ThreadReadParams"}
        }}]},
        "v2": {
            "ThreadReadParams": {"properties": {
                "threadId": {"type": "string"},
                "includeTurns": {"type": "boolean"}
            }},
            "ThreadReadResponse": {"properties": {
                "status": {"allOf": [{"$ref": "#/definitions/v2/ThreadStatus"}]}
            }},
            "ThreadStatus": {"oneOf": [
                {"properties": {"type": {"enum": ["idle"]}}},
                {"properties": {"type": {"enum": ["systemError"]}}}
            ]}
        }
    }});
    let found = differences_between(&koda, &codex);
    let found: Vec<String> = found.keys().map(|(k, at)| format!("{k}: {at}")).collect();
    assert_eq!(
        found,
        [
            "fields: SessionReadParams.withTurns",
            "methods: koda/only",
            "variants: SessionStatus.sleeping",
        ]
    );
}

/// A string enum is compared by its values: one Koda spells differently
/// (`finalAnswer` for Codex's `final_answer`, the 2026-11-26 `MessagePhase`
/// miss) is a difference, whether Koda writes the enum as `oneOf` of
/// `const`s and Codex as a plain `enum` or the other way round. A value only
/// Codex has isn't, and neither is a tag that matches.
#[test]
fn an_enum_value_spelled_differently_is_a_difference() {
    let koda = serde_json::json!({
        "methods": {},
        "notifications": {
            "item/completed": {"params": {"$ref": "#/$defs/ItemCompletedNotification"}}
        },
        "serverRequests": {},
        "$defs": {
            "ItemCompletedNotification": {"properties": {
                "phase": {"anyOf": [{"$ref": "#/$defs/MessagePhase"}, {"type": "null"}]},
                "status": {"$ref": "#/$defs/Status"}
            }},
            "MessagePhase": {"oneOf": [
                {"type": "string", "const": "commentary"},
                {"type": "string", "const": "finalAnswer"}
            ]},
            "Status": {"type": "string", "enum": ["done"]}
        }
    });
    let codex = serde_json::json!({"definitions": {
        "ServerNotification": {"oneOf": [{"properties": {
            "method": {"enum": ["item/completed"]},
            "params": {"$ref": "#/definitions/v2/ItemCompletedNotification"}
        }}]},
        "v2": {
            "ItemCompletedNotification": {"properties": {
                "phase": {"anyOf": [{"$ref": "#/definitions/v2/MessagePhase"}, {"type": "null"}]},
                "status": {"$ref": "#/definitions/v2/Status"}
            }},
            "MessagePhase": {"type": "string", "enum": ["commentary", "partial_answer", "final_answer"]},
            "Status": {"oneOf": [
                {"type": "string", "enum": ["done"], "description": "Finished."},
                {"type": "string", "enum": ["failed"]}
            ]}
        }
    }});
    let found = differences_between(&koda, &codex);
    let found: Vec<String> = found
        .iter()
        .map(|((k, at), detail)| format!("{k}: {at} ({detail})"))
        .collect();
    assert_eq!(
        found,
        ["values: MessagePhase.finalAnswer \
             (Koda: commentary, finalAnswer; Codex: commentary, final_answer, partial_answer)"]
    );
}
