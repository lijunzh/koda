//! TodoWrite tool — session-scoped task list.
//!
//! The model maintains the full todo list by rewriting it on every call.
//! Items are persisted to session metadata (survives compaction) and injected
//! into the system prompt each turn so the model always has its plan in view.
//!
//! ## Schema (matches Claude Code's TodoWrite)
//!
//! Each item has:
//! - `content`  — what to do (non-empty string)
//! - `status`   — `"pending"` | `"in_progress"` | `"completed"`
//! - `priority` — `"high"` | `"medium"` | `"low"`

use crate::db::Database;
use crate::persistence::Persistence as _;
use crate::providers::ToolDefinition;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

// ── Schema ─────────────────────────────────────────────────────────────────

/// Completion state of a todo item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    /// Not started.
    Pending,
    /// Currently being worked on (at most one task should be in this state).
    InProgress,
    /// Finished.
    Completed,
}

impl TodoStatus {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "in_progress" => Some(Self::InProgress),
            "completed" => Some(Self::Completed),
            _ => None,
        }
    }

    /// Checkbox-style marker — universally understood.
    fn checkbox(&self) -> &'static str {
        match self {
            Self::Pending => "[ ]",
            Self::InProgress => "[→]",
            Self::Completed => "[x]",
        }
    }
}

/// Relative importance of a todo item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoPriority {
    /// Must be done first.
    High,
    /// Normal importance.
    Medium,
    /// Nice-to-have.
    Low,
}

impl TodoPriority {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "high" => Some(Self::High),
            "medium" => Some(Self::Medium),
            "low" => Some(Self::Low),
            _ => None,
        }
    }

    /// Compact suffix shown after the task content (only for high priority).
    fn suffix(&self) -> &'static str {
        match self {
            Self::High => " ⚡",
            Self::Medium | Self::Low => "",
        }
    }
}

/// A single task in the session todo list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TodoItem {
    /// Human-readable task description.
    pub content: String,
    /// Current completion state.
    pub status: TodoStatus,
    /// Relative importance.
    pub priority: TodoPriority,
}

// ── Diff types ───────────────────────────────────────────

/// Before/after pair for a todo whose `status` and/or `priority` changed
/// while keeping the same `content` string.
///
/// Computed server-side by [`todo_write`] so every client (TUI / ACP /
/// headless / future) gets the same animation primitives without
/// having to maintain its own previous-list snapshot. Surfaces on
/// [`crate::engine::EngineEvent::TodoUpdate`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TodoChange {
    /// State on the previously persisted list.
    pub before: TodoItem,
    /// State on the newly written list.
    pub after: TodoItem,
}

/// Server-computed delta between the previously persisted todo list
/// and the one the model just wrote.
///
/// **Matching key is `content`.** If the model renames a task the
/// rename surfaces as one entry in `removed` plus one in `added`,
/// which is the right semantic — a renamed task is conceptually a
/// different task to the user even if the underlying intent is the
/// same.
///
/// On the very first `TodoWrite` of a session, every item lands in
/// `added`. On a clear (`todos: []`), every previously persisted
/// item lands in `removed`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TodoDiff {
    /// Items present on the new list whose `content` is not on the old list.
    pub added: Vec<TodoItem>,
    /// Items present on the old list whose `content` is not on the new list.
    pub removed: Vec<TodoItem>,
    /// Items present on both lists by `content` whose `status` or
    /// `priority` changed.
    pub changed: Vec<TodoChange>,
}

impl TodoDiff {
    /// `true` when there are no additions, removals, or changes.
    /// Used to suppress the `TodoUpdate` event on no-op writes — the
    /// dedup-nudge path returns the "unchanged" message to the model
    /// without surfacing a transition to clients.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }

