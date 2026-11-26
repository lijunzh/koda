//! A session's turns as a client saw them, for `session/start` resuming it,
//! `session/read` and `session/turns/list` ([`page`]), as Codex's
//! `thread/resume`, `thread/read` and `thread/turns/list` return a
//! thread's turns.
//!
//! The host stores each turn as it sends it ([`HistoryWriter`]): the turn as
//! it starts and ends, and each item as it completes, with its parent. A
//! read replays those records, so a resumed turn is the one the client saw,
//! as Codex replays its rollout file and Claude Code its transcript.
//!
//! Sessions from before 6h have no records. Their turns are rebuilt from the
//! messages the model read ([`rebuild`]), which keep less; the first time
//! the host opens one, the rebuilt turns are stored as its records
//! ([`turns_for_resume`]), and its new turns follow them.

use std::collections::HashMap;

use koda_core::providers::ToolCall;
use koda_protocol::ServerNotification;
use koda_protocol::item::{Item, ToolCallStatus, TurnItem};
use koda_protocol::turn::{Turn, TurnStatus};
use koda_state::db::Database;
use koda_state::session_store::{Message, Role, SessionStore, TurnRecord, session_event_kind};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::events::Outgoing;

/// A failure, as the database described it.
type Result<T> = std::result::Result<T, String>;

fn checked<T, E: std::fmt::Display>(read: std::result::Result<T, E>) -> Result<T> {
    read.map_err(|e| format!("{e:#}"))
}

/// A record of the turn itself, as it started or ended: a [`Turn`].
const TURN: &str = "turn";
/// A record of a completed item: a [`TurnItem`].
const ITEM: &str = "item";

// ── Writing ─────────────────────────────────────────────────────────────

/// Stores one session's turns as the host sends them, in order, on a task
/// of its own so the engine never waits on the database.
pub(crate) struct HistoryWriter {
    writes: mpsc::UnboundedSender<Write>,
    /// When each item not yet completed started, for its record.
    started: std::sync::Mutex<HashMap<String, i64>>,
}

enum Write {
    Records(Vec<TurnRecord>),
    /// Answered once everything before it is stored.
    Flush(oneshot::Sender<()>),
}

impl HistoryWriter {
    /// A writer for session `session_id`.
    pub(crate) fn spawn(db: Database, session_id: String) -> Self {
        let (writes, mut queue) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(write) = queue.recv().await {
                match write {
                    Write::Records(records) => {
                        if let Err(e) = db.append_turn_records(&session_id, &records).await {
                            tracing::warn!(session_id, "couldn't store the session's turns: {e:#}");
                        }
                    }
                    Write::Flush(done) => {
                        let _ = done.send(());
                    }
                }
            }
        });
        Self {
            writes,
            started: std::sync::Mutex::default(),
        }
    }

    /// Store what `out` adds to the session's turns.
    pub(crate) fn record(&self, out: &[Outgoing]) {
        let mut started = self
            .started
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let records = records_of(out, &mut started);
        if !records.is_empty() {
            let _ = self.writes.send(Write::Records(records));
        }
    }

    /// Wait until everything recorded so far is stored.
    pub(crate) async fn flush(&self) {
        let (done, stored) = oneshot::channel();
        if self.writes.send(Write::Flush(done)).is_ok() {
            let _ = stored.await;
        }
    }
}

