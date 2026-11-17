//! Shrinking an MCP tool's input schema that is unusually large, as Codex
//! does (`codex-rs/tools/src/json_schema/compaction.rs`): every tool's
//! schema reaches the model on every turn, and one server's sprawling
//! schemas can take a large share of a small window.
//!
//! Best-effort, not a hard cap. While the schema is over the limit, Codex's
//! passes run in order, each losing more:
//!
//! 1. drop every `description` inside the schema (the tool's own
//!    description is kept);
//! 2. drop the `$defs` / `definitions` tables, turning local `$ref`s into
//!    `{}`;
//! 3. collapse objects nested three levels or deeper into `{}`;
//! 4. collapse `anyOf` / `oneOf` / `allOf` into `{}`.
//!
//! The top-level arguments always stay. Codex measures the schema after its
//! own normalization; Koda measures the compact JSON as sent.

use serde_json::{Value, json};

/// The size past which a schema is shrunk, in bytes of compact JSON:
/// Codex's `DEFAULT_COMPACT_TOOL_SCHEMA_BYTES`.
pub const DEFAULT_TOOL_SCHEMA_BYTES: usize = 5_000;

/// How deep an object may sit before the third pass collapses it.
const MAX_DEPTH: usize = 3;

const DEFINITION_TABLE_KEYS: [&str; 2] = ["$defs", "definitions"];
const SCHEMA_CHILD_KEYS: [&str; 4] = ["items", "anyOf", "oneOf", "allOf"];
const COMPOSITION_KEYS: [&str; 3] = ["anyOf", "oneOf", "allOf"];

/// Shrink `schema` in place until it fits `max_bytes`, or every pass has run.
pub fn compact(schema: &mut Value, max_bytes: usize) {
    let passes: [fn(&mut Value); 4] = [
        strip_descriptions,
        drop_definitions,
        |value| collapse_deep(value, 0),
        prune_compositions,
    ];
    for pass in passes {
        if size(schema) <= max_bytes {
            break;
        }
        pass(schema);
    }
}

fn size(value: &Value) -> usize {
    serde_json::to_vec(value)
        .map(|json| json.len())
        .unwrap_or(0)
}

/// Call `visit` on each schema-valued child of `map`: properties, items,
/// compositions, a non-boolean `additionalProperties`, and, with
/// `definitions`, the definition tables. Enum values and other instance data
/// aren't schemas and are left alone.
fn for_each_child(
    map: &mut serde_json::Map<String, Value>,
    definitions: bool,
    visit: &mut impl FnMut(&mut Value),
) {
    if let Some(Value::Object(properties)) = map.get_mut("properties") {
        properties.values_mut().for_each(&mut *visit);
    }
    for key in SCHEMA_CHILD_KEYS {
        if let Some(value) = map.get_mut(key) {
            visit(value);
        }
    }
    if let Some(additional) = map.get_mut("additionalProperties")
        && !additional.is_boolean()
    {
        visit(additional);
    }
    if definitions {
        for key in DEFINITION_TABLE_KEYS {
            if let Some(Value::Object(table)) = map.get_mut(key) {
                table.values_mut().for_each(&mut *visit);
            }
        }
    }
}

fn strip_descriptions(value: &mut Value) {
    match value {
        Value::Array(values) => values.iter_mut().for_each(strip_descriptions),
        Value::Object(map) => {
            map.remove("description");
            for_each_child(map, true, &mut strip_descriptions);
        }
        _ => {}
    }
}

fn is_local_ref(map: &serde_json::Map<String, Value>) -> bool {
    map.get("$ref").and_then(Value::as_str).is_some_and(|r| {
        DEFINITION_TABLE_KEYS
            .iter()
            .any(|table| r.starts_with(&format!("#/{table}/")))
    })
}

fn drop_definitions(value: &mut Value) {
    fn rewrite_refs(value: &mut Value) {
        match value {
            Value::Array(values) => values.iter_mut().for_each(rewrite_refs),
            Value::Object(map) if is_local_ref(map) => *value = json!({}),
            Value::Object(map) => for_each_child(map, false, &mut rewrite_refs),
            _ => {}
        }
    }
    rewrite_refs(value);
    if let Value::Object(map) = value {
        for key in DEFINITION_TABLE_KEYS {
            map.remove(key);
        }
    }
}

fn is_complex(map: &serde_json::Map<String, Value>) -> bool {
    SCHEMA_CHILD_KEYS.iter().any(|key| map.contains_key(*key))
        || map.contains_key("properties")
        || map.contains_key("additionalProperties")
        || map.contains_key("$ref")
}

fn collapse_deep(value: &mut Value, depth: usize) {
    match value {
        Value::Array(values) => values.iter_mut().for_each(|v| collapse_deep(v, depth)),
        Value::Object(map) if depth >= MAX_DEPTH && is_complex(map) => *value = json!({}),
        Value::Object(map) => for_each_child(map, false, &mut |v| collapse_deep(v, depth + 1)),
        _ => {}
    }
}

fn prune_compositions(value: &mut Value) {
    match value {
        Value::Array(values) => values.iter_mut().for_each(prune_compositions),
        Value::Object(map) if COMPOSITION_KEYS.iter().any(|k| map.contains_key(*k)) => {
            *value = json!({})
        }
        Value::Object(map) => for_each_child(map, false, &mut prune_compositions),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_small_schema_is_left_alone() {
        let mut schema = json!({"type": "object", "properties": {
            "q": {"type": "string", "description": "What to find"}}});
        let before = schema.clone();
        compact(&mut schema, DEFAULT_TOOL_SCHEMA_BYTES);
        assert_eq!(schema, before);
    }

    #[test]
    fn descriptions_go_first_and_the_arguments_stay() {
        let long = "x".repeat(300);
        let mut schema = json!({"type": "object", "properties": {
            "a": {"type": "string", "description": long},
            "b": {"type": "string", "enum": ["description"], "description": long}}});
        compact(&mut schema, 200);
        assert_eq!(
            schema,
            json!({"type": "object", "properties": {
                "a": {"type": "string"},
                "b": {"type": "string", "enum": ["description"]}}})
        );
    }

    #[test]
    fn definitions_then_depth_then_compositions_go_while_too_big() {
        let deep = json!({"type": "object", "properties": {"l2": {"type": "object",
            "properties": {"l3": {"type": "object", "properties": {"x": {"type": "string"}}}}}}});
        let mut schema = json!({
            "type": "object",
            "properties": {
                "user": {"$ref": "#/$defs/User"},
                "deep": deep,
                "either": {"anyOf": [{"type": "string"}, {"type": "number"}]},
            },
            "$defs": {"User": {"type": "object", "properties": {"name": {"type": "string"}}}},
        });
        compact(&mut schema, 10);
        assert_eq!(schema["properties"]["user"], json!({}));
        assert!(schema.get("$defs").is_none());
        assert_eq!(
            schema["properties"]["deep"]["properties"]["l2"]["properties"]["l3"],
            json!({})
        );
        assert_eq!(schema["properties"]["either"], json!({}));
        // The top-level arguments are all still there.
        let names: Vec<_> = schema["properties"].as_object().unwrap().keys().collect();
        assert_eq!(names, ["deep", "either", "user"]);
    }
}