    /// Compute the diff between an old list and a new list.
    ///
    /// O(n*m) but n and m are bounded by typical todo-list size (low
    /// dozens at the absolute outside) so a HashMap would be more
    /// code than savings.
    fn compute(old: &[TodoItem], new: &[TodoItem]) -> Self {
        let mut added = Vec::new();
        let mut removed = Vec::new();
        let mut changed = Vec::new();

        // Pass 1: walk new list. Each item is either added, changed, or
        // unchanged-equal-to-old.
        for n in new {
            match old.iter().find(|o| o.content == n.content) {
                None => added.push(n.clone()),
                Some(o) if o != n => changed.push(TodoChange {
                    before: o.clone(),
                    after: n.clone(),
                }),
                Some(_) => { /* identical — no diff entry */ }
            }
        }

        // Pass 2: walk old list. Anything whose content is missing from
        // new is a removal.
        for o in old {
            if !new.iter().any(|n| n.content == o.content) {
                removed.push(o.clone());
            }
        }

        Self {
            added,
            removed,
            changed,
        }
    }
}

// ── Outcome ───────────────────────────────────────────────

/// What [`todo_write`] returns to the dispatch layer.
///
/// The dispatch layer:
/// 1. forwards `message` to the model as the tool result string;
/// 2. when `diff.is_empty()` is `false`, emits
///    [`crate::engine::EngineEvent::TodoUpdate`] with `items` and
///    `diff` so every client sees the transition.
///
/// Splitting `message` (model-facing) from `items + diff` (client-
/// facing) is the same separation Claude Code's `TodoWriteTool` uses
/// (`mapToolResultToToolResultBlockParam` returns a plain string
/// while the structured diff goes to the UI). It keeps the model's
/// tool-result clean and the UI's render data rich.
#[derive(Debug, Clone)]
pub struct TodoWriteOutcome {
    /// String returned to the model as the tool result.
    pub message: String,
    /// Full new list, after dedup short-circuit but always populated
    /// (even on the unchanged path) so callers that want to mirror
    /// the latest list don't have to re-read from the DB.
    pub items: Vec<TodoItem>,
    /// Server-computed diff against the previously persisted list.
    /// `is_empty()` on the unchanged path; `added` is non-empty on
    /// the first write of a session.
    pub diff: TodoDiff,
}

// ── Tool definition ─────────────────────────────────────────────────────────

/// Return the tool definition for the LLM.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "TodoWrite".to_string(),
        description: "Create and manage a structured task list for the current session. \
            Rewrite the full list on every call — include all tasks, not just changed ones. \
            Use proactively for: multi-step tasks (3+ steps), complex refactors, or when \
            the user provides a list of things to do. Mark tasks `in_progress` BEFORE \
            starting and `completed` immediately after finishing. Only one task should be \
            `in_progress` at a time."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "The complete todo list (replaces any previous list)",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {
                                "type": "string",
                                "description": "Actionable task description in imperative form"
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"],
                                "description": "Current status of the task"
                            },
                            "priority": {
                                "type": "string",
                                "enum": ["high", "medium", "low"],
                                "description": "Task priority"
                            }
                        },
                        "required": ["content", "status", "priority"]
                    }
                }
            },
            "required": ["todos"]
        }),
    }]
}

// ── Handler ───────────────────────────────────────────────

