//! `SessionStore` trait implementation for `Database`.
//!
//! All SQL queries that fulfill the `SessionStore` contract live here,
//! along with message sanitisation helpers that run on every `load_context`
//! call: orphan tool-call pruning, null-content cleanup, and whitespace-only
//! message removal.

use crate::busy_retry::{PoolWriteExt as _, WriteQueryExt as _};
use anyhow::Result;
use std::path::Path;

use super::{Database, MessageRow, SessionEventRow, SessionInfoRow};
use crate::session_store::{Message, Role, SessionEvent, SessionInfo, SessionStore, SessionUsage};

// ── Private helpers ─────────────────────────────────────────────────────────

/// Remove messages with mismatched tool_use / tool_result pairing (#428).
///
/// Uses the symmetric-difference approach (inspired by Code Puppy):
/// collect all tool_call IDs from calls and returns, find IDs that appear
/// in only one set, and drop any message referencing a mismatched ID.
///
/// Handles orphans from any source: interrupted sessions, compaction
/// boundaries, or session resume.
///
/// Dropping an assistant message orphans the results of its other calls, so
/// this repeats until a pass finds nothing. Each repeat removes at least one
/// message, which bounds the loop.
pub(crate) fn prune_mismatched_tool_calls(messages: &mut Vec<Message>) {
    while prune_mismatched_tool_calls_once(messages) {}
}

/// One pruning pass. Returns whether it found any mismatched ID.
fn prune_mismatched_tool_calls_once(messages: &mut Vec<Message>) -> bool {
    if messages.is_empty() {
        return false;
    }

    let mut tool_call_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut tool_return_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    for msg in messages.iter() {
        if msg.role == Role::Assistant {
            if let Some(ref tc_json) = msg.tool_calls
                && let Ok(calls) = serde_json::from_str::<Vec<serde_json::Value>>(tc_json)
            {
                for call in &calls {
                    if let Some(id) = call.get("id").and_then(|v| v.as_str()) {
                        tool_call_ids.insert(id.to_string());
                    }
                }
            }
        } else if msg.role == Role::Tool
            && let Some(ref id) = msg.tool_call_id
        {
            tool_return_ids.insert(id.clone());
        }
    }

    let mismatched: std::collections::HashSet<&String> = tool_call_ids
        .symmetric_difference(&tool_return_ids)
        .collect();

    if mismatched.is_empty() {
        return false;
    }

    messages.retain(|msg| {
        // Drop tool_result messages with mismatched IDs
        if msg.role == Role::Tool
            && let Some(ref id) = msg.tool_call_id
            && mismatched.contains(id)
        {
            return false;
        }
        // Drop assistant messages whose tool_calls contain mismatched IDs
        if msg.role == Role::Assistant
            && let Some(ref tc_json) = msg.tool_calls
            && let Ok(calls) = serde_json::from_str::<Vec<serde_json::Value>>(tc_json)
        {
            let has_mismatched = calls.iter().any(|call| {
                call.get("id")
                    .and_then(|v| v.as_str())
                    .is_some_and(|id| mismatched.contains(&id.to_string()))
            });
            if has_mismatched {
                return false;
            }
        }
        true
    });
    true
}

/// One sanitization pass that removed rows from the model's view.
#[derive(Debug, PartialEq)]
pub(crate) struct HistoryEdit {
    /// The pass, named after its function.
    pub pass: &'static str,
    /// Removed row ids, oldest first.
    pub ids: Vec<i64>,
    /// How many removed rows were tool results.
    pub tool_rows: usize,
    /// Characters of text the model no longer sees.
    pub chars: usize,
}

impl HistoryEdit {
    /// The rows stay in the database, so without this line a pass that hides
    /// text from the model leaves no trace: repeated call ids once hid every
    /// tool result but the newest for whole runs (#1454).
    fn log(&self, session_id: &str) {
        let Self {
            pass,
            ids,
            tool_rows,
            chars,
        } = self;
        let rows = ids.len();
        if *chars > 0 {
            tracing::warn!(
                session_id,
                pass,
                rows,
                tool_rows,
                chars,
                ?ids,
                "history edited"
            );
        } else {
            tracing::info!(
                session_id,
                pass,
                rows,
                tool_rows,
                chars,
                ?ids,
                "history edited"
            );
        }
    }
}

/// A sanitization pass and the name it reports under.
type SanitizePass = (&'static str, fn(&mut Vec<Message>));

/// Run every sanitization pass in order, each on the output of the previous,
/// and report what each one removed.
pub(crate) fn sanitize_history(messages: &mut Vec<Message>) -> Vec<HistoryEdit> {
    let passes: [SanitizePass; 5] = [
        ("prune_legacy_compaction_hint", prune_legacy_compaction_hint), // (#1444)
        ("prune_mismatched_tool_calls", prune_mismatched_tool_calls),   // (#428)
        ("prune_null_content_messages", prune_null_content_messages),   // (#594)
        (
            "prune_whitespace_only_messages",
            prune_whitespace_only_messages,
        ), // (#594)
        (
            "dedupe_tool_results_by_call_id",
            dedupe_tool_results_by_call_id,
        ), // (#1159)
    ];
    let mut edits = Vec::new();
    for (pass, run) in passes {
        let before: Vec<(i64, bool, usize)> = messages
            .iter()
            .map(|m| {
                let chars = m.content.as_deref().map_or(0, |c| c.trim().len());
                (m.id, m.role == Role::Tool, chars)
            })
            .collect();
        run(messages);
        if messages.len() == before.len() {
            continue;
        }
        let kept: std::collections::HashSet<i64> = messages.iter().map(|m| m.id).collect();
        let removed = before.iter().filter(|(id, ..)| !kept.contains(id));
        edits.push(HistoryEdit {
            pass,
            ids: removed.clone().map(|(id, ..)| *id).collect(),
            tool_rows: removed.clone().filter(|(_, tool, _)| *tool).count(),
            chars: removed.map(|(.., chars)| chars).sum(),
        });
    }
    edits
}

/// Which rows a compaction leaves in place.
enum KeepRows {
    /// The newest N rows.
    Last(usize),
    /// Every row from this id on, except earlier summaries: the new summary
    /// was written from them, whatever their ids.
    From(i64),
}

impl Database {
    /// Archive the rows `keep` leaves out and store `summary` in their place.
    async fn archive_into_summary(
        &self,
        session_id: &str,
        summary: &str,
        keep: KeepRows,
        usage: Option<&crate::session_store::MessageUsage>,
    ) -> Result<usize> {
        // `BEGIN IMMEDIATE`, not the default deferred `BEGIN`: this
        // transaction reads first and writes later. In WAL mode a
        // deferred read that is upgraded to a write after another
        // connection committed fails at once with SQLITE_BUSY
        // ("database is locked"); `busy_timeout` does not apply to
        // that upgrade. Taking the write lock up front makes the
        // wait honour the timeout, so concurrent event inserts
        // (`PersistingSink`) and sub-agent writes queue behind it.
        let mut tx = self.pool.begin_immediate().await?;

        // Get active (non-compacted) message IDs ordered oldest→newest
        let all_ids: Vec<(i64, String)> = sqlx::query_as(
            "SELECT id, role FROM messages WHERE session_id = ? AND compacted_at IS NULL ORDER BY id ASC",
        )
        .bind(session_id)
        .fetch_all(&mut *tx)
        .await?;

        let total = all_ids.len();
        if total == 0 {
            tx.commit().await?;
            return Ok(0);
        }

        // Determine which messages to archive (everything except the tail)
        let ids_to_archive: Vec<i64> = all_ids
            .iter()
            .enumerate()
            .filter(|(index, (id, role))| match keep {
                KeepRows::Last(count) => *index < total.saturating_sub(count),
                KeepRows::From(keep_from) => *id < keep_from || role == "system",
            })
            .map(|(_, (id, _))| *id)
            .collect();
        let archived_count = ids_to_archive.len();

        if archived_count == 0 {
            tx.commit().await?;
            return Ok(0);
        }

        // Mark old messages as compacted (non-destructive — history preserved in DB)
        for chunk in ids_to_archive.chunks(500) {
            let placeholders: String = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "UPDATE messages SET compacted_at = datetime('now') \
                 WHERE session_id = ? AND id IN ({placeholders})"
            );
            let mut query = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(session_id);
            for id in chunk {
                query = query.bind(id);
            }
            query.execute(&mut *tx).await?;
        }

        // Insert the summary as a system message
        sqlx::query(
            "INSERT INTO messages (session_id, role, content, tool_calls, tool_call_id,
                 prompt_tokens, completion_tokens, cache_read_tokens, cache_creation_tokens,
                 thinking_tokens)
             VALUES (?, 'system', ?, NULL, NULL, ?, ?, ?, ?, ?)",
        )
        .bind(session_id)
        .bind(summary)
        .bind(usage.map(|u| u.prompt_tokens))
        .bind(usage.map(|u| u.completion_tokens))
        .bind(usage.map(|u| u.cache_read_tokens))
        .bind(usage.map(|u| u.cache_creation_tokens))
        .bind(usage.map(|u| u.thinking_tokens))
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        // (#1166) Bump the global compaction generation: any cached
        // context entry whose snapshot predates this bump must do a
        // full reload on next `load_context`. Compaction is the only
        // mutation that retroactively changes the row set
        // (sets `compacted_at` on existing rows), so this is the only
        // path that needs to invalidate cached merges.
        //
        // We bump after `commit` (not before) so that any concurrent
        // `load_context` call still sees the pre-compaction state
        // consistently — either it caches with the old gen and gets
        // invalidated next call, or it caches with the new gen and
        // re-fetches the post-compaction set. Idempotent either way.
        self.compaction_gen
            .fetch_add(1, std::sync::atomic::Ordering::Release);

        Ok(archived_count)
    }
}

