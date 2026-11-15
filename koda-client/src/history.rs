//! A stored session's turns, rebuilt from its messages, for `session/start`
//! resuming it and for `session/read`, as Codex's `thread/resume` and
//! `thread/read` return a thread's turns.
//!
//! Koda stores messages, not turns or items, so this reads them back the way
//! [`crate::events::EventProjector`] sent them live: each user prompt opens a
//! turn; a reply's reasoning and text are items; each tool call is an item,
//! completed where its result was stored. A sub-agent's own session supplies
//! its tool calls, after its `InvokeAgent` call's other items and before the
//! call, with the call as their parent, as they completed live. Its text
//! isn't sent live (its answer is the call's output), so it isn't here. What isn't stored
//! is left out; [`koda_protocol::session::Session::turns`] lists it.

use std::collections::HashMap;

use koda_core::persistence::{Message, Persistence, Role, SubAgentSession, session_event_kind};
use koda_core::providers::ToolCall;
use koda_protocol::item::{Item, ToolCallStatus, TurnItem};
use koda_protocol::turn::{Turn, TurnStatus};
use serde_json::Value;

/// A failure, as the database described it.
type Result<T> = std::result::Result<T, String>;

fn checked<T, E: std::fmt::Display>(read: std::result::Result<T, E>) -> Result<T> {
    read.map_err(|e| format!("{e:#}"))
}

/// How deep sub-agents are followed. Koda limits how deep they nest when it
/// runs them; this only stops a bad link from looping.
const MAX_DEPTH: usize = 8;

/// The turns of session `session_id`, oldest first.
pub(crate) async fn turns(db: &dyn Persistence, session_id: &str) -> Result<Vec<Turn>> {
    let messages = checked(db.load_all_messages(session_id).await)?;
    // Notices are stored under the call they are about, in the session that
    // ran the turn, whichever sub-agent sent them.
    let progress = progress_by_call(db, session_id).await?;
    let mut reader = Reader {
        db,
        progress,
        turns: Vec::new(),
    };
    reader.session(session_id, &messages, None, 0).await?;
    Ok(reader.turns)
}

struct Reader<'a> {
    db: &'a dyn Persistence,
    progress: HashMap<String, Vec<String>>,
    turns: Vec<Turn>,
}

