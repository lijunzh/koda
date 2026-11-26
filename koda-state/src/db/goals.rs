//! A session's goal: one objective Koda keeps working on across turns until
//! it is complete, as Codex keeps a thread's (`codex-rs/state/src/runtime/goals.rs`).
//!
//! A session has at most one goal. Each goal has its own id, so a write
//! meant for a goal that has since been replaced (`expected_goal_id`) changes
//! nothing. What the goal has used, tokens and time, is added as its turns
//! end; once the tokens reach the goal's budget, an active goal becomes
//! `budget_limited`.

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use super::Database;
use crate::busy_retry::WriteQueryExt as _;

/// The longest objective a goal takes, in characters: Codex's limit.
pub const MAX_GOAL_OBJECTIVE_CHARS: usize = 4_000;

/// Where a goal stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    /// Koda keeps working on it: each time the session goes idle, it starts
    /// another turn.
    Active,
    /// Stopped by the user; resuming makes it active again.
    Paused,
    /// Stopped because no progress could be made: the model said so, or a
    /// turn failed, or turns repeatedly did nothing.
    Blocked,
    /// Its token budget is used up.
    BudgetLimited,
    /// The model found the objective achieved.
    Complete,
}

impl GoalStatus {
    /// The stored and logged name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::BudgetLimited => "budget_limited",
            Self::Complete => "complete",
        }
    }

    /// The status a stored name stands for.
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "active" => Self::Active,
            "paused" => Self::Paused,
            "blocked" => Self::Blocked,
            "budget_limited" => Self::BudgetLimited,
            "complete" => Self::Complete,
            _ => return None,
        })
    }
}

/// A session's goal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    /// The session it belongs to.
    pub session_id: String,
    /// This goal's own id: a replaced goal gets a new one.
    pub goal_id: String,
    /// What to achieve, as the user or the model wrote it.
    pub objective: String,
    /// Where it stands.
    pub status: GoalStatus,
    /// The tokens it may use, if limited.
    pub token_budget: Option<i64>,
    /// Tokens its turns have used: input not read from the cache, plus
    /// output, as a turn's budget counts them.
    pub tokens_used: i64,
    /// Wall-clock time its turns have taken, in seconds.
    pub time_used_seconds: i64,
    /// When it was created, in Unix seconds.
    pub created_at: i64,
    /// When it last changed, in Unix seconds.
    pub updated_at: i64,
}

impl Goal {
    /// Tokens left of the budget, never below zero; `None` without one.
    pub fn remaining_tokens(&self) -> Option<i64> {
        self.token_budget
            .map(|budget| (budget - self.tokens_used).max(0))
    }
}

/// A change to a session's goal.
#[derive(Debug, Clone, Default)]
pub struct GoalUpdate {
    /// A new objective, or `None` to keep it.
    pub objective: Option<String>,
    /// A new status, or `None` to keep it.
    pub status: Option<GoalStatus>,
    /// A new budget (`Some(None)` removes it), or `None` to keep it.
    pub token_budget: Option<Option<i64>>,
    /// Change the goal only if it is still this one.
    pub expected_goal_id: Option<String>,
}

/// Check an objective: not empty, and within [`MAX_GOAL_OBJECTIVE_CHARS`].
pub fn validate_objective(objective: &str) -> std::result::Result<(), String> {
    if objective.trim().is_empty() {
        return Err("a goal's objective can't be empty".to_string());
    }
    if objective.chars().count() > MAX_GOAL_OBJECTIVE_CHARS {
        return Err(format!(
            "a goal's objective is at most {MAX_GOAL_OBJECTIVE_CHARS} characters"
        ));
    }
    Ok(())
}

/// Check a budget: positive when given.
pub fn validate_budget(token_budget: Option<i64>) -> std::result::Result<(), String> {
    match token_budget {
        Some(budget) if budget <= 0 => Err("a goal's token budget must be positive".to_string()),
        _ => Ok(()),
    }
}

/// The budget a goal gets when `asked` is asked for and the config caps
/// goals at `max` (`max_goal_token_budget`), as Codex applies its
/// `max_goal_token_budget`: no budget means the maximum, and one over it
/// is refused.
pub fn budget_within(
    asked: Option<i64>,
    max: Option<i64>,
) -> std::result::Result<Option<i64>, String> {
    validate_budget(asked)?;
    match (asked, max) {
        (Some(budget), Some(max)) if budget > max => Err(format!(
            "goal token budget {budget} exceeds the maximum allowed goal token budget of {max}"
        )),
        _ => Ok(asked.or(max)),
    }
}