/// The continuation hint compaction stored as an assistant row before #1444.
const LEGACY_COMPACTION_HINT: &str = "Your context was compacted. The previous message contains \
    a summary of our earlier conversation. Do not mention the summary or that compaction \
    occurred. Continue the conversation naturally based on the summarized context.";

/// Drop the old hint row from sessions compacted before #1444. The model never
/// said it, and the summary now carries its own hint.
pub(crate) fn prune_legacy_compaction_hint(messages: &mut Vec<Message>) {
    messages.retain(|msg| {
        msg.role != Role::Assistant
            || msg.tool_calls.is_some()
            || msg.content.as_deref() != Some(LEGACY_COMPACTION_HINT)
    });
}

/// Drop assistant messages that have neither text content nor tool calls.
///
/// These are "ghost" messages: the stream was interrupted (network drop or
/// Ctrl+C) before the model produced any output — only thinking deltas
/// arrived, which are never persisted. Sending such a message to the
/// provider on resume triggers `invalid_request_error` (all-null content).
///
/// Note: this situation should no longer arise after the `NetworkError`
/// detection fix in #594, but the prune pass acts as a safety net for
/// any messages that slipped through before the fix was deployed.
pub(crate) fn prune_null_content_messages(messages: &mut Vec<Message>) {
    messages.retain(|msg| {
        if msg.role != Role::Assistant {
            return true; // keep non-assistant messages unchanged
        }
        let has_content = msg.content.as_deref().is_some_and(|c| !c.trim().is_empty());
        let has_tool_calls = msg.tool_calls.is_some();
        has_content || has_tool_calls
    });
}

/// Drop assistant messages whose text content is entirely whitespace.
///
/// These appear when the connection is lost just after the model emits a
/// `\n\n` prefix (common before a `<think>` block). The resulting message
/// is harmless but adds noise to the conversation and can confuse the model
/// into thinking it already replied to the previous user turn.
pub(crate) fn prune_whitespace_only_messages(messages: &mut Vec<Message>) {
    messages.retain(|msg| {
        if msg.role != Role::Assistant {
            return true;
        }
        // Keep if there are tool calls regardless of content.
        if msg.tool_calls.is_some() {
            return true;
        }
        // Drop if content is present but whitespace-only.
        !matches!(msg.content.as_deref(), Some(c) if c.trim().is_empty())
    });
}

/// Dedupe `Role::Tool` rows that share a `tool_call_id`, keeping only the
/// **latest** (highest `id` — our `created_at` proxy since both are
/// monotonic per session) for each id.
///
/// # Why
///
/// (#1159) Background sub-agents now emit a `Role::Tool` row at completion
/// keyed on the parent `InvokeAgent`'s `tool_call_id`. The original
/// dispatch-turn write (the synchronous "Background agent X started
/// (agent:N)..." stub returned by `InvokeAgent`) is *also* a `Role::Tool`
/// row with the same `tool_call_id`. Sending both to a provider violates
/// the `tool_use_id` ↔ `tool_result_id` 1:1 invariant that Anthropic
/// enforces (and that OpenAI/Gemini handle via last-write-wins anyway).
///
/// This pass is a logical "update via append" — we keep every row in the
/// DB for transcript fidelity (debug bundles, `/copy`, etc. see the full
/// history via `load_all_messages`) but the model only sees the latest
/// status per call when its context is loaded for the next turn.
///
/// # Algorithm
///
/// O(n) two-pass. Pass 1 collects the max `id` per `tool_call_id` for
/// `Role::Tool` rows. Pass 2 retains a `Role::Tool` row only if it is the
/// max-id row for its id (or has no `tool_call_id`, which is impossible
/// for valid Tool rows but defended against). Non-Tool rows are untouched.
pub(crate) fn dedupe_tool_results_by_call_id(messages: &mut Vec<Message>) {
    if messages.is_empty() {
        return;
    }

    // Pass 1: max id per tool_call_id, restricted to Role::Tool rows.
    let mut latest_id_for: std::collections::HashMap<String, i64> =
        std::collections::HashMap::new();
    for msg in messages.iter() {
        if msg.role == Role::Tool
            && let Some(call_id) = msg.tool_call_id.as_deref()
        {
            latest_id_for
                .entry(call_id.to_string())
                .and_modify(|cur| {
                    if msg.id > *cur {
                        *cur = msg.id;
                    }
                })
                .or_insert(msg.id);
        }
    }

    // Early-out: every Role::Tool row was already unique-by-call_id.
    if latest_id_for.is_empty() {
        return;
    }

    // Pass 2: retain non-Tool rows + the latest Tool row per call_id.
    messages.retain(|msg| {
        if msg.role != Role::Tool {
            return true;
        }
        let Some(call_id) = msg.tool_call_id.as_deref() else {
            // Defensive: a Tool row with no tool_call_id is malformed but
            // we don't drop it here — `prune_mismatched_tool_calls` will
            // handle it on a subsequent pass if needed.
            return true;
        };
        latest_id_for
            .get(call_id)
            .is_none_or(|&max_id| msg.id == max_id)
    });
}

/// The start of a prompt, as a session's preview.
fn preview_of(text: &str) -> String {
    text.chars()
        .take(crate::session_store::PREVIEW_CHARS)
        .collect()
}

impl Database {
    /// Archive session `session_id` and its descendants, or bring them
    /// back, in one transaction: the ids it changed, `session_id` first,
    /// then the descendants, the deepest first. Nothing when `session_id`
    /// already is as asked, or names no session; a descendant that already
    /// is stays as it was.
    async fn set_archived(&self, session_id: &str, archived: bool) -> Result<Vec<String>> {
        let mut tx = self.pool.begin_immediate().await?;
        // The session and the sub-agents' sessions under it, each with how
        // far down it is. `UNION` and a depth limit, so rows that name each
        // other as parent (which nothing writes) can't loop for ever.
        let subtree: Vec<(String, i64, bool)> = sqlx::query_as(
            "WITH RECURSIVE subtree(id, depth) AS (
               SELECT id, 0 FROM sessions WHERE id = ?
               UNION
               SELECT s.id, t.depth + 1 FROM sessions s
                 JOIN subtree t ON s.parent_session_id = t.id
                WHERE t.depth < 64
             )
             SELECT t.id, MIN(t.depth) AS level, s.archived_at IS NOT NULL
               FROM subtree t JOIN sessions s ON s.id = t.id
              GROUP BY t.id
              ORDER BY (t.id = ?) DESC, level DESC, s.rowid",
        )
        .bind(session_id)
        .bind(session_id)
        .fetch_all(&mut *tx)
        .await?;
        let root_changes = subtree
            .first()
            .is_some_and(|(id, _, is)| id == session_id && *is != archived);
        if !root_changes {
            return Ok(Vec::new());
        }
        let set = if archived {
            "UPDATE sessions SET archived_at = datetime('now') WHERE id = ?"
        } else {
            "UPDATE sessions SET archived_at = NULL WHERE id = ?"
        };
        let mut changed = Vec::new();
        for (id, _, is) in subtree {
            if is == archived {
                continue;
            }
            sqlx::query(set).bind(&id).execute(&mut *tx).await?;
            changed.push(id);
        }
        tx.commit().await?;
        Ok(changed)
    }
}