/// The records `out` makes: `turn/started`, `item/completed` and
/// `turn/completed`. An item's start time is kept in `started` until it
/// completes, as its record holds both.
fn records_of(out: &[Outgoing], started: &mut HashMap<String, i64>) -> Vec<TurnRecord> {
    let record = |turn_id: &str, kind: &str, payload: serde_json::Result<String>| {
        payload.ok().map(|payload| TurnRecord {
            turn_id: turn_id.to_string(),
            kind: kind.to_string(),
            payload,
        })
    };
    out.iter()
        .filter_map(|message| match message {
            Outgoing::Notification(ServerNotification::ItemStarted(n)) => {
                started.insert(n.item.id().to_string(), n.started_at_ms);
                None
            }
            Outgoing::Notification(ServerNotification::TurnStarted(n)) => {
                record(&n.turn.id, TURN, serde_json::to_string(&n.turn))
            }
            Outgoing::Notification(ServerNotification::TurnCompleted(n)) => {
                record(&n.turn.id, TURN, serde_json::to_string(&n.turn))
            }
            Outgoing::Notification(ServerNotification::ItemCompleted(n)) => record(
                &n.turn_id,
                ITEM,
                serde_json::to_string(&TurnItem {
                    parent_item_id: n.parent_item_id.clone(),
                    item: n.item.clone(),
                    started_at_ms: started.remove(n.item.id()),
                    completed_at_ms: Some(n.completed_at_ms),
                }),
            ),
            _ => None,
        })
        .collect()
}

// ── Paging ──────────────────────────────────────────────────────────────

/// How many turns a page has without a `limit`, and at most: Codex's.
const PAGE_DEFAULT: usize = 25;
const PAGE_MAX: usize = 100;

/// Where a page goes on from: a turn, and whether to include it. Codex's
/// cursor, as JSON, so a client treats it as opaque.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnCursor {
    turn_id: String,
    include_anchor: bool,
}

/// One page of `turns` (oldest first, as [`turns`] gives them), as Codex's
/// `thread/turns/list` pages: `limit` turns in `direction` after the
/// cursor's turn. A cursor whose turn is gone, or that isn't one, is
/// refused rather than read as the start.
pub(crate) fn page(
    turns: Vec<Turn>,
    cursor: Option<&str>,
    limit: Option<u32>,
    direction: koda_protocol::session::SortDirection,
) -> std::result::Result<
    koda_protocol::session::SessionTurnsListResponse,
    koda_protocol::rpc::ErrorObject,
> {
    use koda_protocol::session::{SessionTurnsListResponse, SortDirection};
    let anchor = cursor
        .map(|cursor| {
            serde_json::from_str::<TurnCursor>(cursor)
                .map_err(|_| crate::session::invalid(format!("invalid cursor: {cursor}")))
        })
        .transpose()?;
    let size = limit
        .map_or(PAGE_DEFAULT, |n| n as usize)
        .clamp(1, PAGE_MAX);
    let at = match &anchor {
        Some(anchor) => Some(
            turns
                .iter()
                .position(|turn| turn.id == anchor.turn_id)
                .ok_or_else(|| {
                    crate::session::invalid("invalid cursor: its turn is no longer there")
                })?,
        ),
        None => None,
    };
    let mut keyed: Vec<(usize, Turn)> = turns.into_iter().enumerate().collect();
    if direction == SortDirection::Desc {
        keyed.reverse();
    }
    if let (Some(anchor), Some(at)) = (&anchor, at) {
        keyed.retain(|(index, _)| match (direction, anchor.include_anchor) {
            (SortDirection::Asc, true) => *index >= at,
            (SortDirection::Asc, false) => *index > at,
            (SortDirection::Desc, true) => *index <= at,
            (SortDirection::Desc, false) => *index < at,
        });
    }
    let more = keyed.len() > size;
    keyed.truncate(size);
    let cursor_at = |turn: &Turn, include_anchor: bool| {
        serde_json::to_string(&TurnCursor {
            turn_id: turn.id.clone(),
            include_anchor,
        })
        .ok()
    };
    let backwards_cursor = keyed.first().and_then(|(_, turn)| cursor_at(turn, true));
    let next_cursor = if more {
        keyed.last().and_then(|(_, turn)| cursor_at(turn, false))
    } else {
        None
    };
    Ok(SessionTurnsListResponse {
        data: keyed.into_iter().map(|(_, turn)| turn).collect(),
        next_cursor,
        backwards_cursor,
    })
}

// ── Reading ─────────────────────────────────────────────────────────────