impl Reader<'_> {
    /// Read one session's messages. `parent` is set for a sub-agent's, whose
    /// items join the turn already open.
    async fn session(
        &mut self,
        session_id: &str,
        messages: &[Message],
        parent: Option<&str>,
        depth: usize,
    ) -> Result<()> {
        let children: HashMap<String, SubAgentSession> = if depth < MAX_DEPTH {
            checked(self.db.sub_agent_sessions(session_id).await)?
                .into_iter()
                .filter_map(|child| Some((child.tool_call_id.clone()?, child)))
                .collect()
        } else {
            HashMap::new()
        };
        // Calls started and not yet completed, by id: name and arguments.
        let mut open: HashMap<String, (String, Value)> = HashMap::new();
        // Whether the session's last stored row leaves a turn mid-way: a
        // user row then is steering text, not a new prompt.
        let mut mid_turn = false;

        for msg in messages {
            match msg.role {
                // The system prompt and compaction summaries.
                Role::System => {}
                // A sub-agent's prompt and reminders aren't sent as items.
                Role::User if parent.is_some() => {}
                Role::User => {
                    if msg.tool_call_id.is_some() {
                        continue;
                    }
                    let text = msg.content.clone().unwrap_or_default();
                    if !mid_turn || self.turns.is_empty() {
                        self.turns.push(Turn {
                            id: format!("turn-m{}", msg.id),
                            status: TurnStatus::Completed,
                            error: None,
                            usage: None,
                            duration_ms: None,
                            items: Vec::new(),
                        });
                    }
                    self.push(
                        None,
                        Item::UserMessage {
                            id: format!("user-m{}", msg.id),
                            text,
                            client_message_ids: Vec::new(),
                        },
                    );
                    mid_turn = false;
                }
                Role::Assistant => {
                    // A sub-agent's text isn't sent live: its answer is its
                    // call's output.
                    if parent.is_none()
                        && let Some(thinking) = msg.thinking_content.as_deref()
                        && !thinking.is_empty()
                    {
                        self.push(
                            parent,
                            Item::Reasoning {
                                id: format!("reasoning-m{}", msg.id),
                                text: thinking.to_string(),
                            },
                        );
                    }
                    if parent.is_none()
                        && let Some(text) = msg.content.as_deref()
                        && !text.is_empty()
                    {
                        self.push(
                            parent,
                            Item::AgentMessage {
                                id: format!("message-m{}", msg.id),
                                text: text.to_string(),
                            },
                        );
                    }
                    let calls: Vec<ToolCall> = msg
                        .tool_calls
                        .as_deref()
                        .and_then(|json| serde_json::from_str(json).ok())
                        .unwrap_or_default();
                    mid_turn = !calls.is_empty();
                    for call in calls {
                        let arguments = serde_json::from_str(&call.arguments).unwrap_or_default();
                        open.insert(call.id, (call.function_name, arguments));
                    }
                }
                Role::Tool => {
                    mid_turn = true;
                    let Some(id) = msg.tool_call_id.clone() else {
                        continue;
                    };
                    let (tool, arguments) = open.remove(&id).unwrap_or_default();
                    if let Some(child) = children.get(&id) {
                        Box::pin(self.sub_agent(child, &id, depth)).await?;
                    }
                    let output = msg.content.clone().unwrap_or_default();
                    let progress = self.progress.remove(&id).unwrap_or_default();
                    self.push(
                        parent,
                        Item::ToolCall {
                            status: status_of(&output),
                            progress,
                            id,
                            tool,
                            arguments,
                            output: Some(output),
                            detail: None,
                            changes: Vec::new(),
                        },
                    );
                }
            }
        }
        // The last turn of the session itself, left mid-way.
        if parent.is_none()
            && mid_turn
            && let Some(last) = self.turns.last_mut()
        {
            last.status = TurnStatus::Interrupted;
        }
        Ok(())
    }

    /// The items of the sub-agent that call `call_id` started.
    async fn sub_agent(
        &mut self,
        child: &SubAgentSession,
        call_id: &str,
        depth: usize,
    ) -> Result<()> {
        let messages = checked(self.db.load_all_messages(&child.session_id).await)?;
        let own = messages.get(child.forked_rows..).unwrap_or_default();
        self.session(&child.session_id, own, Some(call_id), depth + 1)
            .await
    }

    fn push(&mut self, parent: Option<&str>, item: Item) {
        if let Some(turn) = self.turns.last_mut() {
            turn.items.push(TurnItem {
                parent_item_id: parent.map(str::to_string),
                item,
            });
        }
    }
}

/// The notices stored for each call, as `item/toolCall/progress` sent them.
/// A sub-agent's stored trace also names each tool it started; those are
/// its items now, so they're left out.
async fn progress_by_call(
    db: &dyn Persistence,
    session_id: &str,
) -> Result<HashMap<String, Vec<String>>> {
    let mut by_call: HashMap<String, Vec<String>> = HashMap::new();
    for event in checked(db.load_session_events(session_id).await)? {
        if event.kind != session_event_kind::SUB_AGENT_EVENT {
            continue;
        }
        let Some(call) = event.parent_tool_call_id else {
            continue;
        };
        let line = event.payload.trim();
        if line.starts_with('\u{1f527}') {
            continue;
        }
        by_call.entry(call).or_default().push(line.to_string());
    }
    Ok(by_call)
}