// ── Listing and searching sessions ──────────────────────────────────────────────────

/// A session's summary: [`SessionInfoRow`]'s columns, before a filter, all
/// from the session's own row, so listing reads no messages. A macro, so
/// queries are built with `concat!` and stay static SQL.
macro_rules! session_info_select {
    () => {
        "SELECT s.id, s.agent_name, s.created_at,
        s.title, s.mode,
        CAST(strftime('%s', s.created_at) AS INTEGER) as created_unix,
        CAST(strftime('%s', COALESCE(s.last_accessed_at, s.created_at)) AS INTEGER)
            as updated_unix,
        s.preview,
        s.project_root, s.provider, s.base_url, s.model,
        s.source, s.originator, s.forked_from_id, s.parent_session_id, s.ephemeral,
        CAST(strftime('%s', s.archived_at) AS INTEGER) as archived_unix,
        s.confidential, s.confidential_brief, s.confidential_brief_draft,
        s.rowid as sequence
     FROM sessions s"
    };
}

impl Database {
    /// One page of sessions for `query`, in its order, narrowed to those
    /// whose stored text matches `text` when there is one.
    async fn sessions_page(
        &self,
        query: &crate::session_store::SessionListQuery,
        text: Option<&StoredTextPatterns>,
    ) -> Result<Vec<SessionInfo>> {
        // The sort time as whole seconds, with the order sessions were made
        // in for those of one second: what a cursor names.
        let at = if query.by_updated {
            "CAST(strftime('%s', COALESCE(s.last_accessed_at, s.created_at)) AS INTEGER)"
        } else {
            "CAST(strftime('%s', s.created_at) AS INTEGER)"
        };
        let mut sql = sqlx::QueryBuilder::<sqlx::Sqlite>::new(session_info_select!());
        sql.push(" WHERE s.ephemeral = 0");
        sql.push(if query.archived {
            " AND s.archived_at IS NOT NULL"
        } else {
            " AND s.archived_at IS NULL"
        });
        if let Some(roots) = &query.project_roots {
            sql.push(" AND (s.project_root IS NULL OR s.project_root IN (");
            let mut list = sql.separated(", ");
            for root in roots {
                list.push_bind(root.clone());
            }
            sql.push("))");
        }
        if let Some(sources) = &query.sources {
            sql.push(" AND (");
            if query.unknown_source {
                sql.push("s.source IS NULL OR ");
            }
            sql.push("s.source IN (");
            let mut list = sql.separated(", ");
            for source in sources {
                list.push_bind(source.clone());
            }
            sql.push("))");
        }
        if let Some(providers) = &query.providers {
            sql.push(" AND s.provider IN (");
            let mut list = sql.separated(", ");
            for provider in providers {
                list.push_bind(provider.clone());
            }
            sql.push(")");
        }
        if let Some(term) = &query.search_term {
            let pattern = like_contains(term);
            sql.push(" AND (s.title LIKE ");
            sql.push_bind(pattern.clone());
            sql.push(" ESCAPE '\\' OR s.preview LIKE ");
            sql.push_bind(pattern);
            sql.push(" ESCAPE '\\')");
        }
        if let Some(text) = text {
            // A session with turn records is read from them; one without,
            // from its messages, where the system prompt is no part of the
            // conversation.
            sql.push(
                " AND (EXISTS (SELECT 1 FROM turn_records t \
                     WHERE t.session_id = s.id AND t.kind = 'item' AND t.payload LIKE ",
            );
            sql.push_bind(text.records.clone());
            sql.push(
                " ESCAPE '\\') OR (NOT EXISTS (SELECT 1 FROM turn_records t \
                     WHERE t.session_id = s.id AND t.kind IN ('turn', 'item')) \
                   AND EXISTS (SELECT 1 FROM messages m \
                     WHERE m.session_id = s.id AND m.role != 'system' AND (m.content LIKE ",
            );
            sql.push_bind(text.messages.clone());
            sql.push(" ESCAPE '\\' OR ");
            match &text.tool_calls {
                // Some providers write `/` as `\/`.
                Some(pattern) => {
                    sql.push("REPLACE(m.tool_calls, '\\\\/', '/') LIKE ");
                    sql.push_bind(pattern.clone());
                    sql.push(" ESCAPE '\\'");
                }
                None => {
                    sql.push("m.tool_calls IS NOT NULL");
                }
            }
            sql.push("))))");
        }
        let (before, order) = if query.ascending {
            (">", "ASC")
        } else {
            ("<", "DESC")
        };
        if let Some((time, sequence)) = query.after {
            sql.push(format!(" AND ({at}, s.rowid) {before} ("));
            sql.push_bind(time);
            sql.push(", ");
            sql.push_bind(sequence);
            sql.push(")");
        }
        sql.push(format!(" ORDER BY {at} {order}, s.rowid {order} LIMIT "));
        sql.push_bind(query.limit);
        let rows: Vec<SessionInfoRow> = sql.build_query_as().fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }
}

