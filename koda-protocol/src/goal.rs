//! A session's goal: an objective Koda keeps working on, turn after turn,
//! until the model finds it achieved or the user stops it. Codex's
//! `thread/goal/*` (`app-server-protocol/src/protocol/v2/thread.rs`).
//!
//! While the goal is active, each time the session goes idle the host
//! starts another turn on it by itself, as `turn/started` shows. The model
//! sees the goal through its `GetGoal`, `CreateGoal` and `UpdateGoal` tools.
//! An interrupt stops the turn, not the goal, and no turn starts on the
//! goal by itself until a `turn/start` turn ends or a `session/goal/set`
//! goes on with it. `session/goal/set` with `status: "paused"` stops it, and
//! `session/goal/clear` removes it.

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

/// Where a goal stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum GoalStatus {
    /// Koda keeps working on it.
    Active,
    /// Stopped by the user.
    Paused,
    /// Stopped because no progress could be made: the model said so, a turn
    /// failed, or three turns in a row did nothing.
    Blocked,
    /// Its token budget is used up.
    BudgetLimited,
    /// The model found the objective achieved.
    Complete,
}

/// A session's goal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Goal {
    /// The session it belongs to.
    pub session_id: String,
    /// What to achieve.
    pub objective: String,
    /// Where it stands.
    pub status: GoalStatus,
    /// The tokens it may use, if limited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<i64>,
    /// Tokens its turns have used: input not read from the cache, plus
    /// output.
    pub tokens_used: i64,
    /// Time its turns have taken, in seconds.
    pub time_used_seconds: i64,
    /// When it was set, in Unix seconds.
    pub created_at: i64,
    /// When it last changed, in Unix seconds.
    pub updated_at: i64,
}

/// `session/goal/set`: set an open session's goal, or change it.
///
/// With an `objective` and no goal, a new goal starts, active unless
/// `status` says otherwise. With a goal, the fields given change it: a new
/// objective is told to the model at its next step. Without an objective,
/// the session must have a goal. Setting an active goal on an idle session
/// starts a turn on it. `session/goal/updated` follows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionGoalSetParams {
    /// An open session.
    pub session_id: String,
    /// The objective: not empty, at most 4,000 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objective: Option<String>,
    /// The status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<GoalStatus>,
    /// The token budget: a positive number, or `null` for none. Absent
    /// keeps it.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<i64>")]
    pub token_budget: Option<Option<i64>>,
}

/// Tell a field given as `null` (`Some(None)`) from one left out (`None`).
fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Option<i64>>, D::Error> {
    Option::<i64>::deserialize(deserializer).map(Some)
}

/// The answer to `session/goal/set`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionGoalSetResponse {
    /// The goal now.
    pub goal: Goal,
}

/// `session/goal/get`: an open session's goal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionGoalGetParams {
    /// An open session.
    pub session_id: String,
}

/// The answer to `session/goal/get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionGoalGetResponse {
    /// The goal, if the session has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<Goal>,
}

/// `session/goal/clear`: remove an open session's goal. A turn on it that
/// is running goes on to its end; no other starts. `session/goal/cleared`
/// follows when there was one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionGoalClearParams {
    /// An open session.
    pub session_id: String,
}

/// The answer to `session/goal/clear`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionGoalClearResponse {
    /// Whether the session had a goal.
    pub cleared: bool,
}

/// `session/goal/updated`: a session's goal was set or changed, by a client,
/// by the model, or by Koda as it counted a turn's use or stopped the goal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionGoalUpdatedNotification {
    /// The session.
    pub session_id: String,
    /// The turn that changed it, if one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// The goal now.
    pub goal: Goal,
}

/// What a goal's evaluator found after a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum GoalVerdict {
    /// The goal holds; `session/goal/updated` follows with it complete.
    Met,
    /// Not yet; the next turn on the goal takes the reason as guidance.
    NotYet,
    /// It can never be satisfied; `session/goal/updated` follows with it
    /// blocked.
    Impossible,
}

/// `session/goal/judged`: with `goal_judge = "evaluator"` (Claude Code's
/// `session/goal/held`: Koda stopped starting turns on the goal, which
/// stays set, after goal turns in a row that called no tool, as Claude
/// Code stops a goal loop that answers without working. The user's next
/// message, or `/goal resume`, goes on with it. Koda's own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionGoalHeldNotification {
    /// The session.
    pub session_id: String,
    /// The turn after which it stopped.
    pub turn_id: String,
    /// How many goal turns in a row called no tool.
    pub turns: u32,
}

/// way), the check after a turn on an active goal found it met, not yet
/// met, or impossible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionGoalJudgedNotification {
    /// The session.
    pub session_id: String,
    /// The turn it judged.
    pub turn_id: String,
    /// What it found.
    pub verdict: GoalVerdict,
    /// Why, in a sentence or two, if it said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `session/goal/cleared`: a session's goal was removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionGoalClearedNotification {
    /// The session.
    pub session_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_budget_left_out_is_kept_and_a_null_one_removed() {
        let set = |json: &str| serde_json::from_str::<SessionGoalSetParams>(json).unwrap();
        assert_eq!(set(r#"{"sessionId":"s"}"#).token_budget, None);
        assert_eq!(
            set(r#"{"sessionId":"s","tokenBudget":null}"#).token_budget,
            Some(None)
        );
        assert_eq!(
            set(r#"{"sessionId":"s","tokenBudget":5}"#).token_budget,
            Some(Some(5))
        );
        let status: GoalStatus = serde_json::from_str(r#""budgetLimited""#).unwrap();
        assert_eq!(status, GoalStatus::BudgetLimited);
    }
}
