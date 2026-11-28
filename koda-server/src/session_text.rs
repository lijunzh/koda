//! A session's conversation as text: its turns ([`crate::history::turns`])
//! as rows in the order things were said, each labelled with what it is and
//! where it is. `session/search` reads these; anything else that needs a
//! session's words rather than its items can too.
//!
//! What a row holds:
//!
//! - [`RowKind::User`]: the text parts of a `userMessage`, a blank line
//!   between them: what the user typed, not the files, images or skills
//!   sent with it.
//! - [`RowKind::Assistant`]: an `agentMessage`'s text.
//! - [`RowKind::ToolCall`]: a tool call's name on the first line, then one
//!   line per argument, its name, a tab and its value. A value inside a
//!   list or an object is on a line of its own under its nearest name, so a
//!   command or a path reads as it was written, not as JSON holds it.
//! - [`RowKind::ToolOutput`]: the call's output, under the same item.
//!
//! Reasoning and `contextCompaction` items have no rows, and neither has an
//! empty text.
//!
//! Stored strings are joined with line breaks and tabs only. Both are
//! control characters, which the store never narrows a search by
//! ([`koda_state::session_store::SessionStore::search_candidates_page`]),
//! so a term it does narrow by can only occur inside one stored string.

use std::borrow::Cow;

use koda_protocol::item::Item;
use koda_protocol::turn::Turn;
use serde_json::Value;

/// What a [`Row`]'s text is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowKind {
    /// What the user typed.
    User,
    /// What the model answered.
    Assistant,
    /// A tool call's name and arguments.
    ToolCall,
    /// A tool call's output.
    ToolOutput,
}

/// One text of a session, and the item it is from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Row<'a> {
    pub kind: RowKind,
    /// The turn the item is in.
    pub turn_id: &'a str,
    /// The item: a tool call's two rows share it.
    pub item_id: &'a str,
    pub text: Cow<'a, str>,
}

/// The rows of `turns`, in the order they were said.
pub(crate) fn rows(turns: &[Turn]) -> Vec<Row<'_>> {
    let mut rows = Vec::new();
    for turn in turns {
        for item in &turn.items {
            let item = &item.item;
            match item {
                Item::UserMessage { content, .. } => {
                    let typed = koda_protocol::turn::text_of(content);
                    push(&mut rows, turn, item, RowKind::User, typed.into());
                }
                Item::AgentMessage { text, .. } => {
                    push(&mut rows, turn, item, RowKind::Assistant, text.into());
                }
                Item::ToolCall {
                    tool,
                    arguments,
                    output,
                    ..
                } => {
                    let call = call_text(tool, arguments);
                    push(&mut rows, turn, item, RowKind::ToolCall, call.into());
                    let output = output.as_deref().unwrap_or_default();
                    push(&mut rows, turn, item, RowKind::ToolOutput, output.into());
                }
                Item::Reasoning { .. } | Item::ContextCompaction { .. } => {}
            }
        }
    }
    rows
}

/// Add `item`'s `text` to `rows`, unless it is empty.
fn push<'a>(
    rows: &mut Vec<Row<'a>>,
    turn: &'a Turn,
    item: &'a Item,
    kind: RowKind,
    text: Cow<'a, str>,
) {
    if !text.is_empty() {
        rows.push(Row {
            kind,
            turn_id: &turn.id,
            item_id: item.id(),
            text,
        });
    }
}

/// A tool call as text: its name, then its arguments, a line each.
fn call_text(tool: &str, arguments: &Value) -> String {
    let mut text = tool.to_string();
    argument_lines(None, arguments, &mut text);
    text
}