/// Write the full todo list for this session.
///
/// Returns a [`TodoWriteOutcome`] with both the model-facing message
/// and structured `items + diff` for the dispatch layer to surface
/// via [`crate::engine::EngineEvent::TodoUpdate`].
///
/// **Validation** (rejected before any DB write):
/// - `todos` must be an array.
/// - Each item needs a non-empty `content`, a valid `status`, and a
///   valid `priority`.
/// - At most one item may have `status == InProgress`. Stolen from
///   Gemini CLI (`packages/core/src/tools/write-todos.ts`); the
///   only one of the four reference projects that enforces it
///   server-side instead of via prompt discipline. Small,
///   deterministic, removes one class of model failure mode.
///
/// **Content-aware dedup**: if the parsed list is byte-equal to
/// what's already stored, we skip the write and return a short
/// "unchanged" message. The returned `diff` is empty and the
/// dispatch layer suppresses the `TodoUpdate` event — this prevents
/// the model from burning tool calls (and triggering loop detection)
/// by re-emitting the same plan, while also not spamming clients
/// with no-op transitions.
pub async fn todo_write(db: &Database, session_id: &str, args: &Value) -> Result<TodoWriteOutcome> {
    let raw = args
        .get("todos")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("Missing 'todos' array"))?;

    let mut todos: Vec<TodoItem> = Vec::with_capacity(raw.len());
    for (i, item) in raw.iter().enumerate() {
        let content = item
            .get("content")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("todos[{i}]: 'content' must be a non-empty string"))?
            .to_string();

        let status_str = item
            .get("status")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("todos[{i}]: missing 'status'"))?;
        let status = TodoStatus::from_str(status_str).ok_or_else(|| {
            anyhow::anyhow!(
                "todos[{i}]: invalid status '{status_str}' — use pending/in_progress/completed"
            )
        })?;

        let priority_str = item
            .get("priority")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("todos[{i}]: missing 'priority'"))?;
        let priority = TodoPriority::from_str(priority_str).ok_or_else(|| {
            anyhow::anyhow!("todos[{i}]: invalid priority '{priority_str}' — use high/medium/low")
        })?;

        todos.push(TodoItem {
            content,
            status,
            priority,
        });
    }

    // ── Single-in-progress invariant (#1077 Phase A) ────────────
    // Reject before reading the previous list — this is a structural
    // input error, not a state-dependent one.
    let in_progress = todos
        .iter()
        .filter(|t| t.status == TodoStatus::InProgress)
        .count();
    if in_progress > 1 {
        anyhow::bail!(
            "Invalid todo list: {in_progress} tasks marked 'in_progress'. \
             Only one task may be 'in_progress' at a time — mark all but one as \
             'pending' or 'completed' and call TodoWrite again."
        );
    }

    // ── Load the previous list once (for both dedup and diff) ─────
    let old: Vec<TodoItem> = match db.get_todo(session_id).await {
        Ok(Some(raw)) => serde_json::from_str(&raw).unwrap_or_default(),
        _ => Vec::new(),
    };

    // ── Content-aware dedup ─────────────────────────────
    // Byte-equal previous list short-circuits the write AND the event
    // emission. `TodoDiff::default()` (empty) signals "no transition".
    if old == todos {
        return Ok(TodoWriteOutcome {
            message: format!(
                "Todo list unchanged ({} task{}). \
                 Do not call TodoWrite again unless you are changing a task's status or content.",
                todos.len(),
                if todos.len() == 1 { "" } else { "s" }
            ),
            items: todos,
            diff: TodoDiff::default(),
        });
    }

    let diff = TodoDiff::compute(&old, &todos);

    let json = serde_json::to_string(&todos)?;
    db.set_todo(session_id, &json).await?;

    Ok(TodoWriteOutcome {
        message: format_todo_list(&todos),
        items: todos,
        diff,
    })
}

// ── Formatting ──────────────────────────────────────────────────────────────

/// Format a single todo item: `[x] Task description`
fn format_item(t: &TodoItem) -> String {
    format!("{} {}", t.status.checkbox(), t.content)
}

fn format_todo_list(todos: &[TodoItem]) -> String {
    if todos.is_empty() {
        return "Todo list cleared.".to_string();
    }

    let completed = todos
        .iter()
        .filter(|t| t.status == TodoStatus::Completed)
        .count();

    let mut out = format!("Todo list updated ({}/{} done):\n", completed, todos.len(),);
    for t in todos {
        out.push_str(&format!("  {}{}\n", format_item(t), t.priority.suffix()));
    }
    out
}