/// A stored result's status. The outcome itself isn't stored, so a call
/// that was refused or stopped reads as completed.
fn status_of(output: &str) -> ToolCallStatus {
    let head = output.trim_start();
    if head.starts_with("Error:") || head.starts_with('\u{274c}') {
        ToolCallStatus::Failed
    } else {
        ToolCallStatus::Completed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use koda_core::db::Database;
    use koda_core::persistence::{FORKED_ROWS_KEY, PARENT_SESSION_KEY, PARENT_TOOL_CALL_KEY};
    use serde_json::json;

    fn calls(id: &str, name: &str, args: Value) -> String {
        json!([{"id": id, "function_name": name, "arguments": args.to_string()}]).to_string()
    }

    async fn insert(db: &Database, session: &str, role: Role, content: &str) {
        db.insert_message(session, &role, Some(content), None, None, None)
            .await
            .unwrap();
    }

    /// Prompts open turns and steering text joins one; a forked sub-agent's
    /// own tool calls follow its call, without the history it copied or its
    /// text, and its notices are the call's progress.
    #[tokio::test]
    async fn turns_are_rebuilt_with_a_forked_sub_agents_items() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::init(dir.path()).await.unwrap();
        let root = dir.path();
        let parent = db.create_session("default", root).await.unwrap();
        insert(&db, &parent, Role::System, "system prompt").await;
        insert(&db, &parent, Role::User, "Ask the fork").await;
        db.insert_message(
            &parent,
            &Role::Assistant,
            Some("Asking."),
            Some(&calls(
                "call-a",
                "InvokeAgent",
                json!({"agent_name": "fork"}),
            )),
            None,
            None,
        )
        .await
        .unwrap();
        // The fork copies the history so far, then works on its own.
        let child = db.create_session("fork", root).await.unwrap();
        db.set_metadata(&child, PARENT_SESSION_KEY, &parent)
            .await
            .unwrap();
        db.set_metadata(&child, PARENT_TOOL_CALL_KEY, "call-a")
            .await
            .unwrap();
        db.set_metadata(&child, FORKED_ROWS_KEY, "2").await.unwrap();
        insert(&db, &child, Role::System, "system prompt").await;
        insert(&db, &child, Role::User, "Ask the fork").await;
        insert(&db, &child, Role::User, "the fork's prompt").await;
        db.insert_message(
            &child,
            &Role::Assistant,
            None,
            Some(&calls("call-r", "Read", json!({"path": "a.txt"}))),
            None,
            None,
        )
        .await
        .unwrap();
        db.insert_message(
            &child,
            &Role::Tool,
            Some("Error: no such file"),
            None,
            Some("call-r"),
            None,
        )
        .await
        .unwrap();
        insert(&db, &child, Role::Assistant, "Nothing there.").await;
        for (line, call) in [
            ("  \u{1f527} Read", "call-a"),
            ("  \u{23f3} fork: reached max turns (1)", "call-a"),
        ] {
            db.insert_session_event(
                &parent,
                session_event_kind::SUB_AGENT_EVENT,
                line,
                Some(call),
            )
            .await
            .unwrap();
        }
        db.insert_message(
            &parent,
            &Role::Tool,
            Some("Nothing there."),
            None,
            Some("call-a"),
            None,
        )
        .await
        .unwrap();
        insert(&db, &parent, Role::User, "and steer").await;
        insert(&db, &parent, Role::Assistant, "Done.").await;
        insert(&db, &parent, Role::User, "Next").await;
        insert(&db, &parent, Role::Assistant, "OK.").await;

        let turns = turns(&db, &parent).await.unwrap();
        assert_eq!(turns.len(), 2, "{turns:#?}");
        let shape: Vec<(Option<&str>, String)> = turns[0]
            .items
            .iter()
            .map(|i| {
                let what = match &i.item {
                    Item::UserMessage { text, .. } => format!("user {text}"),
                    Item::AgentMessage { text, .. } => format!("message {text}"),
                    Item::Reasoning { text, .. } => format!("reasoning {text}"),
                    Item::ToolCall {
                        id, tool, status, ..
                    } => format!("{tool} {id} {status:?}"),
                    Item::ContextCompaction { .. } => "compaction".to_string(),
                };
                (i.parent_item_id.as_deref(), what)
            })
            .collect();
        assert_eq!(
            shape,
            [
                (None, "user Ask the fork".to_string()),
                (None, "message Asking.".to_string()),
                (Some("call-a"), "Read call-r Failed".to_string()),
                (None, "InvokeAgent call-a Completed".to_string()),
                (None, "user and steer".to_string()),
                (None, "message Done.".to_string()),
            ]
        );
        let Item::ToolCall {
            progress,
            arguments,
            ..
        } = &turns[0].items[3].item
        else {
            unreachable!()
        };
        assert_eq!(progress, &["\u{23f3} fork: reached max turns (1)"]);
        assert_eq!(arguments["agent_name"], "fork");
        assert_eq!(turns[0].status, TurnStatus::Completed);
        assert_eq!(turns[1].items.len(), 2);
    }

    /// A session that stopped mid-way ends on an interrupted turn.
    #[tokio::test]
    async fn a_turn_left_mid_way_is_interrupted() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::init(dir.path()).await.unwrap();
        let session = db.create_session("default", dir.path()).await.unwrap();
        insert(&db, &session, Role::User, "Read it").await;
        db.insert_message(
            &session,
            &Role::Assistant,
            None,
            Some(&calls("call-1", "Read", json!({"path": "a.txt"}))),
            None,
            None,
        )
        .await
        .unwrap();
        let turns = turns(&db, &session).await.unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].status, TurnStatus::Interrupted);
        // A call with no stored result never completed, so it isn't listed.
        assert_eq!(turns[0].items.len(), 1);
    }
}