/// `term` as a `LIKE` pattern that finds it anywhere in a text, for
/// `ESCAPE '\'`.
fn like_contains(term: &str) -> String {
    let mut pattern = String::with_capacity(term.len() + 2);
    pattern.push('%');
    for c in term.chars() {
        if matches!(c, '\\' | '%' | '_') {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    pattern.push('%');
    pattern
}

/// What [`SessionStore::search_candidates_page`] looks for, as `LIKE`
/// patterns.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct StoredTextPatterns {
    /// In an item's record, which is JSON that Koda wrote: the term as a
    /// JSON string holds it, as Codex looks for the escaped term in a
    /// rollout's lines.
    records: String,
    /// In a message's text, stored as it is.
    messages: String,
    /// In a message's tool calls; `None` when any session with a tool call
    /// may hold the term.
    tool_calls: Option<String>,
}

/// The patterns that find every stored text `term` occurs in, compared with
/// [`search_fold`](crate::session_store::search_fold); `None` when `LIKE`
/// can't promise that, and the caller is to read every session.
///
/// `LIKE` ignores case for ASCII letters only, so a letter outside ASCII
/// that has another case rules it out. So does a control character: a line
/// break or a tab may stand between two stored texts in what the caller
/// reads, where no stored text has it.
///
/// A session from before turn records keeps its tool calls as the provider
/// sent them, the arguments as the model's own JSON text, which may escape
/// a character Koda wouldn't (`\u00e9`, Go's `\u003c`) or write a number
/// another way (`1e3`). There the term is looked for only when no JSON
/// writer escapes any of its characters (ASCII letters, digits, spaces and
/// `-_.:/`, with `\/` read as `/`) and it isn't all digits and number
/// signs.
pub(super) fn stored_text_patterns(term: &str) -> Option<StoredTextPatterns> {
    use crate::session_store::search_fold;
    let like_agrees = |c: char| {
        !c.is_control()
            && (c.is_ascii() || !(c.is_lowercase() || c.is_uppercase() || search_fold(c) != c))
    };
    if !term.chars().all(like_agrees) {
        return None;
    }
    let json = serde_json::to_string(term).ok()?;
    let as_written =
        |c: char| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.' | ':' | '/');
    let in_a_number = |c: char| matches!(c, '0'..='9' | '.' | '-' | '+' | 'e' | 'E');
    let plain = term.chars().all(as_written) && !term.chars().all(in_a_number);
    Some(StoredTextPatterns {
        records: like_contains(&json[1..json.len() - 1]),
        messages: like_contains(term),
        tool_calls: plain.then(|| like_contains(term)),
    })
}

// ── SessionStore trait ──────────────────────────────────────────────────────────────

#[async_trait::async_trait]
impl SessionStore for Database {
    /// Create a new session, returning the generated session ID.
    async fn create_session_from(
        &self,
        agent_name: &str,
        project_root: &Path,
        origin: &crate::session_store::SessionOrigin,
    ) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let root = project_root.to_string_lossy().to_string();
        sqlx::query(
            "INSERT INTO sessions \
               (id, agent_name, project_root, last_accessed_at, \
                source, originator, koda_version, parent_session_id, ephemeral, confidential) \
             VALUES (?, ?, ?, datetime('now'), ?, ?, ?, ?, ?, \
                COALESCE((SELECT p.confidential FROM sessions p WHERE p.id = ?), 0))",
        )
        .bind(&id)
        .bind(agent_name)
        .bind(&root)
        .bind(origin.source.as_deref())
        .bind(origin.originator.as_deref())
        .bind(env!("CARGO_PKG_VERSION"))
        .bind(origin.parent_session_id.as_deref())
        .bind(origin.ephemeral)
        // A sub-agent's session holds parts of its parent's conversation.
        .bind(origin.parent_session_id.as_deref())
        .execute_write(&self.pool)
        .await?;
        tracing::info!("Created session: {id} (project: {root})");
        Ok(id)
    }

    async fn fork_session(
        &self,
        source: &str,
        title: &str,
        project_root: &Path,
        origin: &crate::session_store::SessionOrigin,
    ) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let root = project_root.to_string_lossy().to_string();
        let mut tx = self.pool.begin_immediate().await?;
        let copied = sqlx::query(
            "INSERT INTO sessions \
               (id, agent_name, project_root, last_accessed_at, title, mode, provider, base_url, model, \
                preview, forked_from_id, source, originator, koda_version, ephemeral, \
                confidential) \
             SELECT ?, agent_name, ?, datetime('now'), ?, mode, provider, base_url, model, \
                preview, id, ?, ?, ?, ?, confidential \
             FROM sessions WHERE id = ?",
        )
        .bind(&id)
        .bind(&root)
        .bind(title)
        .bind(origin.source.as_deref())
        .bind(origin.originator.as_deref())
        .bind(env!("CARGO_PKG_VERSION"))
        .bind(origin.ephemeral)
        .bind(source)
        .execute(&mut *tx)
        .await?;
        anyhow::ensure!(copied.rows_affected() == 1, "no session {source}");
        sqlx::query(
            "INSERT INTO messages \
               (session_id, role, content, full_content, tool_calls, tool_call_id, \
                prompt_tokens, completion_tokens, cache_read_tokens, cache_creation_tokens, \
                thinking_tokens, thinking_content, thinking_model, thinking_blocks, agent_name, \
                compacted_at, completed_at, created_at, images) \
             SELECT ?, role, content, full_content, tool_calls, tool_call_id, \
                prompt_tokens, completion_tokens, cache_read_tokens, cache_creation_tokens, \
                thinking_tokens, thinking_content, thinking_model, thinking_blocks, agent_name, \
                compacted_at, completed_at, created_at, images \
             FROM messages WHERE session_id = ? ORDER BY id",
        )
        .bind(&id)
        .bind(source)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO turn_records (session_id, turn_id, kind, payload, created_at) \
             SELECT ?, turn_id, kind, payload, created_at \
             FROM turn_records WHERE session_id = ? ORDER BY id",
        )
        .bind(&id)
        .bind(source)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO session_metadata (session_id, key, value, updated_at) \
             SELECT ?, key, value, updated_at FROM session_metadata \
             WHERE session_id = ? AND key != ?",
        )
        .bind(&id)
        .bind(source)
        .bind(crate::session_store::PARENT_SESSION_KEY)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO session_events (session_id, kind, payload, parent_tool_call_id, created_at) \
             SELECT ?, kind, payload, parent_tool_call_id, created_at \
             FROM session_events WHERE session_id = ? ORDER BY id",
        )
        .bind(&id)
        .bind(source)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        tracing::info!("Forked session {source} as {id}");
        Ok(id)
    }

    /// Insert a message into the conversation log.
    async fn insert_message(
        &self,
        session_id: &str,
        role: &Role,
        content: Option<&str>,
        tool_calls: Option<&str>,
        tool_call_id: Option<&str>,
        usage: Option<&crate::session_store::MessageUsage>,
    ) -> Result<i64> {
        self.insert_message_with_agent(
            session_id,
            role,
            content,
            tool_calls,
            tool_call_id,
            usage,
            None,
        )
        .await
    }

    /// Insert a message with an optional agent name for cost tracking.
    #[allow(clippy::too_many_arguments)]
    async fn insert_message_with_agent(
        &self,
        session_id: &str,
        role: &Role,
        content: Option<&str>,
        tool_calls: Option<&str>,
        tool_call_id: Option<&str>,
        usage: Option<&crate::session_store::MessageUsage>,
        agent_name: Option<&str>,
    ) -> Result<i64> {
        let result = sqlx::query(
            "INSERT INTO messages (session_id, role, content, tool_calls, tool_call_id, \
             prompt_tokens, completion_tokens, cache_read_tokens, cache_creation_tokens, \
             thinking_tokens, agent_name)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(session_id)
        .bind(role.as_str())
        .bind(content)
        .bind(tool_calls)
        .bind(tool_call_id)
        .bind(usage.map(|u| u.prompt_tokens))
        .bind(usage.map(|u| u.completion_tokens))
        .bind(usage.map(|u| u.cache_read_tokens))
        .bind(usage.map(|u| u.cache_creation_tokens))
        .bind(usage.map(|u| u.thinking_tokens))
        .bind(agent_name)
        .execute_write(&self.pool)
        .await?;

        // The session was written now, and its first prompt is its preview.
        let preview = (matches!(role, Role::User))
            .then(|| content.map(preview_of))
            .flatten();
        sqlx::query(
            "UPDATE sessions SET last_accessed_at = datetime('now'), \
             preview = COALESCE(preview, ?) WHERE id = ?",
        )
        .bind(preview)
        .bind(session_id)
        .execute_write(&self.pool)
        .await?;

        Ok(result.last_insert_rowid())
    }

    async fn insert_tool_message_with_full(
        &self,
        session_id: &str,
        content: &str,
        tool_call_id: &str,
        full_content: &str,
    ) -> Result<i64> {
        let result = sqlx::query(
            "INSERT INTO messages (session_id, role, content, full_content, tool_call_id) \
             VALUES (?, 'tool', ?, ?, ?)",
        )
        .bind(session_id)
        .bind(content)
        .bind(full_content)
        .bind(tool_call_id)
        .execute_write(&self.pool)
        .await?;

        sqlx::query("UPDATE sessions SET last_accessed_at = datetime('now') WHERE id = ?")
            .bind(session_id)
            .execute_write(&self.pool)
            .await?;

        Ok(result.last_insert_rowid())
    }

    async fn mark_message_complete(&self, message_id: i64) -> Result<()> {
        sqlx::query("UPDATE messages SET completed_at = datetime('now') WHERE id = ?")
            .bind(message_id)
            .execute_write(&self.pool)
            .await?;
        Ok(())
    }

    /// **#1022 B20**: atomic batch copy of messages into a session.
    ///
    /// Replaces the pre-fix per-row loop (`insert_message` x N times,
    /// each a separate UPDATE-last_accessed_at + optional
    /// `mark_message_complete`). All inserts run inside a single sqlx
    /// transaction — one fsync at COMMIT instead of N — with
    /// `completed_at` written inline for assistant rows so no follow-up
    /// UPDATE is needed.
    ///
    /// Errors mid-loop bubble up via `?`; the transaction is dropped
    /// without `commit()`, so the destination session sees no partial
    /// state. Sqlx rolls back on Drop.
    async fn copy_messages_into_session(
        &self,
        dst_session: &str,
        messages: &[Message],
    ) -> Result<()> {
        // Empty-input shortcut: skip the BEGIN/COMMIT roundtrip
        // entirely. Cheap branch; matches "do nothing if there's
        // nothing to do" intent and avoids touching the WAL for
        // a no-op fork (fresh session has no history).
        if messages.is_empty() {
            return Ok(());
        }

        let mut tx = self.pool.begin_immediate().await?;

        for msg in messages {
            // Inline `completed_at`: assistant rows are born complete
            // (they've already been delivered to the parent session),
            // everything else stays NULL. This eliminates the
            // per-row `mark_message_complete` round-trip from the
            // pre-fix loop. Mirrors the `completed_at = datetime('now')`
            // inline pattern already used by the compaction continuation
            // hint elsewhere in this file.
            sqlx::query(
                "INSERT INTO messages (session_id, role, content, tool_calls, tool_call_id, completed_at) \
                 VALUES (?, ?, ?, ?, ?, \
                   CASE WHEN ? = 'assistant' THEN datetime('now') ELSE NULL END)",
            )
            .bind(dst_session)
            .bind(msg.role.as_str())
            .bind(msg.content.as_deref())
            .bind(msg.tool_calls.as_deref())
            .bind(msg.tool_call_id.as_deref())
            .bind(msg.role.as_str()) // bound twice for the CASE; sqlx has no positional reuse
            .execute(&mut *tx)
            .await?;
        }

        // Single last_accessed_at bump for the whole batch. Parent's
        // per-row pattern updated this N times; the value is
        // monotonic-by-second so collapsing to one update is
        // observationally identical.
        sqlx::query("UPDATE sessions SET last_accessed_at = datetime('now') WHERE id = ?")
            .bind(dst_session)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(())
    }

    /// Persist signed thinking blocks. The row isn't complete yet, so no
    /// cached context holds it and the cache needs no bump.
    async fn update_message_thinking_blocks(&self, message_id: i64, blocks: &str) -> Result<()> {
        sqlx::query("UPDATE messages SET thinking_blocks = ? WHERE id = ?")
            .bind(blocks)
            .bind(message_id)
            .execute_write(&self.pool)
            .await?;
        Ok(())
    }

    /// Persist the images sent with a message. A user row is complete when
    /// it is inserted, so a cached context may already hold it: drop the
    /// session's cache.
    async fn set_message_images(
        &self,
        session_id: &str,
        message_id: i64,
        images: &str,
    ) -> Result<()> {
        sqlx::query("UPDATE messages SET images = ? WHERE id = ? AND session_id = ?")
            .bind(images)
            .bind(message_id)
            .bind(session_id)
            .execute_write(&self.pool)
            .await?;
        // A tool row is complete when inserted, so a cached context may
        // already hold it.
        self.context_cache.invalidate(session_id);
        Ok(())
    }

    async fn set_last_user_message_images(&self, session_id: &str, images: &str) -> Result<()> {
        sqlx::query(
            "UPDATE messages SET images = ? WHERE id = (
                SELECT MAX(id) FROM messages
                WHERE session_id = ? AND role = 'user' AND tool_call_id IS NULL)",
        )
        .bind(images)
        .bind(session_id)
        .execute_write(&self.pool)
        .await?;
        self.context_cache.invalidate(session_id);
        Ok(())
    }

    /// Persist the accumulated thinking/reasoning text for an assistant message.
    ///
    /// Called after `insert_message` so the INSERT signature stays lean —
    /// only assistant messages from Claude with extended thinking enabled will
    /// ever call this. All other callers of `insert_message` are unaffected.
    async fn update_message_thinking_content(
        &self,
        message_id: i64,
        model: &str,
        content: &str,
    ) -> Result<()> {
        sqlx::query("UPDATE messages SET thinking_content = ?, thinking_model = ? WHERE id = ?")
            .bind(content)
            .bind(model)
            .bind(message_id)
            .execute_write(&self.pool)
            .await?;
        // (#1166) Retroactive content mutation — bump gen so any cached
        // load_context snapshot containing this row's pre-update content
        // is forced to do a full reload.
        self.compaction_gen
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    /// Load active (non-compacted) messages for a session.
    ///
    /// Returns messages in chronological order. Compacted messages
    /// (archived by `/compact`) are excluded — their summary replaces them.
    /// Several sanitisation passes run before returning:
    /// - Incomplete assistant messages (interrupted/network-error) are excluded (#875, #877)
    /// - Mismatched tool_use / tool_result pairs are pruned (#428)
    /// - Null-content assistant messages with no tool calls are dropped (#594)
    /// - Whitespace-only assistant messages are dropped (#594)
    /// - Duplicate `Role::Tool` rows sharing a `tool_call_id` are deduped
    ///   to the latest write (#1159 — background sub-agent completion
    ///   updates the original `InvokeAgent` tool_result via append).
    async fn load_context(&self, session_id: &str) -> Result<Vec<Message>> {
        // (#1166 audit item A) Per-session delta-fetch cache.
        //
        // Fast path:
        //   - Snapshot cache + current compaction_gen.
        //   - If gen matches: SELECT only `id > max_id_returned` (delta).
        //     Append to cached messages, re-run sanitization, return.
        //   - Else: full reload (compaction is the only retroactive
        //     mutation against rows we may have cached).
        //
        // Cost vs. pre-cache:
        //   - Cache hit, k new rows: ~k SQL row deserializations + O(N)
        //     sanitization. SQL+deser was 93–96% of the cost at N=1000
        //     in the bench; this drops to O(k) for the SQL phase.
        //   - Cache miss / compaction: identical cost to today.
        //   - Concurrent callers: idempotent (last writer wins on the
        //     `store`); both compute the same merged result.
        let current_gen = self
            .compaction_gen
            .load(std::sync::atomic::Ordering::Acquire);
        let snapshot = self.context_cache.snapshot(session_id);

        let (mut messages, full_reload) = match &snapshot {
            Some(entry) if entry.compaction_gen_snapshot == current_gen => {
                // Cache hit + no compaction since: delta-fetch.
                let after_id = entry.max_id_returned.unwrap_or(0);
                let new_rows: Vec<Message> = sqlx::query_as::<_, MessageRow>(
                    "SELECT id, session_id, role, content, full_content, tool_calls, tool_call_id,
                            prompt_tokens, completion_tokens,
                            cache_read_tokens, cache_creation_tokens, thinking_tokens, thinking_content, thinking_model, thinking_blocks, images,
                            created_at
                     FROM messages
                     WHERE session_id = ? AND id > ? AND compacted_at IS NULL
                       AND (role != 'assistant' OR completed_at IS NOT NULL)
                     ORDER BY id ASC",
                )
                .bind(session_id)
                .bind(after_id)
                .fetch_all(&self.pool)
                .await?
                .into_iter()
                .map(|r| r.into())
                .collect();

                let mut merged = entry.messages.clone();
                merged.extend(new_rows);
                (merged, false)
            }
            _ => {
                // Cache miss or compaction-induced invalidation: full reload.
                let rows: Vec<Message> = sqlx::query_as::<_, MessageRow>(
                    "SELECT id, session_id, role, content, full_content, tool_calls, tool_call_id,
                            prompt_tokens, completion_tokens,
                            cache_read_tokens, cache_creation_tokens, thinking_tokens, thinking_content, thinking_model, thinking_blocks, images,
                            created_at
                     FROM messages
                     WHERE session_id = ? AND compacted_at IS NULL
                       AND (role != 'assistant' OR completed_at IS NOT NULL)
                     ORDER BY id ASC",
                )
                .bind(session_id)
                .fetch_all(&self.pool)
                .await?
                .into_iter()
                .map(|r| r.into())
                .collect();
                (rows, true)
            }
        };
        let _ = full_reload; // currently unused; kept for future tracing/metrics

        // Sanitisation passes: run in order, each sees the output of the previous.
        for edit in sanitize_history(&mut messages) {
            edit.log(session_id);
        }

        // Cache the freshly-sanitized snapshot.
        let max_id_returned = messages.iter().map(|m| m.id).max();
        self.context_cache.store(
            session_id,
            crate::db::ContextCacheEntry {
                messages: messages.clone(),
                max_id_returned,
                compaction_gen_snapshot: current_gen,
            },
        );

        Ok(messages)
    }

    /// Load ALL messages for a session (for RecallContext search).
    /// Returns messages in chronological order, no truncation.
    async fn load_all_messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let rows: Vec<Message> = sqlx::query_as::<_, MessageRow>(
            "SELECT id, session_id, role, content, full_content, tool_calls, tool_call_id,
                    prompt_tokens, completion_tokens,
                    cache_read_tokens, cache_creation_tokens, thinking_tokens, thinking_content, thinking_model, thinking_blocks, images,
                    created_at
             FROM messages
             WHERE session_id = ?
             ORDER BY id ASC",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|r| r.into())
        .collect();
        Ok(rows)
    }

    /// Get token usage totals for a session.
    async fn last_call_usage(&self, session_id: &str) -> Result<Option<SessionUsage>> {
        let row: Option<(i64, i64, i64, i64, i64)> = sqlx::query_as(
            "SELECT COALESCE(prompt_tokens, 0), COALESCE(completion_tokens, 0),
                    COALESCE(cache_read_tokens, 0), COALESCE(cache_creation_tokens, 0),
                    COALESCE(thinking_tokens, 0)
             FROM messages
             WHERE session_id = ?
               AND (prompt_tokens IS NOT NULL OR completion_tokens IS NOT NULL)
             ORDER BY id DESC LIMIT 1",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| SessionUsage {
            prompt_tokens: row.0,
            completion_tokens: row.1,
            cache_read_tokens: row.2,
            cache_creation_tokens: row.3,
            thinking_tokens: row.4,
            api_calls: 1,
        }))
    }

    async fn session_token_usage(&self, session_id: &str) -> Result<SessionUsage> {
        let row: (i64, i64, i64, i64, i64, i64) = sqlx::query_as(
            "SELECT
                COALESCE(SUM(prompt_tokens), 0),
                COALESCE(SUM(completion_tokens), 0),
                COALESCE(SUM(cache_read_tokens), 0),
                COALESCE(SUM(cache_creation_tokens), 0),
                COALESCE(SUM(thinking_tokens), 0),
                COUNT(*)
             FROM messages
             WHERE (session_id = ?
                    OR session_id IN (SELECT session_id FROM session_metadata
                                      WHERE key = ? AND value = ?))
               AND (prompt_tokens IS NOT NULL OR completion_tokens IS NOT NULL)",
        )
        .bind(session_id)
        .bind(crate::session_store::PARENT_SESSION_KEY)
        .bind(session_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(SessionUsage {
            prompt_tokens: row.0,
            completion_tokens: row.1,
            cache_read_tokens: row.2,
            cache_creation_tokens: row.3,
            thinking_tokens: row.4,
            api_calls: row.5,
        })
    }

    /// Get token usage broken down by agent name. A sub-agent's calls are
    /// listed under its name; the session's own are `main`.
    async fn session_usage_by_agent(
        &self,
        session_id: &str,
    ) -> Result<Vec<(String, SessionUsage)>> {
        let rows: Vec<(String, i64, i64, i64, i64, i64, i64)> = sqlx::query_as(
            "SELECT
                CASE WHEN m.session_id = ? THEN COALESCE(m.agent_name, 'main')
                     ELSE s.agent_name END AS agent,
                COALESCE(SUM(m.prompt_tokens), 0),
                COALESCE(SUM(m.completion_tokens), 0),
                COALESCE(SUM(m.cache_read_tokens), 0),
                COALESCE(SUM(m.cache_creation_tokens), 0),
                COALESCE(SUM(m.thinking_tokens), 0),
                COUNT(*)
             FROM messages m
             JOIN sessions s ON s.id = m.session_id
             WHERE (m.session_id = ?
                    OR m.session_id IN (SELECT session_id FROM session_metadata
                                        WHERE key = ? AND value = ?))
               AND (m.prompt_tokens IS NOT NULL OR m.completion_tokens IS NOT NULL)
             GROUP BY agent
             ORDER BY COALESCE(SUM(m.prompt_tokens), 0) + COALESCE(SUM(m.completion_tokens), 0) DESC",
        )
        .bind(session_id)
        .bind(session_id)
        .bind(crate::session_store::PARENT_SESSION_KEY)
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.0,
                    SessionUsage {
                        prompt_tokens: r.1,
                        completion_tokens: r.2,
                        cache_read_tokens: r.3,
                        cache_creation_tokens: r.4,
                        thinking_tokens: r.5,
                        api_calls: r.6,
                    },
                )
            })
            .collect())
    }

    /// List recent sessions for a specific project, from `offset` on.
    async fn list_sessions_from(
        &self,
        limit: i64,
        offset: i64,
        project_root: &Path,
    ) -> Result<Vec<SessionInfo>> {
        let root = project_root.to_string_lossy().to_string();
        let rows: Vec<SessionInfoRow> = sqlx::query_as(concat!(
            session_info_select!(),
            " WHERE (s.project_root = ? OR s.project_root IS NULL)
               AND s.archived_at IS NULL
             ORDER BY s.created_at DESC, s.rowid DESC
             LIMIT ? OFFSET ?"
        ))
        .bind(&root)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.into()).collect())
    }

    async fn list_sessions_page(
        &self,
        query: &crate::session_store::SessionListQuery,
    ) -> Result<Vec<SessionInfo>> {
        self.sessions_page(query, None).await
    }

    async fn search_candidates_page(
        &self,
        query: &crate::session_store::SessionListQuery,
        term: &str,
    ) -> Result<Vec<SessionInfo>> {
        self.sessions_page(query, stored_text_patterns(term).as_ref())
            .await
    }

    async fn get_session(&self, session_id: &str) -> Result<Option<SessionInfo>> {
        let row: Option<SessionInfoRow> =
            sqlx::query_as(concat!(session_info_select!(), " WHERE s.id = ?"))
                .bind(session_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(Into::into))
    }

    async fn sessions_with_id_prefix(&self, prefix: &str, limit: i64) -> Result<Vec<SessionInfo>> {
        let rows: Vec<SessionInfoRow> = sqlx::query_as(concat!(
            session_info_select!(),
            " WHERE substr(s.id, 1, length(?)) = ? LIMIT ?"
        ))
        .bind(prefix)
        .bind(prefix)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Get the model's final answer since the last user message (for headless
    /// JSON output).
    ///
    /// Only a row with text and no tool calls counts: text next to a tool call
    /// is narration, not the answer (#1444). Empty when the model finished
    /// without one, so a reader sees that instead of an older row.
    async fn last_assistant_message(&self, session_id: &str) -> Result<String> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT content FROM messages
             WHERE session_id = ? AND role = 'assistant' AND tool_calls IS NULL
               AND TRIM(content, ' ' || char(9, 10, 13)) != ''
               AND id > COALESCE(
                   (SELECT MAX(id) FROM messages WHERE session_id = ? AND role = 'user'), 0)
             ORDER BY id DESC LIMIT 1",
        )
        .bind(session_id)
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.0).unwrap_or_default())
    }

    /// Delete a session and everything stored for it, and with it the
    /// sessions of the sub-agents it ran, and theirs: nothing lists them once
    /// their parent is gone. One transaction.
    async fn delete_session(&self, session_id: &str) -> Result<bool> {
        let mut tx = self.pool.begin_immediate().await?;

        let doomed: Vec<(String,)> = sqlx::query_as(
            "WITH RECURSIVE doomed(id) AS (
               SELECT ?
               UNION
               SELECT s.id FROM sessions s JOIN doomed d ON s.parent_session_id = d.id
             )
             SELECT id FROM doomed",
        )
        .bind(session_id)
        .fetch_all(&mut *tx)
        .await?;

        let mut existed = false;
        for (id,) in &doomed {
            // A session's rows in the other tables hold a foreign key to
            // it: all of them go first, or the last delete is refused.
            for delete in [
                "DELETE FROM messages WHERE session_id = ?",
                "DELETE FROM session_metadata WHERE session_id = ?",
                "DELETE FROM turn_records WHERE session_id = ?",
                "DELETE FROM session_goals WHERE session_id = ?",
                "DELETE FROM session_events WHERE session_id = ?",
            ] {
                sqlx::query(delete).bind(id).execute(&mut *tx).await?;
            }
            let result = sqlx::query("DELETE FROM sessions WHERE id = ?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            existed |= id == session_id && result.rows_affected() > 0;
        }

        tx.commit().await?;

        // Reclaim freed pages from the deletion.
        sqlx::query("PRAGMA incremental_vacuum")
            .execute_write(&self.pool)
            .await?;

        // (#1166) Drop any cached context for these sessions so a future
        // `load_context` call doesn't return stale data if the same
        // session_id were ever to be re-created (defensive; sessions
        // ids are UUIDs so collision is improbable).
        for (id,) in &doomed {
            self.context_cache.invalidate(id);
        }

        Ok(existed)
    }

    async fn archive_session(&self, session_id: &str) -> Result<Vec<String>> {
        self.set_archived(session_id, true).await
    }

    async fn unarchive_session(&self, session_id: &str) -> Result<Vec<String>> {
        self.set_archived(session_id, false).await
    }

    async fn set_session_confidential(
        &self,
        session_id: &str,
        confidential: bool,
    ) -> Result<Vec<String>> {
        let mut tx = self.pool.begin_immediate().await?;
        // The session and the sub-agents' sessions under it, as
        // `set_archived` finds them; the session itself first.
        let subtree: Vec<(String, bool)> = sqlx::query_as(
            "WITH RECURSIVE subtree(id, depth) AS (
               SELECT id, 0 FROM sessions WHERE id = ?
               UNION
               SELECT s.id, t.depth + 1 FROM sessions s
                 JOIN subtree t ON s.parent_session_id = t.id
                WHERE t.depth < 64
             )
             SELECT t.id, s.confidential
               FROM subtree t JOIN sessions s ON s.id = t.id
              GROUP BY t.id
              ORDER BY (t.id = ?) DESC, MIN(t.depth), s.rowid",
        )
        .bind(session_id)
        .bind(session_id)
        .fetch_all(&mut *tx)
        .await?;
        let mut changed = Vec::new();
        for (id, is) in subtree {
            if is == confidential {
                continue;
            }
            // A brief says what a confidential session is: it goes with
            // the mark, and so does a draft of one.
            sqlx::query(
                "UPDATE sessions
                    SET confidential = ?,
                        confidential_brief =
                            CASE WHEN ? THEN confidential_brief END,
                        confidential_brief_draft =
                            CASE WHEN ? THEN confidential_brief_draft END
                  WHERE id = ?",
            )
            .bind(confidential)
            .bind(confidential)
            .bind(confidential)
            .bind(&id)
            .execute(&mut *tx)
            .await?;
            changed.push(id);
        }
        tx.commit().await?;
        Ok(changed)
    }

    async fn set_confidential_brief(&self, session_id: &str, brief: Option<&str>) -> Result<bool> {
        let set = sqlx::query(
            "UPDATE sessions
                SET confidential_brief = ?, confidential_brief_draft = NULL
              WHERE id = ? AND confidential = 1",
        )
        .bind(brief)
        .bind(session_id)
        .execute(&self.pool)
        .await?;
        Ok(set.rows_affected() == 1)
    }

    async fn offer_confidential_brief_draft(&self, session_id: &str, draft: &str) -> Result<bool> {
        // One statement, so of two processes that drafted for the same
        // session one stores its draft and the other finds it there.
        let stored = sqlx::query(
            "UPDATE sessions
                SET confidential_brief_draft = ?
              WHERE id = ? AND confidential = 1
                AND confidential_brief IS NULL AND confidential_brief_draft IS NULL",
        )
        .bind(draft)
        .bind(session_id)
        .execute(&self.pool)
        .await?;
        Ok(stored.rows_affected() == 1)
    }

    async fn mark_confidential_with_forks(
        &self,
        session_id: &str,
    ) -> Result<crate::session_store::ConfidentialMarked> {
        let mut tx = self.pool.begin_immediate().await?;
        // The session, the sub-agents' sessions under it, the sessions
        // forked from any of those, and so on down; `fork` says a session
        // was reached as a fork. `UNION` and a depth limit, as above.
        let reach: Vec<(String, bool, bool, bool)> = sqlx::query_as(
            "WITH RECURSIVE reach(id, depth, fork) AS (
               SELECT id, 0, 0 FROM sessions WHERE id = ?
               UNION
               SELECT s.id, r.depth + 1, s.forked_from_id IS r.id FROM sessions s
                 JOIN reach r ON s.parent_session_id = r.id OR s.forked_from_id = r.id
                WHERE r.depth < 64
             )
             SELECT r.id, s.confidential, MAX(r.fork), s.ephemeral
               FROM reach r JOIN sessions s ON s.id = r.id
              GROUP BY r.id
              ORDER BY (r.id = ?) DESC, MIN(r.depth), s.rowid",
        )
        .bind(session_id)
        .bind(session_id)
        .fetch_all(&mut *tx)
        .await?;
        let mut marked = crate::session_store::ConfidentialMarked::default();
        for (id, is, fork, ephemeral) in reach {
            if is {
                continue;
            }
            sqlx::query("UPDATE sessions SET confidential = 1 WHERE id = ?")
                .bind(&id)
                .execute(&mut *tx)
                .await?;
            marked.forks += usize::from(fork && !ephemeral);
            marked.sessions.push(id);
        }
        tx.commit().await?;
        Ok(marked)
    }

    async fn confidential_sessions(&self) -> Result<Vec<SessionInfo>> {
        let rows: Vec<SessionInfoRow> = sqlx::query_as(concat!(
            session_info_select!(),
            " WHERE s.confidential = 1
              ORDER BY COALESCE(s.last_accessed_at, s.created_at) DESC, s.rowid DESC"
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    async fn set_session_title(&self, session_id: &str, title: &str) -> Result<()> {
        sqlx::query("UPDATE sessions SET title = ? WHERE id = ?")
            .bind(title)
            .bind(session_id)
            .execute_write(&self.pool)
            .await?;
        Ok(())
    }

    async fn set_session_mode(&self, session_id: &str, mode: &str) -> Result<()> {
        sqlx::query("UPDATE sessions SET mode = ? WHERE id = ?")
            .bind(mode)
            .bind(session_id)
            .execute_write(&self.pool)
            .await?;
        Ok(())
    }

    async fn set_session_model(
        &self,
        session_id: &str,
        provider: &str,
        base_url: &str,
        model: &str,
    ) -> Result<()> {
        sqlx::query("UPDATE sessions SET provider = ?, base_url = ?, model = ? WHERE id = ?")
            .bind(provider)
            .bind(base_url)
            .bind(model)
            .bind(session_id)
            .execute_write(&self.pool)
            .await?;
        Ok(())
    }

    async fn get_session_mode(&self, session_id: &str) -> Result<Option<String>> {
        let row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT mode FROM sessions WHERE id = ?")
                .bind(session_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|r| r.0))
    }

    /// Compact a session: summarize old messages while preserving the most recent ones.
    ///
    /// Keeps the last `preserve_count` messages intact, archives the rest, and
    /// inserts `summary` as a `system` row. That row is engine bookkeeping, so
    /// it stays out of the user's history and of interruption detection;
    /// the engine's `assemble_messages` sends
    /// it to the model as a user message ahead of the preserved tail.
    ///
    /// Returns the number of messages that were deleted/replaced.
    async fn compact_session(
        &self,
        session_id: &str,
        summary: &str,
        preserve_count: usize,
    ) -> Result<usize> {
        self.archive_into_summary(session_id, summary, KeepRows::Last(preserve_count), None)
            .await
    }

    async fn compact_session_before(
        &self,
        session_id: &str,
        summary: &str,
        keep_from_id: i64,
        usage: Option<&crate::session_store::MessageUsage>,
    ) -> Result<usize> {
        self.archive_into_summary(session_id, summary, KeepRows::From(keep_from_id), usage)
            .await
    }

    /// Check if the last message in a session is a tool call awaiting a response.
    /// Used to defer compaction during active tool execution.
    async fn has_pending_tool_calls(&self, session_id: &str) -> Result<bool> {
        let last_msg: Option<(String, Option<String>)> = sqlx::query_as(
            "SELECT role, tool_calls FROM messages
             WHERE session_id = ? AND compacted_at IS NULL
             ORDER BY id DESC LIMIT 1",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(matches!(last_msg, Some((role, Some(_))) if role == "assistant"))
    }

    /// Get a session metadata value by key.
    async fn get_metadata(&self, session_id: &str, key: &str) -> Result<Option<String>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT value FROM session_metadata WHERE session_id = ? AND key = ?")
                .bind(session_id)
                .bind(key)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|r| r.0))
    }

    async fn child_session_ids(&self, parent: &str) -> Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT m.session_id FROM session_metadata m \
             JOIN sessions s ON s.id = m.session_id \
             WHERE m.key = ? AND m.value = ? ORDER BY s.created_at, s.id",
        )
        .bind(crate::session_store::PARENT_SESSION_KEY)
        .bind(parent)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.0).collect())
    }

    async fn export_session(&self, session_id: &str) -> Result<serde_json::Value> {
        // The tables that hold a session's own rows, by the column naming it.
        const TABLES: [(&str, &str); 5] = [
            ("sessions", "id"),
            ("session_metadata", "session_id"),
            ("messages", "session_id"),
            ("turn_records", "session_id"),
            ("session_events", "session_id"),
        ];
        let mut export = serde_json::Map::new();
        for (table, column) in TABLES {
            // Safe: the table and column names are the constants above.
            let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
                "SELECT * FROM {table} WHERE {column} = ? ORDER BY rowid"
            )))
            .bind(session_id)
            .fetch_all(&self.pool)
            .await?;
            export.insert(
                table.to_string(),
                serde_json::Value::Array(rows.iter().map(row_to_json).collect()),
            );
        }
        Ok(serde_json::Value::Object(export))
    }

    async fn append_turn_records(
        &self,
        session_id: &str,
        records: &[crate::session_store::TurnRecord],
    ) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin_immediate().await?;
        for record in records {
            sqlx::query(
                "INSERT INTO turn_records (session_id, turn_id, kind, payload) VALUES (?, ?, ?, ?)",
            )
            .bind(session_id)
            .bind(&record.turn_id)
            .bind(&record.kind)
            .bind(&record.payload)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn message_boundary(
        &self,
        session_id: &str,
    ) -> Result<crate::session_store::MessageBoundary> {
        let (last_message_id, compacted): (i64, i64) = sqlx::query_as(
            "SELECT COALESCE(MAX(id), 0), COUNT(compacted_at) FROM messages WHERE session_id = ?",
        )
        .bind(session_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(crate::session_store::MessageBoundary {
            last_message_id,
            compacted,
        })
    }

    async fn rewind(
        &self,
        session_id: &str,
        turn_id: &str,
        boundary: crate::session_store::MessageBoundary,
    ) -> Result<()> {
        let mut tx = self.pool.begin_immediate().await?;
        let (compacted,): (i64,) = sqlx::query_as(
            "SELECT COUNT(compacted_at) FROM messages WHERE session_id = ? AND id <= ?",
        )
        .bind(session_id)
        .bind(boundary.last_message_id)
        .fetch_one(&mut *tx)
        .await?;
        anyhow::ensure!(
            compacted == boundary.compacted,
            "the history was compacted after that turn, so it can't be rewound to before it"
        );
        let first: Option<(i64,)> = sqlx::query_as(
            "SELECT MIN(id) FROM turn_records WHERE session_id = ? AND turn_id = ?              HAVING MIN(id) IS NOT NULL",
        )
        .bind(session_id)
        .bind(turn_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((first,)) = first else {
            anyhow::bail!("no turn {turn_id} in session {session_id}");
        };
        sqlx::query("DELETE FROM messages WHERE session_id = ? AND id > ?")
            .bind(session_id)
            .bind(boundary.last_message_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM turn_records WHERE session_id = ? AND id >= ?")
            .bind(session_id)
            .bind(first)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        // Rows the cache returned are gone: it assumes an append-only log.
        self.context_cache.invalidate(session_id);
        Ok(())
    }

    async fn load_turn_records(
        &self,
        session_id: &str,
    ) -> Result<Vec<crate::session_store::TurnRecord>> {
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT turn_id, kind, payload FROM turn_records WHERE session_id = ? ORDER BY id ASC",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(turn_id, kind, payload)| crate::session_store::TurnRecord {
                    turn_id,
                    kind,
                    payload,
                },
            )
            .collect())
    }

    /// Set a session metadata value (upsert).
    async fn set_metadata(&self, session_id: &str, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO session_metadata (session_id, key, value, updated_at)
             VALUES (?, ?, ?, CURRENT_TIMESTAMP)
             ON CONFLICT(session_id, key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        )
        .bind(session_id)
        .bind(key)
        .bind(value)
        .execute_write(&self.pool)
        .await?;
        Ok(())
    }

    /// Get the todo list for a session (convenience wrapper).
    async fn get_todo(&self, session_id: &str) -> Result<Option<String>> {
        self.get_metadata(session_id, "todo").await
    }

    /// Set the todo list for a session (convenience wrapper).
    async fn set_todo(&self, session_id: &str, content: &str) -> Result<()> {
        self.set_metadata(session_id, "todo", content).await
    }

    // ── Session events (#1108 P1b/P2a) ─────────────────────────────

    async fn insert_session_event(
        &self,
        session_id: &str,
        kind: &str,
        payload: &str,
        parent_tool_call_id: Option<&str>,
    ) -> Result<i64> {
        let result = sqlx::query(
            "INSERT INTO session_events (session_id, kind, payload, parent_tool_call_id) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(session_id)
        .bind(kind)
        .bind(payload)
        .bind(parent_tool_call_id)
        .execute_write(&self.pool)
        .await?;
        Ok(result.last_insert_rowid())
    }

    async fn load_session_events(&self, session_id: &str) -> Result<Vec<SessionEvent>> {
        // Order by id (auto-increment) gives append order, which is
        // semantically what we want — created_at has 1-second
        // resolution and a busy session emits dozens of events per
        // second. Id is monotonic and gap-free per insert.
        let rows = sqlx::query_as::<_, SessionEventRow>(
            "SELECT id, session_id, kind, payload, parent_tool_call_id, created_at \
             FROM session_events WHERE session_id = ? ORDER BY id ASC",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(SessionEvent::from).collect())
    }
}

/// One row as JSON, every column by name, each value as SQLite stored it:
/// integers and reals as numbers, text as strings, a blob by its length.
fn row_to_json(row: &sqlx::sqlite::SqliteRow) -> serde_json::Value {
    use sqlx::{Column, Row, TypeInfo, ValueRef};
    let mut object = serde_json::Map::new();
    for column in row.columns() {
        let index = column.ordinal();
        let value = match row.try_get_raw(index) {
            Ok(raw) if raw.is_null() => serde_json::Value::Null,
            Ok(raw) => match raw.type_info().name() {
                "INTEGER" => row
                    .try_get::<i64, _>(index)
                    .map(serde_json::Value::from)
                    .unwrap_or_default(),
                "REAL" => row
                    .try_get::<f64, _>(index)
                    .map(serde_json::Value::from)
                    .unwrap_or_default(),
                "BLOB" => row
                    .try_get::<Vec<u8>, _>(index)
                    .map(|bytes| serde_json::Value::from(format!("<{} bytes>", bytes.len())))
                    .unwrap_or_default(),
                _ => row
                    .try_get::<String, _>(index)
                    .map(serde_json::Value::from)
                    .unwrap_or_default(),
            },
            Err(_) => serde_json::Value::Null,
        };
        object.insert(column.name().to_string(), value);
    }
    serde_json::Value::Object(object)
}