// =============================================================
// Tool trait implementation (#1265 item 5, PR-7/N).
//
// `TodoWrite` is `ReadOnly` from a user-impact perspective: it
// only mutates Koda's own session-state todo list, not the user's
// files or shell. Gating it behind approval breaks Plan-mode
// planning entirely (#1212). Same classification as pre-#1265.
//
// Special wrinkle: on success, `TodoWrite` emits an
// `EngineEvent::TodoUpdate` to the streaming sink so all clients
// (TUI / ACP / headless) see the transition (#1077 Phase A). We
// fold that emit *into* the trait's `execute`, so the registry's
// dispatch fast path doesn't need a `name == "TodoWrite"` arm —
// the trait owns its own side-effect plumbing.
//
// Empty-diff writes (the dedup-nudge path) suppress the event by
// design — unchanged-list writes are a no-op for clients, only a
// reminder for the model.
// =============================================================

use crate::tools::{Tool, ToolEffect, ToolExecCtx, ToolResult};
use async_trait::async_trait;

/// `TodoWrite` — update the session-state todo list.
pub struct TodoWriteTool;

#[async_trait]
impl Tool for TodoWriteTool {
    fn name(&self) -> &'static str {
        "TodoWrite"
    }
    fn definition(&self) -> ToolDefinition {
        definitions()
            .into_iter()
            .find(|d| d.name == "TodoWrite")
            .expect("todo::definitions() must contain TodoWrite")
    }
    fn classify(&self, _args: &serde_json::Value) -> ToolEffect {
        // ReadOnly because it only mutates Koda's own state.
        ToolEffect::ReadOnly
    }
    async fn execute(&self, ctx: &ToolExecCtx<'_>, args: &serde_json::Value) -> ToolResult {
        let Some((db, sid)) = ctx.session else {
            // Pre-#1265 dispatch returned `Ok("requires an active session.")`
            // — a successful tool result with a self-explanatory message.
            // Behavior preserved exactly.
            return ToolResult {
                output: "TodoWrite requires an active session.".to_string(),
                success: true,
                full_output: None,
            };
        };
        match todo_write(db, sid, args).await {
            Ok(outcome) => {
                // #1077 Phase A: surface the transition unless the
                // diff is empty (dedup-nudge path).
                if !outcome.diff.is_empty()
                    && let Some((sink, _call_id)) = ctx.sink
                {
                    sink.emit(crate::engine::EngineEvent::TodoUpdate {
                        items: outcome.items.clone(),
                        diff: outcome.diff.clone(),
                    });
                }
                ToolResult {
                    output: outcome.message,
                    success: true,
                    full_output: None,
                }
            }
            Err(e) => ToolResult {
                output: format!("Error: {e:#}"),
                success: false,
                full_output: None,
            },
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    // ── Trait invariants (#1265 PR-7) ─────────────────────────

    #[test]
    fn todo_tool_metadata() {
        let t = TodoWriteTool;
        assert_eq!(t.name(), "TodoWrite");
        assert_eq!(t.definition().name, "TodoWrite");
        // ReadOnly: only mutates Koda's own state (#1212).
        assert_eq!(t.classify(&json!({})), crate::tools::ToolEffect::ReadOnly,);
        assert!(t.extract_undo_path(&json!({})).is_none());
    }

    /// No-session path returns a graceful Ok message (preserves
    /// pre-#1265 behavior exactly).
    #[tokio::test]
    async fn todo_no_session_returns_graceful_message() {
        let t = TodoWriteTool;
        let tmp = TempDir::new().unwrap();
        let cache = crate::tools::FileReadCache::default();
        let fs = koda_sandbox::fs::LocalFileSystem;
        let caps = crate::output_caps::OutputCaps::for_context(100_000);
        let bg = crate::tools::bg_process::BgRegistry::new();
        let trust = crate::trust::TrustMode::Safe;
        let policy = koda_sandbox::SandboxPolicy::default();
        let skills = crate::skills::SkillRegistry::default();
        let ctx = crate::tools::ToolExecCtx::for_test(
            tmp.path(),
            &cache,
            &fs,
            &caps,
            &bg,
            &trust,
            &policy,
            &skills,
        );
        let r = t.execute(&ctx, &json!({"todos": []})).await;
        assert!(r.success);
        assert_eq!(r.output, "TodoWrite requires an active session.");
    }

    async fn test_db() -> (Database, TempDir, String) {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("test.db")).await.unwrap();
        use crate::persistence::Persistence;
        let sid = db.create_session("koda", dir.path()).await.unwrap();
        (db, dir, sid)
    }

    #[tokio::test]
    async fn write_and_read_back() {
        let (db, _dir, sid) = test_db().await;
        let args = json!({
            "todos": [
                {"content": "Add tests", "status": "pending", "priority": "high"},
                {"content": "Write docs", "status": "in_progress", "priority": "medium"},
            ]
        });
        let out = todo_write(&db, &sid, &args).await.unwrap();
        assert!(out.message.contains("0/2 done"));
        assert!(out.message.contains("[ ] Add tests"));
        assert!(out.message.contains("[→] Write docs"));

        // (#1077 Phase B) Persistence verified through the public DB
        // accessor instead of the deleted `get_todo_section` helper.
        // Clients now mirror state from `EngineEvent::TodoUpdate`;
        // the DB row stays as the source-of-truth for ACP
        // reconnects but is no longer read into the system prompt.
        use crate::persistence::Persistence;
        let raw = db.get_todo(&sid).await.unwrap().expect("row persisted");
        assert!(raw.contains("Add tests"));
        assert!(raw.contains("Write docs"));
    }

    #[tokio::test]
    async fn empty_list_clears_todos() {
        let (db, _dir, sid) = test_db().await;
        // First write something
        let args = json!({ "todos": [
            {"content": "Task", "status": "pending", "priority": "low"}
        ]});
        todo_write(&db, &sid, &args).await.unwrap();
        // Then clear it
        let clear = json!({ "todos": [] });
        let out = todo_write(&db, &sid, &clear).await.unwrap();
        assert!(out.message.contains("cleared"));
        // Persisted row is the empty list (not deleted) — same
        // observable behaviour as before, just verified directly.
        use crate::persistence::Persistence;
        let raw = db.get_todo(&sid).await.unwrap().expect("row persisted");
        assert_eq!(raw, "[]");
    }

    #[tokio::test]
    async fn invalid_status_returns_error() {
        let (db, _dir, sid) = test_db().await;
        let args = json!({
            "todos": [{"content": "Task", "status": "doing", "priority": "high"}]
        });
        let err = todo_write(&db, &sid, &args).await.unwrap_err();
        assert!(err.to_string().contains("invalid status"));
    }

    #[tokio::test]
    async fn invalid_priority_returns_error() {
        let (db, _dir, sid) = test_db().await;
        let args = json!({
            "todos": [{"content": "Task", "status": "pending", "priority": "urgent"}]
        });
        let err = todo_write(&db, &sid, &args).await.unwrap_err();
        assert!(err.to_string().contains("invalid priority"));
    }

    #[tokio::test]
    async fn empty_content_returns_error() {
        let (db, _dir, sid) = test_db().await;
        let args = json!({
            "todos": [{"content": "  ", "status": "pending", "priority": "low"}]
        });
        let err = todo_write(&db, &sid, &args).await.unwrap_err();
        assert!(err.to_string().contains("non-empty"));
    }

    #[tokio::test]
    async fn missing_todos_field_returns_error() {
        let (db, _dir, sid) = test_db().await;
        let err = todo_write(&db, &sid, &json!({})).await.unwrap_err();
        assert!(err.to_string().contains("todos"));
    }

    #[test]
    fn format_single_task() {
        let todos = vec![TodoItem {
            content: "Ship it".into(),
            status: TodoStatus::InProgress,
            priority: TodoPriority::High,
        }];
        let out = format_todo_list(&todos);
        assert!(out.contains("0/1 done"));
        assert!(out.contains("[→] Ship it"));
        // High priority gets a suffix
        assert!(out.contains("⚡"));
    }

    #[test]
    fn format_completed_task() {
        let todos = vec![
            TodoItem {
                content: "Done thing".into(),
                status: TodoStatus::Completed,
                priority: TodoPriority::Medium,
            },
            TodoItem {
                content: "Todo thing".into(),
                status: TodoStatus::Pending,
                priority: TodoPriority::Low,
            },
        ];
        let out = format_todo_list(&todos);
        assert!(out.contains("1/2 done"));
        assert!(out.contains("[x] Done thing"));
        assert!(out.contains("[ ] Todo thing"));
        // Medium/Low priority: no suffix
        assert!(!out.contains("⚡") || !out.contains("Done thing ⚡"));
    }

    #[test]
    fn status_checkbox_coverage() {
        assert_eq!(TodoStatus::Pending.checkbox(), "[ ]");
        assert_eq!(TodoStatus::InProgress.checkbox(), "[→]");
        assert_eq!(TodoStatus::Completed.checkbox(), "[x]");
    }

    #[tokio::test]
    async fn dedup_skips_identical_write() {
        let (db, _dir, sid) = test_db().await;
        let args = json!({
            "todos": [
                {"content": "Task A", "status": "pending", "priority": "high"},
                {"content": "Task B", "status": "in_progress", "priority": "medium"},
            ]
        });
        // First write — should persist and return full list
        let out1 = todo_write(&db, &sid, &args).await.unwrap();
        assert!(out1.message.contains("0/2 done"));

        // Second write with identical content — should short-circuit
        let out2 = todo_write(&db, &sid, &args).await.unwrap();
        assert!(
            out2.message.contains("unchanged"),
            "identical call should return 'unchanged', got: {}",
            out2.message
        );
        assert!(
            out2.message.contains("Do not call TodoWrite again"),
            "should tell model to stop calling"
        );
        assert!(
            out2.diff.is_empty(),
            "unchanged write must yield an empty diff so the dispatch \
             layer suppresses the TodoUpdate event"
        );
    }

    #[tokio::test]
    async fn dedup_allows_status_change() {
        let (db, _dir, sid) = test_db().await;
        let args1 = json!({
            "todos": [
                {"content": "Task A", "status": "pending", "priority": "high"},
            ]
        });
        todo_write(&db, &sid, &args1).await.unwrap();

        // Same content but status changed — should NOT short-circuit
        let args2 = json!({
            "todos": [
                {"content": "Task A", "status": "completed", "priority": "high"},
            ]
        });
        let out = todo_write(&db, &sid, &args2).await.unwrap();
        assert!(
            out.message.contains("1/1 done"),
            "status change should write normally, got: {}",
            out.message
        );
        assert!(out.message.contains("[x] Task A"));
    }

    // ── #1077 Phase A: validation ───────────────────────────

    /// Two `in_progress` items must be rejected up front. Stolen
    /// from Gemini CLI; the only one of the four reference projects
    /// that enforces single-in-progress server-side. Without this,
    /// the model can silently keep two tasks active and clients
    /// render a contradictory checklist.
    #[tokio::test]
    async fn rejects_two_in_progress_items() {
        let (db, _dir, sid) = test_db().await;
        let args = json!({
            "todos": [
                {"content": "A", "status": "in_progress", "priority": "high"},
                {"content": "B", "status": "in_progress", "priority": "medium"},
            ]
        });
        let err = todo_write(&db, &sid, &args).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Only one task"), "got: {msg}");
        assert!(msg.contains("in_progress"), "got: {msg}");
        // Must reject BEFORE writing — the DB should still be empty.
        use crate::persistence::Persistence;
        assert!(
            db.get_todo(&sid).await.unwrap().is_none(),
            "failed validation must not touch the DB"
        );
    }

    /// Single `in_progress` is the happy path; many `pending`
    /// alongside is fine.
    #[tokio::test]
    async fn accepts_single_in_progress() {
        let (db, _dir, sid) = test_db().await;
        let args = json!({
            "todos": [
                {"content": "A", "status": "in_progress", "priority": "high"},
                {"content": "B", "status": "pending", "priority": "medium"},
                {"content": "C", "status": "pending", "priority": "low"},
            ]
        });
        let out = todo_write(&db, &sid, &args).await.unwrap();
        assert!(out.message.contains("0/3 done"));
    }

    /// Zero `in_progress` (all pending or all completed) is
    /// permitted — the rule is at-most-one, not exactly-one.
    #[tokio::test]
    async fn accepts_zero_in_progress() {
        let (db, _dir, sid) = test_db().await;
        let args = json!({
            "todos": [
                {"content": "A", "status": "pending", "priority": "high"},
                {"content": "B", "status": "pending", "priority": "medium"},
            ]
        });
        let out = todo_write(&db, &sid, &args).await.unwrap();
        assert!(out.message.contains("0/2 done"));
    }

    // ── #1077 Phase A: diff computation ───────────────────────

    fn item(content: &str, status: TodoStatus, priority: TodoPriority) -> TodoItem {
        TodoItem {
            content: content.into(),
            status,
            priority,
        }
    }

    #[test]
    fn diff_first_write_lands_everything_in_added() {
        // First write of a session: previous list is empty so every
        // item must show up in `added` — nothing in `changed` or
        // `removed`. This is what enables clients to do a one-shot
        // "populate from empty" render on session start.
        let new = vec![
            item("A", TodoStatus::Pending, TodoPriority::High),
            item("B", TodoStatus::InProgress, TodoPriority::Medium),
        ];
        let diff = TodoDiff::compute(&[], &new);
        assert_eq!(diff.added.len(), 2);
        assert!(diff.changed.is_empty());
        assert!(diff.removed.is_empty());
        assert!(!diff.is_empty());
    }

    #[test]
    fn diff_clear_lands_everything_in_removed() {
        let old = vec![
            item("A", TodoStatus::Pending, TodoPriority::High),
            item("B", TodoStatus::Completed, TodoPriority::Medium),
        ];
        let diff = TodoDiff::compute(&old, &[]);
        assert!(diff.added.is_empty());
        assert!(diff.changed.is_empty());
        assert_eq!(diff.removed.len(), 2);
    }

    #[test]
    fn diff_status_change_lands_in_changed() {
        // Same content, status flipped — surfaces as a single
        // `TodoChange`, not as removed+added. Clients can render
        // an in-place state transition (animate the checkbox) vs. a
        // wholesale list rewrite.
        let old = vec![item("A", TodoStatus::Pending, TodoPriority::High)];
        let new = vec![item("A", TodoStatus::InProgress, TodoPriority::High)];
        let diff = TodoDiff::compute(&old, &new);
        assert!(diff.added.is_empty());
        assert!(diff.removed.is_empty());
        assert_eq!(diff.changed.len(), 1);
        assert_eq!(diff.changed[0].before.status, TodoStatus::Pending);
        assert_eq!(diff.changed[0].after.status, TodoStatus::InProgress);
    }

    #[test]
    fn diff_rename_lands_as_remove_plus_add() {
        // Rename = different `content` string, so by design the diff
        // surfaces it as removal + addition rather than a
        // `TodoChange`. Documented behaviour on `TodoDiff` — if a
        // future product decision wants rename detection, that's a
        // schema change (need a stable id), not a diff-algorithm
        // tweak. Lock this in with a test so the trade-off doesn't
        // silently flip.
        let old = vec![item("old name", TodoStatus::Pending, TodoPriority::High)];
        let new = vec![item("new name", TodoStatus::Pending, TodoPriority::High)];
        let diff = TodoDiff::compute(&old, &new);
        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.removed.len(), 1);
        assert!(diff.changed.is_empty());
    }

    #[test]
    fn diff_unchanged_item_does_not_surface() {
        // An item identical on both sides must NOT appear in any
        // bucket. This is what lets clients render "only what
        // changed" without filtering noise.
        let old = vec![
            item("A", TodoStatus::Pending, TodoPriority::High),
            item("B", TodoStatus::InProgress, TodoPriority::Medium),
        ];
        let new = vec![
            item("A", TodoStatus::Pending, TodoPriority::High), // unchanged
            item("B", TodoStatus::Completed, TodoPriority::Medium), // status flipped
        ];
        let diff = TodoDiff::compute(&old, &new);
        assert!(diff.added.is_empty());
        assert!(diff.removed.is_empty());
        assert_eq!(diff.changed.len(), 1);
        assert_eq!(diff.changed[0].after.content, "B");
    }

    #[test]
    fn diff_priority_only_change_lands_in_changed() {
        // Edge case: priority changed but status unchanged. The
        // matching key is `content`; the change predicate is
        // `before != after`, which uses derived `PartialEq` on the
        // whole struct including priority. So priority bumps DO
        // surface as `changed`. Important for clients that render
        // priority badges — they need to know to re-render.
        let old = vec![item("A", TodoStatus::Pending, TodoPriority::Low)];
        let new = vec![item("A", TodoStatus::Pending, TodoPriority::High)];
        let diff = TodoDiff::compute(&old, &new);
        assert_eq!(diff.changed.len(), 1);
        assert_eq!(diff.changed[0].before.priority, TodoPriority::Low);
        assert_eq!(diff.changed[0].after.priority, TodoPriority::High);
    }

    #[test]
    fn diff_empty_when_lists_identical() {
        let old = vec![item("A", TodoStatus::Pending, TodoPriority::High)];
        let new = old.clone();
        let diff = TodoDiff::compute(&old, &new);
        assert!(diff.is_empty(), "identical lists must produce no diff");
    }

    // ── #1077 Phase A: outcome shape ────────────────────────

    /// `TodoWriteOutcome.items` must always carry the full list,
    /// even on the dedup-nudge path. This is what lets clients
    /// (e.g. ACP IDEs) mirror the latest state without re-reading
    /// from the DB on every event.
    #[tokio::test]
    async fn outcome_items_populated_on_dedup_path() {
        let (db, _dir, sid) = test_db().await;
        let args = json!({
            "todos": [
                {"content": "A", "status": "pending", "priority": "high"},
            ]
        });
        todo_write(&db, &sid, &args).await.unwrap();
        let out2 = todo_write(&db, &sid, &args).await.unwrap();
        assert!(out2.diff.is_empty(), "dedup must yield empty diff");
        assert_eq!(out2.items.len(), 1, "dedup must still populate items");
        assert_eq!(out2.items[0].content, "A");
    }

    /// First-ever write must produce a non-empty diff with the
    /// initial items in `added`. This is the event the dispatch
    /// layer surfaces as `EngineEvent::TodoUpdate`.
    #[tokio::test]
    async fn outcome_first_write_yields_added_diff() {
        let (db, _dir, sid) = test_db().await;
        let args = json!({
            "todos": [
                {"content": "A", "status": "pending", "priority": "high"},
                {"content": "B", "status": "in_progress", "priority": "medium"},
            ]
        });
        let out = todo_write(&db, &sid, &args).await.unwrap();
        assert!(!out.diff.is_empty());
        assert_eq!(out.diff.added.len(), 2);
        assert!(out.diff.removed.is_empty());
        assert!(out.diff.changed.is_empty());
        assert_eq!(out.items.len(), 2);
    }
}
