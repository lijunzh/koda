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

// ── Handler ─────────────────────────────────────────────────────────────────

/// Write the full todo list for this session.
///
/// **Content-aware dedup**: if the incoming list is identical to what's
/// already stored, we skip the write and return a short "unchanged"
/// message. This prevents the model from burning tool calls (and
/// triggering loop detection) by re-emitting the same plan.
pub async fn todo_write(db: &Database, session_id: &str, args: &Value) -> Result<String> {
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

    // ── Content-aware dedup ──────────────────────────────────────────
    // Compare with what's already stored. If identical, skip the write
    // and tell the model explicitly so it stops re-emitting the same plan.
    if let Ok(Some(existing_json)) = db.get_todo(session_id).await
        && let Ok(existing) = serde_json::from_str::<Vec<TodoItem>>(&existing_json)
        && existing == todos
    {
        return Ok(format!(
            "Todo list unchanged ({} task{}). \
             Do not call TodoWrite again unless you are changing a task's status or content.",
            todos.len(),
            if todos.len() == 1 { "" } else { "s" }
        ));
    }

    let json = serde_json::to_string(&todos)?;
    db.set_todo(session_id, &json).await?;

    Ok(format_todo_list(&todos))
}

/// Load the todo section for injection into the system prompt.
///
/// Returns an empty string when no todos are stored (zero cost).
pub async fn get_todo_section(db: &Database, session_id: &str) -> String {
    let Ok(Some(raw)) = db.get_todo(session_id).await else {
        return String::new();
    };
    let Ok(todos) = serde_json::from_str::<Vec<TodoItem>>(&raw) else {
        return String::new();
    };
    if todos.is_empty() {
        return String::new();
    }

    // Only surface non-completed tasks in the prompt (completed ones are noise).
    let active: Vec<&TodoItem> = todos
        .iter()
        .filter(|t| t.status != TodoStatus::Completed)
        .collect();

    if active.is_empty() {
        return String::new();
    }

    let mut out = "\n## Current Tasks\n".to_string();
    for t in &active {
        out.push_str(&format!("{}{}\n", format_item(t), t.priority.suffix(),));
    }
    out
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

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

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
        assert!(out.contains("0/2 done"));
        assert!(out.contains("[ ] Add tests"));
        assert!(out.contains("[→] Write docs"));

        let section = get_todo_section(&db, &sid).await;
        assert!(section.contains("Add tests"));
        assert!(section.contains("Write docs"));
    }

    #[tokio::test]
    async fn completed_tasks_hidden_from_section() {
        let (db, _dir, sid) = test_db().await;
        let args = json!({
            "todos": [
                {"content": "Done task", "status": "completed", "priority": "low"},
                {"content": "Active task", "status": "pending", "priority": "high"},
            ]
        });
        todo_write(&db, &sid, &args).await.unwrap();
        let section = get_todo_section(&db, &sid).await;
        assert!(
            !section.contains("Done task"),
            "completed tasks should be hidden"
        );
        assert!(section.contains("Active task"));
    }

    #[tokio::test]
    async fn all_completed_returns_empty_section() {
        let (db, _dir, sid) = test_db().await;
        let args = json!({
            "todos": [
                {"content": "Done", "status": "completed", "priority": "medium"}
            ]
        });
        todo_write(&db, &sid, &args).await.unwrap();
        assert!(get_todo_section(&db, &sid).await.is_empty());
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
        assert!(out.contains("cleared"));
        assert!(get_todo_section(&db, &sid).await.is_empty());
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
        assert!(out1.contains("0/2 done"));

        // Second write with identical content — should short-circuit
        let out2 = todo_write(&db, &sid, &args).await.unwrap();
        assert!(
            out2.contains("unchanged"),
            "identical call should return 'unchanged', got: {out2}"
        );
        assert!(
            out2.contains("Do not call TodoWrite again"),
            "should tell model to stop calling"
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
            out.contains("1/1 done"),
            "status change should write normally, got: {out}"
        );
        assert!(out.contains("[x] Task A"));
    }
}