/// The turns of session `session_id`, oldest first. A turn still in
/// progress in its records ended without the host seeing it end, unless
/// `running` says the session is open and the turn is going on.
pub(crate) async fn turns(
    db: &dyn SessionStore,
    session_id: &str,
    running: bool,
) -> Result<Vec<Turn>> {
    let records = turn_records(db, session_id).await?;
    if records.is_empty() {
        return rebuild(db, session_id).await;
    }
    Ok(replay(&records, running))
}

/// [`turns`] for a session the host is opening again: a session without
/// records gets its rebuilt turns stored first, so the turns it runs now
/// follow them.
pub(crate) async fn turns_for_resume(db: &dyn SessionStore, session_id: &str) -> Result<Vec<Turn>> {
    let records = turn_records(db, session_id).await?;
    if !records.is_empty() {
        let mut turns = replay(&records, true);
        // The host is opening the session, so none of its turns is
        // running: one still in progress in its records ended with the
        // process that ran it. Store that end, or the turn would read as
        // running to everyone who looks while the session is open.
        if let Some(last) = turns
            .last_mut()
            .filter(|turn| turn.status == TurnStatus::InProgress)
        {
            last.status = TurnStatus::Interrupted;
            let bare = Turn {
                items: Vec::new(),
                ..last.clone()
            };
            let ended = serde_json::to_string(&bare).ok().map(|payload| TurnRecord {
                turn_id: last.id.clone(),
                kind: TURN.to_string(),
                payload,
            });
            checked(
                db.append_turn_records(session_id, &ended.into_iter().collect::<Vec<_>>())
                    .await,
            )?;
        }
        return Ok(turns);
    }
    let turns = rebuild(db, session_id).await?;
    let mut backfill = Vec::new();
    for turn in &turns {
        let bare = Turn {
            items: Vec::new(),
            ..turn.clone()
        };
        backfill.extend(serde_json::to_string(&bare).ok().map(|payload| TurnRecord {
            turn_id: turn.id.clone(),
            kind: TURN.to_string(),
            payload,
        }));
        for item in &turn.items {
            backfill.extend(serde_json::to_string(item).ok().map(|payload| TurnRecord {
                turn_id: turn.id.clone(),
                kind: ITEM.to_string(),
                payload,
            }));
        }
    }
    checked(db.append_turn_records(session_id, &backfill).await)?;
    Ok(turns)
}

/// The session's records of turns and items. Core also stores each turn's
/// context ([`koda_state::session_store::TurnContextItem`]) among them, for
/// debugging; it isn't part of a turn as a client sees it, so a session
/// whose only records are contexts has no turns stored.
async fn turn_records(db: &dyn SessionStore, session_id: &str) -> Result<Vec<TurnRecord>> {
    let mut records = checked(db.load_turn_records(session_id).await)?;
    records.retain(|record| matches!(record.kind.as_str(), TURN | ITEM));
    Ok(records)
}

/// The turns `records` hold, in the order they started.
fn replay(records: &[TurnRecord], running: bool) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    for record in records {
        let at = match index.get(record.turn_id.as_str()) {
            Some(&at) => at,
            None => {
                index.insert(&record.turn_id, turns.len());
                turns.push(Turn {
                    id: record.turn_id.clone(),
                    status: TurnStatus::InProgress,
                    error: None,
                    usage: None,
                    duration_ms: None,
                    items: Vec::new(),
                });
                turns.len() - 1
            }
        };
        match record.kind.as_str() {
            TURN => match serde_json::from_str::<Turn>(&record.payload) {
                Ok(turn) => {
                    let items = std::mem::take(&mut turns[at].items);
                    turns[at] = Turn { items, ..turn };
                }
                Err(e) => tracing::warn!("skipped a stored turn: {e}"),
            },
            ITEM => match serde_json::from_str::<TurnItem>(&record.payload) {
                Ok(item) => turns[at].items.push(item),
                Err(e) => tracing::warn!("skipped a stored item: {e}"),
            },
            other => tracing::debug!("skipped a turn record of kind {other:?}"),
        }
    }
    // Only the session's last turn can still be running.
    let last = turns.len().saturating_sub(1);
    for (i, turn) in turns.iter_mut().enumerate() {
        if turn.status == TurnStatus::InProgress && !(running && i == last) {
            turn.status = TurnStatus::Interrupted;
        }
    }
    turns
}

