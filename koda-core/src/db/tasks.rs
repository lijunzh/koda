//! Background task registry — #884 Phase 2.
//!
//! Tracks headless tasks the supervisor spawns so they survive terminal
//! disconnection and can be inspected with `koda tasks`.

use anyhow::Result;

use super::Database;

/// Status of a background task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskStatus {
    /// Task is currently running.
    Running,
    /// Task completed successfully (exit code 0).
    Done,
    /// Task exited with a non-zero status.
    Failed,
}

impl TaskStatus {
    /// Return the canonical string representation stored in the database.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A row from the `tasks` table.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TaskRow {
    /// Unique task ID (UUIDv4).
    pub id: String,
    /// Current status: `"running"`, `"done"`, or `"failed"`.
    pub status: String,
    /// The user's original prompt text.
    pub prompt: String,
    /// Session ID used for this task's conversation history.
    pub session_id: Option<String>,
    /// Canonical project root path.
    pub project_root: Option<String>,
    /// Unix timestamp (seconds) when the task was created.
    pub created_at: i64,
    /// Unix timestamp (seconds) when the task finished, if it has.
    pub completed_at: Option<i64>,
    /// Process exit code of the worker, if it has exited.
    pub exit_code: Option<i32>,
}

impl Database {
    /// Insert a new task record when the supervisor spawns a worker.
    pub async fn create_task(
        &self,
        id: &str,
        prompt: &str,
        session_id: &str,
        project_root: &str,
    ) -> Result<()> {
        let now = unix_now();
        sqlx::query(
            "INSERT INTO tasks (id, status, prompt, session_id, project_root, created_at)
             VALUES (?, 'running', ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(prompt)
        .bind(session_id)
        .bind(project_root)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Mark a task as complete (done or failed) once the worker exits.
    pub async fn complete_task(&self, id: &str, exit_code: i32) -> Result<()> {
        let status = if exit_code == 0 {
            TaskStatus::Done
        } else {
            TaskStatus::Failed
        };
        let now = unix_now();
        sqlx::query("UPDATE tasks SET status = ?, completed_at = ?, exit_code = ? WHERE id = ?")
            .bind(status.as_str())
            .bind(now)
            .bind(exit_code)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Return the most recent `limit` tasks, newest first.
    pub async fn list_tasks(&self, limit: i64) -> Result<Vec<TaskRow>> {
        let rows: Vec<TaskRow> = sqlx::query_as(
            "SELECT id, status, prompt, session_id, project_root,
                    created_at, completed_at, exit_code
             FROM tasks
             ORDER BY created_at DESC
             LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Fetch a single task by ID.
    pub async fn get_task(&self, id: &str) -> Result<Option<TaskRow>> {
        let row: Option<TaskRow> = sqlx::query_as(
            "SELECT id, status, prompt, session_id, project_root,
                    created_at, completed_at, exit_code
             FROM tasks WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn temp_db() -> (Database, TempDir) {
        let tmp = TempDir::new().unwrap();
        let db = Database::open(&tmp.path().join("tasks-test.db"))
            .await
            .unwrap();
        (db, tmp)
    }

    #[tokio::test]
    async fn create_and_retrieve_task() {
        let (db, _tmp) = temp_db().await;
        db.create_task("t1", "fix tests", "sess-1", "/home/user/proj")
            .await
            .unwrap();
        let row = db.get_task("t1").await.unwrap().expect("task must exist");
        assert_eq!(row.id, "t1");
        assert_eq!(row.status, "running");
        assert_eq!(row.prompt, "fix tests");
        assert!(row.completed_at.is_none());
    }

    #[tokio::test]
    async fn complete_task_marks_done() {
        let (db, _tmp) = temp_db().await;
        db.create_task("t2", "refactor", "sess-2", "/proj")
            .await
            .unwrap();
        db.complete_task("t2", 0).await.unwrap();
        let row = db.get_task("t2").await.unwrap().expect("must exist");
        assert_eq!(row.status, "done");
        assert_eq!(row.exit_code, Some(0));
        assert!(row.completed_at.is_some());
    }

    #[tokio::test]
    async fn complete_task_nonzero_is_failed() {
        let (db, _tmp) = temp_db().await;
        db.create_task("t3", "crash", "sess-3", "/proj")
            .await
            .unwrap();
        db.complete_task("t3", 1).await.unwrap();
        let row = db.get_task("t3").await.unwrap().expect("must exist");
        assert_eq!(row.status, "failed");
    }

    #[tokio::test]
    async fn list_tasks_newest_first() {
        let (db, _tmp) = temp_db().await;
        // Timestamps are integer seconds — insert with explicit created_at.
        sqlx::query("INSERT INTO tasks (id, status, prompt, created_at) VALUES ('t-old','running','first',1)")
            .execute(&db.pool).await.unwrap();
        sqlx::query("INSERT INTO tasks (id, status, prompt, created_at) VALUES ('t-new','running','second',2)")
            .execute(&db.pool).await.unwrap();
        let rows = db.list_tasks(10).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "t-new");
        assert_eq!(rows[1].id, "t-old");
    }
}
