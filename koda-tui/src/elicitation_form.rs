//! An MCP server's request for input (`mcpServer/elicitation/request`),
//! asked one field at a time in the question dialog.
//!
//! Form mode's schema is flat: each property is a string, number, integer,
//! boolean or string enum, as MCP allows. The fields come in the order of
//! their names, as Codex's typed schema (a `BTreeMap`) gives them. Each
//! field is one question; a
//! choice or a yes/no shows numbered options. Enter on an empty answer takes
//! the field's default, or skips a field that isn't required. URL mode is one
//! step: visit the page, then Enter. Esc declines.

use koda_client::protocol::mcp::McpServerElicitationRequestParams;
use koda_client::protocol::rpc::RequestId;
use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq)]
enum Kind {
    Text,
    Number,
    Integer,
    Boolean,
    /// Each choice's value and label.
    Choice(Vec<(String, String)>),
}

#[derive(Debug, Clone, PartialEq)]
struct Field {
    name: String,
    title: String,
    description: Option<String>,
    kind: Kind,
    required: bool,
    default: Option<Value>,
}

/// The form under way.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ElicitationForm {
    /// The host's request this answers.
    pub id: RequestId,
    server: String,
    message: String,
    url: Option<String>,
    fields: Vec<Field>,
    index: usize,
    values: Map<String, Value>,
}