// ── Sessions from before 6h ─────────────────────────────────────────────

/// A session's turns rebuilt from the messages the model read, the way
/// [`crate::events::EventProjector`] would have sent them: each prompt
/// opens a turn, a reply's reasoning and text are items, and a tool call is
/// an item where its result was stored. Less is kept than a record keeps:
/// [`koda_protocol::session::Session::turns`] says what.
async fn rebuild(db: &dyn SessionStore, session_id: &str) -> Result<Vec<Turn>> {
    let messages = checked(db.load_all_messages(session_id).await)?;
    let mut progress = progress_by_call(db, session_id).await?;
    Ok(rebuild_from(&messages, &mut progress))
}

fn rebuild_from(messages: &[Message], progress: &mut HashMap<String, Vec<String>>) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    let push = |turns: &mut Vec<Turn>, item: Item| {
        if let Some(turn) = turns.last_mut() {
            turn.items.push(TurnItem {
                parent_item_id: None,
                item,
                started_at_ms: None,
                completed_at_ms: None,
            });
        }
    };
    // Calls started and not yet completed, by id: name and arguments.
    let mut open: HashMap<String, (String, Value)> = HashMap::new();
    // Whether the last row leaves a turn mid-way: a user row then is
    // steering text, not a new prompt.
    let mut mid_turn = false;

    for msg in messages {
        match msg.role {
            // The system prompt and compaction summaries.
            Role::System => {}
            Role::User => {
                if msg.tool_call_id.is_some() {
                    continue;
                }
                if !mid_turn || turns.is_empty() {
                    turns.push(Turn {
                        id: format!("turn-m{}", msg.id),
                        status: TurnStatus::Completed,
                        error: None,
                        usage: None,
                        duration_ms: None,
                        items: Vec::new(),
                    });
                }
                push(
                    &mut turns,
                    Item::UserMessage {
                        id: format!("user-m{}", msg.id),
                        text: msg.content.clone().unwrap_or_default(),
                        client_message_ids: Vec::new(),
                        mentions: Vec::new(),
                        images: Vec::new(),
                    },
                );
                mid_turn = false;
            }
            Role::Assistant => {
                if let Some(thinking) = msg.thinking_content.as_deref()
                    && !thinking.is_empty()
                {
                    push(
                        &mut turns,
                        Item::Reasoning {
                            id: format!("reasoning-m{}", msg.id),
                            text: thinking.to_string(),
                        },
                    );
                }
                if let Some(text) = msg.content.as_deref()
                    && !text.is_empty()
                {
                    push(
                        &mut turns,
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
                let output = msg.content.clone().unwrap_or_default();
                push(
                    &mut turns,
                    Item::ToolCall {
                        status: status_of(&output),
                        progress: progress.remove(&id).unwrap_or_default(),
                        id,
                        // Known from the tool's name, unlike its effect.
                        kind: Some(koda_protocol::item::ToolKind::of(&tool)),
                        tool,
                        arguments,
                        // Not stored with sessions from before turns were.
                        effect: None,
                        output: Some(output),
                        detail: None,
                        changes: Vec::new(),
                        exit_code: None,
                        duration_ms: None,
                    },
                );
            }
        }
    }
    // A turn left mid-way, or one whose question has no reply yet: the host
    // stopped before the model answered, as when it's killed in the middle
    // of a turn before the turn's own records were stored.
    if let Some(last) = turns.last_mut()
        && (mid_turn
            || matches!(
                last.items.last().map(|i| &i.item),
                Some(Item::UserMessage { .. })
            ))
    {
        last.status = TurnStatus::Interrupted;
    }
    turns
}

/// The notices stored for each call. A sub-agent's stored trace also names
/// each tool it started; those aren't notices, so they're left out.
async fn progress_by_call(
    db: &dyn SessionStore,
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

/// A stored result's status. The outcome itself isn't in the messages, so a
/// call that was refused or stopped reads as completed.
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
    use serde_json::json;

    fn calls(id: &str, name: &str, args: Value) -> String {
        json!([{"id": id, "function_name": name, "arguments": args.to_string()}]).to_string()
    }

    async fn db() -> (tempfile::TempDir, Database, String) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::init(dir.path()).await.unwrap();
        let session = db.create_session("default", dir.path()).await.unwrap();
        (dir, db, session)
    }

    async fn insert(db: &Database, session: &str, role: Role, content: &str) {
        db.insert_message(session, &role, Some(content), None, None, None)
            .await
            .unwrap();
    }

    fn turn_record(id: &str, status: TurnStatus) -> TurnRecord {
        TurnRecord {
            turn_id: id.into(),
            kind: TURN.into(),
            payload: serde_json::to_string(&Turn {
                id: id.into(),
                status,
                error: None,
                usage: None,
                duration_ms: Some(5),
                items: Vec::new(),
            })
            .unwrap(),
        }
    }

    fn item_record(turn: &str, parent: Option<&str>, text: &str) -> TurnRecord {
        TurnRecord {
            turn_id: turn.into(),
            kind: ITEM.into(),
            payload: serde_json::to_string(&TurnItem {
                parent_item_id: parent.map(str::to_string),
                item: Item::AgentMessage {
                    id: format!("message-{text}"),
                    text: text.into(),
                },
                started_at_ms: None,
                completed_at_ms: None,
            })
            .unwrap(),
        }
    }

    fn context_record(turn: &str) -> TurnRecord {
        TurnRecord {
            turn_id: turn.into(),
            kind: koda_state::session_store::TURN_CONTEXT_KIND.into(),
            payload: "{}".into(),
        }
    }

    /// Core stores each turn's context among the records, often before the
    /// host stores the turn. It is not part of a turn as a client sees it:
    /// it opens no turn, and a session holding only contexts, such as a
    /// sub-agent's, has no turns stored, so its turns are rebuilt from its
    /// messages.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn context_records_are_not_turns() {
        let (_dir2, old, old_session) = db().await;
        let (_dir, db, session) = db().await;
        db.append_turn_records(
            &session,
            &[
                context_record("t1"),
                turn_record("t1", TurnStatus::InProgress),
                item_record("t1", None, "one"),
                turn_record("t1", TurnStatus::Completed),
                context_record("t2"),
            ],
        )
        .await
        .unwrap();
        let read = turns(&db, &session, false).await.unwrap();
        assert_eq!(read.len(), 1, "{read:#?}");
        assert_eq!(read[0].id, "t1");
        assert_eq!(read[0].items.len(), 1);

        insert(&old, &old_session, Role::User, "hello").await;
        insert(&old, &old_session, Role::Assistant, "Hi.").await;
        old.append_turn_records(&old_session, &[context_record("t9")])
            .await
            .unwrap();
        let resumed = turns_for_resume(&old, &old_session).await.unwrap();
        assert_eq!(resumed.len(), 1, "rebuilt from the messages: {resumed:#?}");
        let kinds: Vec<String> = old
            .load_turn_records(&old_session)
            .await
            .unwrap()
            .into_iter()
            .map(|record| record.kind)
            .collect();
        assert!(
            kinds.iter().any(|kind| kind == TURN),
            "backfilled: {kinds:?}"
        );
    }

    /// Records replay in order: a turn's end replaces its start, its items
    /// keep their parents, and a turn that never ended is interrupted unless
    /// it is the running one.
    #[test]
    fn records_replay_as_the_turns_were_sent() {
        let records = [
            turn_record("t1", TurnStatus::InProgress),
            item_record("t1", None, "one"),
            item_record("t1", Some("call-a"), "sub"),
            turn_record("t1", TurnStatus::Completed),
            turn_record("t2", TurnStatus::InProgress),
            item_record("t2", None, "two"),
        ];
        let turns = replay(&records, false);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].status, TurnStatus::Completed);
        assert_eq!(turns[0].duration_ms, Some(5));
        assert_eq!(turns[0].items.len(), 2);
        assert_eq!(turns[0].items[1].parent_item_id.as_deref(), Some("call-a"));
        assert_eq!(turns[1].status, TurnStatus::Interrupted);
        assert_eq!(replay(&records, true)[1].status, TurnStatus::InProgress);
    }

    /// A host killed after storing a turn's question but before storing the
    /// turn itself (its records are written in the background): the turn is
    /// rebuilt from the messages, and with no reply it was cut off. Seen on
    /// CI as the kill test's first turn listed `completed`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_question_with_no_reply_is_an_interrupted_turn() {
        let (_dir, db, session) = db().await;
        insert(&db, &session, Role::System, "system prompt").await;
        insert(&db, &session, Role::User, "First question").await;
        let rebuilt = turns_for_resume(&db, &session).await.unwrap();
        assert_eq!(rebuilt.len(), 1, "{rebuilt:#?}");
        assert_eq!(rebuilt[0].status, TurnStatus::Interrupted);
        // Stored that way, so it stays interrupted once later turns follow.
        let stored = turns(&db, &session, false).await.unwrap();
        assert_eq!(stored[0].status, TurnStatus::Interrupted);

        // A question that was answered is still a completed turn.
        let (_answered_dir, answered, chat) = super::tests::db().await;
        insert(&answered, &chat, Role::User, "Hi").await;
        insert(&answered, &chat, Role::Assistant, "Hello.").await;
        let rebuilt = turns(&answered, &chat, false).await.unwrap();
        assert_eq!(rebuilt[0].status, TurnStatus::Completed);
    }

    /// A turn the server died in is stored as ended when the session is
    /// opened again: read while the session is open, it is interrupted, not
    /// still running.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_turn_that_died_with_the_server_is_stored_as_interrupted_on_resume() {
        let (_dir, db, session) = db().await;
        db.append_turn_records(
            &session,
            &[
                turn_record("t1", TurnStatus::Completed),
                turn_record("t2", TurnStatus::InProgress),
                item_record("t2", None, "half an answer"),
            ],
        )
        .await
        .unwrap();

        let resumed = turns_for_resume(&db, &session).await.unwrap();
        assert_eq!(resumed[1].status, TurnStatus::Interrupted);
        assert_eq!(resumed[1].items.len(), 1);

        // The session is open now; a second client reads it.
        let read = turns(&db, &session, true).await.unwrap();
        assert_eq!(read[0].status, TurnStatus::Completed);
        assert_eq!(read[1].status, TurnStatus::Interrupted);
        assert_eq!(read[1].items.len(), 1);
    }

    /// A session from before 6h: prompts open turns, steering text joins
    /// one, a call's stored notices are its progress, and a turn left
    /// mid-way is interrupted. Resuming it stores the rebuilt turns, which
    /// later reads replay.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_older_session_is_rebuilt_then_stored_on_resume() {
        let (_dir, db, session) = db().await;
        insert(&db, &session, Role::System, "system prompt").await;
        insert(&db, &session, Role::User, "Ask the agent").await;
        db.insert_message(
            &session,
            &Role::Assistant,
            Some("Asking."),
            Some(&calls(
                "call-a",
                "InvokeAgent",
                json!({"agent_name": "explore"}),
            )),
            None,
            None,
        )
        .await
        .unwrap();
        for line in [
            "  \u{1f527} Read",
            "  \u{23f3} explore: reached max turns (1)",
        ] {
            db.insert_session_event(
                &session,
                session_event_kind::SUB_AGENT_EVENT,
                line,
                Some("call-a"),
            )
            .await
            .unwrap();
        }
        db.insert_message(
            &session,
            &Role::Tool,
            Some("Error: no such file"),
            None,
            Some("call-a"),
            None,
        )
        .await
        .unwrap();
        insert(&db, &session, Role::User, "and steer").await;
        insert(&db, &session, Role::Assistant, "Done.").await;
        insert(&db, &session, Role::User, "Read it").await;
        db.insert_message(
            &session,
            &Role::Assistant,
            None,
            Some(&calls("call-r", "Read", json!({"path": "a.txt"}))),
            None,
            None,
        )
        .await
        .unwrap();

        let rebuilt = turns(&db, &session, false).await.unwrap();
        assert_eq!(rebuilt.len(), 2, "{rebuilt:#?}");
        let shape: Vec<String> = rebuilt[0]
            .items
            .iter()
            .map(|i| match &i.item {
                Item::UserMessage { text, .. } => format!("user {text}"),
                Item::AgentMessage { text, .. } => format!("message {text}"),
                Item::ToolCall {
                    tool,
                    status,
                    progress,
                    arguments,
                    ..
                } => format!("{tool} {status:?} {progress:?} {}", arguments["agent_name"]),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            shape,
            [
                "user Ask the agent",
                "message Asking.",
                "InvokeAgent Failed [\"\u{23f3} explore: reached max turns (1)\"] \"explore\"",
                "user and steer",
                "message Done.",
            ]
        );
        assert_eq!(rebuilt[0].status, TurnStatus::Completed);
        // A call with no stored result never completed, so it isn't listed.
        assert_eq!(rebuilt[1].items.len(), 1);
        assert_eq!(rebuilt[1].status, TurnStatus::Interrupted);
        // Nothing is stored by reading.
        assert!(db.load_turn_records(&session).await.unwrap().is_empty());

        let resumed = turns_for_resume(&db, &session).await.unwrap();
        assert_eq!(resumed, rebuilt);
        assert!(!db.load_turn_records(&session).await.unwrap().is_empty());
        assert_eq!(turns(&db, &session, false).await.unwrap(), rebuilt);
    }

    /// The writer stores what the host sends, in order, by the time a flush
    /// returns.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_writer_stores_turns_and_completed_items_only() {
        use koda_protocol::item::{ItemCompletedNotification, ItemStartedNotification};
        use koda_protocol::turn::{TurnCompletedNotification, TurnStartedNotification};
        let (_dir, db, session) = db().await;
        let writer = HistoryWriter::spawn(db.clone(), session.clone());
        let turn = |status| Turn {
            id: "t1".into(),
            status,
            error: None,
            usage: None,
            duration_ms: None,
            items: Vec::new(),
        };
        let item = Item::AgentMessage {
            id: "message-1".into(),
            text: "Hi.".into(),
        };
        let note = |n| Outgoing::Notification(n);
        writer.record(&[
            note(ServerNotification::TurnStarted(TurnStartedNotification {
                session_id: session.clone(),
                turn: turn(TurnStatus::InProgress),
            })),
            note(ServerNotification::ItemStarted(ItemStartedNotification {
                session_id: session.clone(),
                turn_id: "t1".into(),
                parent_item_id: None,
                item: item.clone(),
                started_at_ms: 1_000,
            })),
            note(ServerNotification::ItemCompleted(
                ItemCompletedNotification {
                    session_id: session.clone(),
                    turn_id: "t1".into(),
                    parent_item_id: None,
                    item: item.clone(),
                    completed_at_ms: 3_500,
                },
            )),
        ]);
        writer.record(&[note(ServerNotification::TurnCompleted(
            TurnCompletedNotification {
                session_id: session.clone(),
                turn: turn(TurnStatus::Completed),
            },
        ))]);
        writer.flush().await;
        let kinds: Vec<String> = db
            .load_turn_records(&session)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.kind)
            .collect();
        assert_eq!(kinds, [TURN, ITEM, TURN]);
        let turns = turns(&db, &session, false).await.unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].status, TurnStatus::Completed);
        assert_eq!(turns[0].items[0].item, item);
        // The item keeps when it started and completed, as Codex's does.
        assert_eq!(turns[0].items[0].started_at_ms, Some(1_000));
        assert_eq!(turns[0].items[0].completed_at_ms, Some(3_500));
    }

    // ── Paging ──────────────────────────────────────────────────────────

    use koda_protocol::session::SortDirection::{Asc, Desc};

    fn numbered(n: usize) -> Vec<Turn> {
        (0..n)
            .map(|i| Turn {
                id: format!("t{i}"),
                status: TurnStatus::Completed,
                error: None,
                usage: None,
                duration_ms: None,
                items: Vec::new(),
            })
            .collect()
    }

    fn ids(page: &koda_protocol::session::SessionTurnsListResponse) -> Vec<&str> {
        page.data.iter().map(|t| t.id.as_str()).collect()
    }

    #[test]
    fn pages_go_newest_first_and_follow_their_cursor() {
        let first = page(numbered(5), None, Some(2), Desc).unwrap();
        assert_eq!(ids(&first), ["t4", "t3"]);
        let second = page(numbered(5), first.next_cursor.as_deref(), Some(2), Desc).unwrap();
        assert_eq!(ids(&second), ["t2", "t1"]);
        let last = page(numbered(5), second.next_cursor.as_deref(), Some(2), Desc).unwrap();
        assert_eq!(ids(&last), ["t0"]);
        assert_eq!(last.next_cursor, None);
    }

    #[test]
    fn ascending_pages_start_from_the_oldest() {
        let first = page(numbered(3), None, Some(2), Asc).unwrap();
        assert_eq!(ids(&first), ["t0", "t1"]);
        let rest = page(numbered(3), first.next_cursor.as_deref(), None, Asc).unwrap();
        assert_eq!(ids(&rest), ["t2"]);
    }

    /// The backwards cursor reads the other way from the page's first turn,
    /// that turn included, as Codex's does.
    #[test]
    fn the_backwards_cursor_includes_the_pages_first_turn() {
        let newest = page(numbered(4), None, Some(2), Desc).unwrap();
        let back = page(numbered(4), newest.backwards_cursor.as_deref(), None, Asc).unwrap();
        assert_eq!(ids(&back), ["t3"]);
        let older = page(numbered(4), Some(&newest.next_cursor.unwrap()), None, Desc).unwrap();
        let back = page(numbered(4), older.backwards_cursor.as_deref(), None, Asc).unwrap();
        assert_eq!(ids(&back), ["t1", "t2", "t3"]);
    }

    #[test]
    fn the_limit_defaults_to_25_and_stops_at_100() {
        assert_eq!(page(numbered(30), None, None, Desc).unwrap().data.len(), 25);
        assert_eq!(
            page(numbered(150), None, Some(500), Desc)
                .unwrap()
                .data
                .len(),
            100
        );
        assert_eq!(
            page(numbered(3), None, Some(0), Desc).unwrap().data.len(),
            1
        );
    }

    #[test]
    fn a_bad_or_stale_cursor_is_refused() {
        let error = page(numbered(3), Some("nope"), None, Desc).unwrap_err();
        assert!(error.message.contains("invalid cursor"), "{error:?}");
        let gone = r#"{"turnId": "t9", "includeAnchor": false}"#;
        let error = page(numbered(3), Some(gone), None, Desc).unwrap_err();
        assert!(error.message.contains("no longer there"), "{error:?}");
    }

    #[test]
    fn no_turns_is_an_empty_page() {
        let empty = page(Vec::new(), None, None, Desc).unwrap();
        assert!(empty.data.is_empty());
        assert_eq!(empty.next_cursor, None);
        assert_eq!(empty.backwards_cursor, None);
    }
}