#[derive(sqlx::FromRow)]
struct GoalRow {
    session_id: String,
    goal_id: String,
    objective: String,
    status: String,
    token_budget: Option<i64>,
    tokens_used: i64,
    time_used_seconds: i64,
    created_at: i64,
    updated_at: i64,
}

impl TryFrom<GoalRow> for Goal {
    type Error = anyhow::Error;

    fn try_from(row: GoalRow) -> Result<Self> {
        Ok(Self {
            status: GoalStatus::parse(&row.status)
                .with_context(|| format!("unknown goal status {:?}", row.status))?,
            session_id: row.session_id,
            goal_id: row.goal_id,
            objective: row.objective,
            token_budget: row.token_budget,
            tokens_used: row.tokens_used,
            time_used_seconds: row.time_used_seconds,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// The status a goal is stored with: an active goal whose tokens have
/// reached its budget is `budget_limited`.
fn within_budget(status: GoalStatus, tokens_used: i64, token_budget: Option<i64>) -> GoalStatus {
    match token_budget {
        Some(budget) if status == GoalStatus::Active && tokens_used >= budget => {
            GoalStatus::BudgetLimited
        }
        _ => status,
    }
}

impl Database {
    /// Create the goals table. Called by the schema migration.
    pub(crate) async fn migrate_goals(conn: &mut sqlx::SqliteConnection) -> Result<()> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS session_goals (
                session_id TEXT PRIMARY KEY NOT NULL,
                goal_id TEXT NOT NULL,
                objective TEXT NOT NULL,
                status TEXT NOT NULL,
                token_budget INTEGER,
                tokens_used INTEGER NOT NULL DEFAULT 0,
                time_used_seconds INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                FOREIGN KEY(session_id) REFERENCES sessions(id)
            );",
        )
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    /// The session's goal, if it has one.
    pub async fn get_goal(&self, session_id: &str) -> Result<Option<Goal>> {
        let row: Option<GoalRow> = sqlx::query_as(
            "SELECT session_id, goal_id, objective, status, token_budget, tokens_used,
                    time_used_seconds, created_at, updated_at
             FROM session_goals WHERE session_id = ?",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(Goal::try_from).transpose()
    }

    /// Start a new goal for the session, replacing any it has. `status`
    /// becomes `budget_limited` if the budget is already reached.
    pub async fn replace_goal(
        &self,
        session_id: &str,
        objective: &str,
        status: GoalStatus,
        token_budget: Option<i64>,
    ) -> Result<Goal> {
        let now = now();
        let status = within_budget(status, 0, token_budget);
        sqlx::query(
            "INSERT INTO session_goals (session_id, goal_id, objective, status, token_budget,
                 tokens_used, time_used_seconds, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, 0, 0, ?, ?)
             ON CONFLICT(session_id) DO UPDATE SET
                 goal_id = excluded.goal_id,
                 objective = excluded.objective,
                 status = excluded.status,
                 token_budget = excluded.token_budget,
                 tokens_used = 0,
                 time_used_seconds = 0,
                 created_at = excluded.created_at,
                 updated_at = excluded.updated_at",
        )
        .bind(session_id)
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(objective)
        .bind(status.as_str())
        .bind(token_budget)
        .bind(now)
        .bind(now)
        .execute_write(&self.pool)
        .await?;
        self.get_goal(session_id)
            .await?
            .context("the goal just stored isn't there")
    }

    /// Start a new active goal for the session, as the model's `CreateGoal`
    /// does: only when it has none, or its goal is complete. `None` when an
    /// unfinished goal is in the way.
    pub async fn create_goal(
        &self,
        session_id: &str,
        objective: &str,
        token_budget: Option<i64>,
    ) -> Result<Option<Goal>> {
        let now = now();
        let status = within_budget(GoalStatus::Active, 0, token_budget);
        let result = sqlx::query(
            "INSERT INTO session_goals (session_id, goal_id, objective, status, token_budget,
                 tokens_used, time_used_seconds, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, 0, 0, ?, ?)
             ON CONFLICT(session_id) DO UPDATE SET
                 goal_id = excluded.goal_id,
                 objective = excluded.objective,
                 status = excluded.status,
                 token_budget = excluded.token_budget,
                 tokens_used = 0,
                 time_used_seconds = 0,
                 created_at = excluded.created_at,
                 updated_at = excluded.updated_at
             WHERE session_goals.status = 'complete'",
        )
        .bind(session_id)
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(objective)
        .bind(status.as_str())
        .bind(token_budget)
        .bind(now)
        .bind(now)
        .execute_write(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        self.get_goal(session_id).await
    }

    /// Change the session's goal. `None` when it has none, or it isn't the
    /// expected one.
    ///
    /// Codex's rules for the status: a `budget_limited` goal stays so when
    /// asked to pause or block (the budget takes precedence), and a goal
    /// made active whose tokens have reached its budget is `budget_limited`.
    pub async fn update_goal(&self, session_id: &str, update: GoalUpdate) -> Result<Option<Goal>> {
        let Some(goal) = self.get_goal(session_id).await? else {
            return Ok(None);
        };
        if update
            .expected_goal_id
            .as_ref()
            .is_some_and(|expected| *expected != goal.goal_id)
        {
            return Ok(None);
        }
        let token_budget = update.token_budget.unwrap_or(goal.token_budget);
        let status = match update.status {
            Some(GoalStatus::Paused | GoalStatus::Blocked)
                if goal.status == GoalStatus::BudgetLimited =>
            {
                GoalStatus::BudgetLimited
            }
            Some(status) => within_budget(status, goal.tokens_used, token_budget),
            None => within_budget(goal.status, goal.tokens_used, token_budget),
        };
        let result = sqlx::query(
            "UPDATE session_goals
             SET objective = ?, status = ?, token_budget = ?, updated_at = ?
             WHERE session_id = ? AND goal_id = ?",
        )
        .bind(update.objective.as_deref().unwrap_or(&goal.objective))
        .bind(status.as_str())
        .bind(token_budget)
        .bind(now())
        .bind(session_id)
        .bind(&goal.goal_id)
        .execute_write(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        self.get_goal(session_id).await
    }

    /// Add a turn's tokens and time to goal `goal_id`, whatever its status,
    /// and mark it `budget_limited` if it is active and the tokens reach its
    /// budget. `None` when the session's goal is no longer that one.
    pub async fn account_goal(
        &self,
        session_id: &str,
        goal_id: &str,
        tokens: i64,
        seconds: i64,
    ) -> Result<Option<Goal>> {
        let (tokens, seconds) = (tokens.max(0), seconds.max(0));
        let result = sqlx::query(
            "UPDATE session_goals
             SET tokens_used = tokens_used + ?,
                 time_used_seconds = time_used_seconds + ?,
                 status = CASE
                     WHEN status = 'active' AND token_budget IS NOT NULL
                          AND tokens_used + ? >= token_budget THEN 'budget_limited'
                     ELSE status
                 END,
                 updated_at = ?
             WHERE session_id = ? AND goal_id = ?",
        )
        .bind(tokens)
        .bind(seconds)
        .bind(tokens)
        .bind(now())
        .bind(session_id)
        .bind(goal_id)
        .execute_write(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        self.get_goal(session_id).await
    }

    /// Remove the session's goal, returning it.
    pub async fn delete_goal(&self, session_id: &str) -> Result<Option<Goal>> {
        let Some(goal) = self.get_goal(session_id).await? else {
            return Ok(None);
        };
        sqlx::query("DELETE FROM session_goals WHERE session_id = ? AND goal_id = ?")
            .bind(session_id)
            .bind(&goal.goal_id)
            .execute_write(&self.pool)
            .await?;
        Ok(Some(goal))
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_maximum_budget_is_the_default_and_the_ceiling() {
        assert_eq!(super::budget_within(None, None), Ok(None));
        assert_eq!(super::budget_within(Some(5), None), Ok(Some(5)));
        assert_eq!(super::budget_within(None, Some(100)), Ok(Some(100)));
        assert_eq!(super::budget_within(Some(40), Some(100)), Ok(Some(40)));
        assert!(super::budget_within(Some(101), Some(100)).is_err());
        assert!(super::budget_within(Some(0), Some(100)).is_err());
    }
    use super::*;
    use crate::session_store::SessionStore as _;

    async fn db() -> (tempfile::TempDir, Database, String) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("koda.db")).await.unwrap();
        let id = db.create_session("koda", dir.path()).await.unwrap();
        (dir, db, id)
    }

    #[tokio::test]
    async fn a_goal_is_stored_with_the_session_and_replaced_with_a_new_id() {
        let (_dir, db, id) = db().await;
        assert_eq!(db.get_goal(&id).await.unwrap(), None);
        let first = db
            .replace_goal(&id, "ship it", GoalStatus::Active, Some(1_000))
            .await
            .unwrap();
        assert_eq!(first.objective, "ship it");
        assert_eq!(first.status, GoalStatus::Active);
        assert_eq!(first.remaining_tokens(), Some(1_000));
        let second = db
            .replace_goal(&id, "ship it again", GoalStatus::Paused, None)
            .await
            .unwrap();
        assert_ne!(first.goal_id, second.goal_id);
        assert_eq!(db.get_goal(&id).await.unwrap(), Some(second));
    }

    #[tokio::test]
    async fn the_model_creates_a_goal_only_over_none_or_a_complete_one() {
        let (_dir, db, id) = db().await;
        let goal = db.create_goal(&id, "a", None).await.unwrap().unwrap();
        assert!(db.create_goal(&id, "b", None).await.unwrap().is_none());
        for status in [GoalStatus::Paused, GoalStatus::Blocked] {
            db.update_goal(
                &id,
                GoalUpdate {
                    status: Some(status),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            assert!(
                db.create_goal(&id, "b", None).await.unwrap().is_none(),
                "{status:?} is unfinished"
            );
        }
        db.update_goal(
            &id,
            GoalUpdate {
                status: Some(GoalStatus::Complete),
                expected_goal_id: Some(goal.goal_id.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let next = db.create_goal(&id, "b", None).await.unwrap().unwrap();
        assert_eq!(next.objective, "b");
        assert_eq!(next.status, GoalStatus::Active);
        assert_ne!(next.goal_id, goal.goal_id);
    }

    #[tokio::test]
    async fn using_the_budget_up_limits_an_active_goal() {
        let (_dir, db, id) = db().await;
        let goal = db
            .replace_goal(&id, "a", GoalStatus::Active, Some(100))
            .await
            .unwrap();
        let goal = db
            .account_goal(&id, &goal.goal_id, 60, 3)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (goal.status, goal.tokens_used, goal.time_used_seconds),
            (GoalStatus::Active, 60, 3)
        );
        let goal = db
            .account_goal(&id, &goal.goal_id, 40, 2)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(goal.status, GoalStatus::BudgetLimited);
        assert_eq!(goal.remaining_tokens(), Some(0));
        // The budget wins over a pause, and resuming stays limited.
        for status in [GoalStatus::Paused, GoalStatus::Active] {
            let goal = db
                .update_goal(
                    &id,
                    GoalUpdate {
                        status: Some(status),
                        ..Default::default()
                    },
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(goal.status, GoalStatus::BudgetLimited, "{status:?}");
        }
        // A bigger budget lets it go on.
        let goal = db
            .update_goal(
                &id,
                GoalUpdate {
                    status: Some(GoalStatus::Active),
                    token_budget: Some(Some(500)),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(goal.status, GoalStatus::Active);
        assert_eq!(goal.remaining_tokens(), Some(400));
    }

    #[tokio::test]
    async fn a_write_for_a_replaced_goal_changes_nothing() {
        let (_dir, db, id) = db().await;
        let old = db
            .replace_goal(&id, "old", GoalStatus::Active, None)
            .await
            .unwrap();
        let new = db
            .replace_goal(&id, "new", GoalStatus::Active, None)
            .await
            .unwrap();
        assert!(
            db.account_goal(&id, &old.goal_id, 10, 1)
                .await
                .unwrap()
                .is_none()
        );
        let update = GoalUpdate {
            status: Some(GoalStatus::Complete),
            expected_goal_id: Some(old.goal_id),
            ..Default::default()
        };
        assert!(db.update_goal(&id, update).await.unwrap().is_none());
        assert_eq!(db.get_goal(&id).await.unwrap(), Some(new));
    }

    #[tokio::test]
    async fn a_goal_goes_with_its_session() {
        let (_dir, db, id) = db().await;
        db.replace_goal(&id, "a", GoalStatus::Active, None)
            .await
            .unwrap();
        assert!(db.delete_session(&id).await.unwrap());
        assert_eq!(db.get_goal(&id).await.unwrap(), None);
        // And clearing one returns what was cleared.
        let other = db
            .create_session("koda", std::path::Path::new("/tmp"))
            .await
            .unwrap();
        db.replace_goal(&other, "b", GoalStatus::Active, None)
            .await
            .unwrap();
        let cleared = db.delete_goal(&other).await.unwrap().unwrap();
        assert_eq!(cleared.objective, "b");
        assert_eq!(db.delete_goal(&other).await.unwrap(), None);
    }

    #[test]
    fn objectives_and_budgets_are_checked() {
        assert!(validate_objective(" ").is_err());
        assert!(validate_objective(&"x".repeat(MAX_GOAL_OBJECTIVE_CHARS)).is_ok());
        assert!(validate_objective(&"x".repeat(MAX_GOAL_OBJECTIVE_CHARS + 1)).is_err());
        assert!(validate_budget(Some(0)).is_err());
        assert!(validate_budget(None).is_ok());
        assert!(validate_budget(Some(1)).is_ok());
    }
}