impl ElicitationForm {
    pub(crate) fn new(id: RequestId, params: &McpServerElicitationRequestParams) -> Self {
        let schema = params.requested_schema.clone().unwrap_or(Value::Null);
        let required: Vec<&str> = schema["required"]
            .as_array()
            .map(|names| names.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let fields = schema["properties"]
            .as_object()
            .map(|properties| {
                properties
                    .iter()
                    .map(|(name, property)| Field {
                        name: name.clone(),
                        title: property["title"].as_str().unwrap_or(name).to_string(),
                        description: property["description"].as_str().map(str::to_string),
                        kind: kind(property),
                        required: required.contains(&name.as_str()),
                        default: property.get("default").cloned(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            id,
            server: params.server_name.clone(),
            message: params.message.clone(),
            url: params.url.clone(),
            fields,
            index: 0,
            values: Map::new(),
        }
    }

    /// The question for the current step.
    pub(crate) fn question(&self) -> String {
        let head = format!("{} asks: {}", self.server, self.message);
        if let Some(url) = &self.url {
            return format!("{head}\nOpen {url}, then press Enter when done.");
        }
        let Some(field) = self.fields.get(self.index) else {
            return head;
        };
        let mut line = format!(
            "{head}\n({}/{}) {}",
            self.index + 1,
            self.fields.len(),
            field.title
        );
        if let Some(description) = &field.description {
            line.push_str(&format!(" — {description}"));
        }
        let kind = match field.kind {
            Kind::Number => Some("a number"),
            Kind::Integer => Some("a whole number"),
            _ => None,
        };
        if let Some(kind) = kind {
            line.push_str(&format!(" ({kind})"));
        }
        if let Some(default) = &field.default {
            line.push_str(&format!(" [default: {}]", display(default)));
        } else if !field.required {
            line.push_str(" [optional]");
        }
        line
    }

    /// The current step's numbered choices, if it has any.
    pub(crate) fn options(&self) -> Vec<String> {
        match self.fields.get(self.index).map(|f| &f.kind) {
            Some(Kind::Boolean) => vec!["yes".into(), "no".into()],
            Some(Kind::Choice(choices)) => choices.iter().map(|(_, l)| l.clone()).collect(),
            _ => Vec::new(),
        }
    }

    /// Take `answer` for the current step. `Ok(Some(content))` once every
    /// field has its value, `Ok(None)` for the next question, `Err` with
    /// why the answer can't be taken.
    pub(crate) fn submit(&mut self, answer: &str) -> Result<Option<Value>, String> {
        if self.url.is_some() || self.fields.is_empty() {
            return Ok(Some(Value::Object(self.values.clone())));
        }
        let field = &self.fields[self.index];
        let answer = answer.trim();
        let value = if answer.is_empty() {
            match (&field.default, field.required) {
                (Some(default), _) => Some(default.clone()),
                (None, false) => None,
                (None, true) => return Err(format!("{} is required.", field.title)),
            }
        } else {
            Some(parse(&field.kind, answer)?)
        };
        if let Some(value) = value {
            self.values.insert(field.name.clone(), value);
        }
        self.index += 1;
        Ok((self.index == self.fields.len()).then(|| Value::Object(self.values.clone())))
    }
}

fn kind(property: &Value) -> Kind {
    let labels = property["enumNames"].as_array();
    if let Some(values) = property["enum"].as_array() {
        return Kind::Choice(
            values
                .iter()
                .enumerate()
                .filter_map(|(i, v)| {
                    let value = v.as_str()?.to_string();
                    let label = labels
                        .and_then(|l| l.get(i))
                        .and_then(Value::as_str)
                        .unwrap_or(&value)
                        .to_string();
                    Some((value, label))
                })
                .collect(),
        );
    }
    if let Some(one_of) = property["oneOf"].as_array() {
        return Kind::Choice(
            one_of
                .iter()
                .filter_map(|o| {
                    let value = o["const"].as_str()?.to_string();
                    let label = o["title"].as_str().unwrap_or(&value).to_string();
                    Some((value, label))
                })
                .collect(),
        );
    }
    match property["type"].as_str() {
        Some("number") => Kind::Number,
        Some("integer") => Kind::Integer,
        Some("boolean") => Kind::Boolean,
        _ => Kind::Text,
    }
}

fn parse(kind: &Kind, answer: &str) -> Result<Value, String> {
    match kind {
        Kind::Text => Ok(Value::String(answer.to_string())),
        Kind::Number => answer
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .ok_or_else(|| format!("{answer:?} isn't a number.")),
        Kind::Integer => answer
            .parse::<i64>()
            .map(Value::from)
            .map_err(|_| format!("{answer:?} isn't a whole number.")),
        Kind::Boolean => match answer.to_ascii_lowercase().as_str() {
            "1" | "y" | "yes" | "true" => Ok(Value::Bool(true)),
            "2" | "n" | "no" | "false" => Ok(Value::Bool(false)),
            _ => Err("Answer yes or no (1 or 2).".into()),
        },
        Kind::Choice(choices) => {
            let by_number = answer
                .parse::<usize>()
                .ok()
                .and_then(|n| n.checked_sub(1))
                .and_then(|i| choices.get(i));
            let by_name = choices.iter().find(|(value, label)| {
                value.eq_ignore_ascii_case(answer) || label.eq_ignore_ascii_case(answer)
            });
            by_number
                .or(by_name)
                .map(|(value, _)| Value::String(value.clone()))
                .ok_or_else(|| format!("Pick one of the {} choices.", choices.len()))
        }
    }
}

fn display(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn form(schema: Value) -> ElicitationForm {
        ElicitationForm::new(
            RequestId::Integer(1),
            &McpServerElicitationRequestParams {
                session_id: "s".into(),
                server_name: "github".into(),
                message: "Which repository?".into(),
                requested_schema: Some(schema),
                url: None,
            },
        )
    }

    #[test]
    fn each_field_is_asked_in_turn_and_typed() {
        let mut form = form(json!({
            "type": "object",
            "properties": {
                "repo": {"type": "string", "title": "Repository"},
                "count": {"type": "integer"},
                "private": {"type": "boolean"},
                "kind": {"type": "string", "enum": ["bug", "feature"], "enumNames": ["Bug", "Feature"]}
            },
            "required": ["repo", "count"]
        }));
        // In name order: count, kind, private, repo.
        assert!(form.question().contains("github asks: Which repository?"));
        assert!(form.question().contains("(1/4) count (a whole number)"));
        assert!(form.submit("two").unwrap_err().contains("whole number"));
        assert_eq!(form.submit("2"), Ok(None));
        assert_eq!(form.options(), ["Bug", "Feature"]);
        assert!(form.submit("3").unwrap_err().contains("choices"));
        assert_eq!(form.submit("2"), Ok(None));
        assert_eq!(form.options(), ["yes", "no"]);
        assert_eq!(form.submit("1"), Ok(None));
        assert!(form.question().contains("Repository"));
        let content = form.submit("koda").unwrap().unwrap();
        assert_eq!(
            content,
            json!({"repo": "koda", "count": 2, "private": true, "kind": "feature"})
        );
    }

    #[test]
    fn an_empty_answer_takes_the_default_skips_an_optional_field_or_is_refused() {
        let mut form = form(json!({
            "type": "object",
            "properties": {
                "a": {"type": "string"},
                "b": {"type": "string", "default": "x"},
                "c": {"type": "string"}
            },
            "required": ["a"]
        }));
        assert!(form.submit("").unwrap_err().contains("required"));
        assert_eq!(form.submit("one"), Ok(None));
        assert!(form.question().contains("[default: x]"));
        assert_eq!(form.submit(""), Ok(None));
        assert!(form.question().contains("[optional]"));
        assert_eq!(form.submit(""), Ok(Some(json!({"a": "one", "b": "x"}))));
    }

    #[test]
    fn a_url_request_is_one_step() {
        let mut form = ElicitationForm::new(
            RequestId::Integer(2),
            &McpServerElicitationRequestParams {
                session_id: "s".into(),
                server_name: "pay".into(),
                message: "Approve the charge".into(),
                requested_schema: None,
                url: Some("https://pay.example.com/approve".into()),
            },
        );
        assert!(form.question().contains("https://pay.example.com/approve"));
        assert_eq!(form.submit(""), Ok(Some(json!({}))));
    }
}