/// `value`'s lines: a string as it is and any other value as JSON writes
/// it, after the name it was given under; the values of a list or an
/// object each on their own.
fn argument_lines(name: Option<&str>, value: &Value, text: &mut String) {
    let mut line = |written: &str| {
        text.push('\n');
        if let Some(name) = name {
            text.push_str(name);
            text.push('\t');
        }
        text.push_str(written);
    };
    match value {
        Value::Null => {}
        Value::String(string) => line(string),
        Value::Bool(_) | Value::Number(_) => line(&value.to_string()),
        Value::Array(values) => {
            for value in values {
                argument_lines(name, value, text);
            }
        }
        Value::Object(fields) => {
            for (name, value) in fields {
                argument_lines(Some(name), value, text);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use koda_protocol::item::{CompactionStatus, CompactionTrigger, ToolCallStatus, TurnItem};
    use koda_protocol::turn::{TurnStatus, UserInput};
    use serde_json::json;

    fn turn(id: &str, items: Vec<Item>) -> Turn {
        Turn {
            id: id.into(),
            status: TurnStatus::Completed,
            error: None,
            usage: None,
            duration_ms: None,
            started_at: None,
            completed_at: None,
            items: items
                .into_iter()
                .map(|item| TurnItem {
                    parent_item_id: None,
                    item,
                    started_at_ms: None,
                    completed_at_ms: None,
                })
                .collect(),
        }
    }

    fn call(id: &str, tool: &str, arguments: Value, output: Option<&str>) -> Item {
        Item::ToolCall {
            id: id.into(),
            tool: tool.into(),
            arguments,
            effect: None,
            kind: None,
            status: ToolCallStatus::Completed,
            output: output.map(str::to_string),
            exit_code: None,
            duration_ms: None,
            detail: Some("a detail is not searched".into()),
            changes: Vec::new(),
            command_actions: Vec::new(),
            progress: Vec::new(),
        }
    }

    /// Each kind of item gives the rows the module says, in order, under
    /// its turn and item; what the user attached, the model's reasoning, a
    /// compaction and empty texts give none.
    #[test]
    fn a_sessions_turns_read_as_rows_in_order() {
        let turns = [
            turn(
                "t1",
                vec![
                    Item::UserMessage {
                        id: "u1".into(),
                        content: vec![
                            UserInput::text("look at"),
                            UserInput::mention("src/secret.rs"),
                            UserInput::image("image/png", "aGk="),
                            UserInput::text("and fix it"),
                        ],
                        client_user_message_ids: Vec::new(),
                    },
                    Item::Reasoning {
                        id: "r1".into(),
                        text: "thinking is not said".into(),
                    },
                    Item::AgentMessage {
                        id: "m1".into(),
                        text: "Reading it.".into(),
                        phase: None,
                    },
                    call(
                        "c1",
                        "Bash",
                        json!({"command": "grep \"fn main\" src/a.rs", "timeout": 120}),
                        Some("src/a.rs:1:fn main() {\n"),
                    ),
                ],
            ),
            turn(
                "t2",
                vec![
                    Item::UserMessage {
                        id: "u2".into(),
                        content: vec![UserInput::image("image/png", "aGk=")],
                        client_user_message_ids: Vec::new(),
                    },
                    Item::ContextCompaction {
                        id: "x1".into(),
                        trigger: CompactionTrigger::Manual,
                        status: CompactionStatus::Completed,
                        context_percent: None,
                        messages_removed: None,
                        summary_tokens: None,
                        reason: Some("a reason is not said".into()),
                    },
                    call("c2", "Glob", json!({}), None),
                    Item::AgentMessage {
                        id: "m2".into(),
                        text: String::new(),
                        phase: None,
                    },
                ],
            ),
        ];
        let rows = rows(&turns);
        let read: Vec<(RowKind, &str, &str, &str)> = rows
            .iter()
            .map(|row| (row.kind, row.turn_id, row.item_id, row.text.as_ref()))
            .collect();
        assert_eq!(
            read,
            [
                (RowKind::User, "t1", "u1", "look at\n\nand fix it"),
                (RowKind::Assistant, "t1", "m1", "Reading it."),
                (
                    RowKind::ToolCall,
                    "t1",
                    "c1",
                    "Bash\ncommand\tgrep \"fn main\" src/a.rs\ntimeout\t120"
                ),
                (RowKind::ToolOutput, "t1", "c1", "src/a.rs:1:fn main() {\n"),
                (RowKind::ToolCall, "t2", "c2", "Glob"),
            ]
        );
    }

    /// Arguments read a line per value, whatever they are nested in: a
    /// string as written, anything else as JSON writes it, nothing for a
    /// null.
    #[test]
    fn a_calls_arguments_read_a_line_per_value() {
        for (arguments, expected) in [
            (json!({"path": "a b.rs"}), "Edit\npath\ta b.rs"),
            (
                json!({"edits": [
                    {"new": "z", "old": "x\ny"},
                    {"all": true, "old": "q"},
                ]}),
                "Edit\nnew\tz\nold\tx\ny\nall\ttrue\nold\tq",
            ),
            (
                json!({"after": null, "depth": 2, "paths": ["a.rs", "b.rs"]}),
                "Edit\ndepth\t2\npaths\ta.rs\npaths\tb.rs",
            ),
            (json!("not an object"), "Edit\nnot an object"),
            (json!(null), "Edit"),
        ] {
            assert_eq!(call_text("Edit", &arguments), expected, "{arguments}");
        }
    }
}
