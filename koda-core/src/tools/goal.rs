//! The model's goal tools: `GetGoal`, `CreateGoal` and `UpdateGoal`,
//! Codex's `get_goal`, `create_goal` and `update_goal`
//! (`codex-rs/ext/goal/src/spec.rs`, whose descriptions these keep, with
//! Koda's tool names).
//!
//! They read and write the session's goal ([`koda_state::db::goals`]); the
//! host runs the turns that keep an active goal going. A change is sent to
//! clients as [`EngineEvent::GoalUpdated`]. Like `TodoWrite`, they change
//! only Koda's own state, so they run without approval in every mode.
//!
//! [`EngineEvent::GoalUpdated`]: crate::engine::EngineEvent::GoalUpdated

use async_trait::async_trait;
use koda_state::db::goals::{self, Goal, GoalStatus, GoalUpdate};
use serde_json::{Value, json};

use crate::providers::ToolDefinition;
use crate::tools::{Tool, ToolEffect, ToolExecCtx, ToolResult};

/// `GetGoal`.
pub struct GetGoalTool;
/// `CreateGoal`.
pub struct CreateGoalTool;
/// `UpdateGoal`.
pub struct UpdateGoalTool;

/// The three tools' definitions.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![
        GetGoalTool.definition(),
        CreateGoalTool.definition(),
        UpdateGoalTool.definition(),
    ]
}

#[async_trait]
impl Tool for GetGoalTool {
    fn name(&self) -> &'static str {
        "GetGoal"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Get the current goal for this thread, including status, budgets, \
                token and elapsed-time usage, and remaining token budget."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }),
        }
    }

    fn classify(&self, _args: &Value) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    async fn execute(&self, ctx: &ToolExecCtx<'_>, _args: &Value) -> ToolResult {
        let Some((db, session_id)) = ctx.session else {
            return failed(NO_SESSION);
        };
        match db.get_goal(session_id).await {
            Ok(goal) => answer(goal.as_ref(), false),
            Err(e) => failed(format!("failed to read goal: {e:#}")),
        }
    }
}

#[async_trait]
impl Tool for CreateGoalTool {
    fn name(&self) -> &'static str {
        "CreateGoal"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Create a goal only when explicitly requested by the user or \
                system/developer instructions; do not infer goals from ordinary tasks.\n\
                Set token_budget only when an explicit token budget is requested. Fails if an \
                unfinished goal exists; use UpdateGoal only for status."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "objective": {
                        "type": "string",
                        "description": "Required. The concrete objective to start pursuing. \
                            This starts a new active goal when no goal exists or replaces the \
                            current goal when it is complete."
                    },
                    "token_budget": {
                        "type": "integer",
                        "description": "Positive token budget for the new goal. Omit unless \
                            explicitly requested."
                    }
                },
                "required": ["objective"],
                "additionalProperties": false
            }),
        }
    }

    fn classify(&self, _args: &Value) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    async fn execute(&self, ctx: &ToolExecCtx<'_>, args: &Value) -> ToolResult {
        let Some((db, session_id)) = ctx.session else {
            return failed(NO_SESSION);
        };
        let objective = args["objective"].as_str().unwrap_or_default().trim();
        if let Err(why) = goals::validate_objective(objective) {
            return failed(why);
        }
        let token_budget = match &args["token_budget"] {
            Value::Null => None,
            value => match value.as_i64() {
                Some(budget) => Some(budget),
                None => return failed("token_budget must be an integer"),
            },
        };
        if let Err(why) = goals::validate_budget(token_budget) {
            return failed(why);
        }
        match db.create_goal(session_id, objective, token_budget).await {
            Ok(Some(goal)) => {
                updated(ctx, &goal);
                answer(Some(&goal), false)
            }
            Ok(None) => failed(
                "cannot create a new goal because this thread has an unfinished goal; \
                 complete the existing goal first",
            ),
            Err(e) => failed(format!("failed to create goal: {e:#}")),
        }
    }
}

#[async_trait]
impl Tool for UpdateGoalTool {
    fn name(&self) -> &'static str {
        "UpdateGoal"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Update the existing goal.\n\
                Set status to `paused` only at the user's explicit request to pause this goal, \
                never on your own initiative. Ask if unclear; a later resume revokes that \
                request. Report the returned status and stop goal work. Budget limits take \
                precedence over pausing.\n\
                Set status to `complete` only when the objective has actually been achieved and \
                no required work remains.\n\
                Set status to `blocked` only when the same blocking condition has repeated for \
                at least three consecutive goal turns, counting the original/user-triggered turn \
                and any automatic continuations, and the agent cannot make meaningful progress \
                without user input or an external-state change.\n\
                If the user resumes a goal that was previously marked `blocked`, treat the \
                resumed run as a fresh blocked audit. If the same blocking condition then \
                repeats for at least three consecutive resumed goal turns, set status to \
                `blocked` again.\n\
                Once the blocked threshold is satisfied, do not keep reporting that you are \
                still blocked while leaving the goal active; set status to `blocked`.\n\
                Do not use `blocked` merely because the work is hard, slow, uncertain, \
                incomplete, or would benefit from clarification.\n\
                Do not mark a goal complete merely because its budget is nearly exhausted or \
                because you are stopping work.\n\
                You cannot use this tool to resume, budget-limit, or usage-limit a goal; those \
                status changes are controlled by the user or system.\n\
                When marking a budgeted goal achieved with status `complete`, report the final \
                token usage from the tool result to the user."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "enum": ["complete", "blocked", "paused"],
                        "description": "Required. `paused` requires an explicit user request. \
                            Set to `complete` only when the objective is achieved and no required \
                            work remains. Set to `blocked` only after the same blocking condition \
                            has recurred for at least three consecutive goal turns and the agent \
                            is at an impasse. After a previously blocked goal is resumed, the \
                            resumed run starts a fresh blocked audit."
                    }
                },
                "required": ["status"],
                "additionalProperties": false
            }),
        }
    }

    fn classify(&self, _args: &Value) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    async fn execute(&self, ctx: &ToolExecCtx<'_>, args: &Value) -> ToolResult {
        let Some((db, session_id)) = ctx.session else {
            return failed(NO_SESSION);
        };
        let status = match args["status"].as_str() {
            Some("complete") => GoalStatus::Complete,
            Some("blocked") => GoalStatus::Blocked,
            Some("paused") => GoalStatus::Paused,
            _ => {
                return failed(
                    "UpdateGoal can only mark the existing goal complete, blocked, or paused at \
                     the user's explicit request; resume, budget-limited, and usage-limited \
                     status changes are controlled by the user or system",
                );
            }
        };
        let update = GoalUpdate {
            status: Some(status),
            ..Default::default()
        };
        match db.update_goal(session_id, update).await {
            Ok(Some(goal)) => {
                updated(ctx, &goal);
                answer(Some(&goal), status == GoalStatus::Complete)
            }
            Ok(None) => failed("cannot update goal because this thread has no goal"),
            Err(e) => failed(format!("failed to update goal: {e:#}")),
        }
    }
}

const NO_SESSION: &str = "Goal tools require a persistent thread.";

/// Tell clients the goal changed.
fn updated(ctx: &ToolExecCtx<'_>, goal: &Goal) {
    if let Some((sink, _)) = ctx.sink {
        sink.emit(crate::engine::EngineEvent::GoalUpdated { goal: goal.clone() });
    }
}

/// The tool's answer, as Codex's: the goal, the tokens left of its budget,
/// and, when it was just completed with a budget or time spent, a reminder
/// to report the usage.
fn answer(goal: Option<&Goal>, completed: bool) -> ToolResult {
    let report = goal
        .filter(|goal| {
            completed
                && goal.status == GoalStatus::Complete
                && (goal.token_budget.is_some() || goal.time_used_seconds > 0)
        })
        .map(|_| {
            "Goal achieved. Report final usage from this tool result's structured goal fields. \
             If `goal.tokenBudget` is present, include token usage from `goal.tokensUsed` and \
             `goal.tokenBudget`. If `goal.timeUsedSeconds` is greater than 0, summarize elapsed \
             time in a concise, human-friendly form appropriate to the response language."
        });
    let mut body = json!({
        "goal": goal.map(|goal| json!({
            "objective": goal.objective,
            "status": goal.status,
            "tokenBudget": goal.token_budget,
            "tokensUsed": goal.tokens_used,
            "timeUsedSeconds": goal.time_used_seconds,
        })),
        "remainingTokens": goal.and_then(Goal::remaining_tokens),
        "completionBudgetReport": report,
    });
    // What isn't there is left out, not sent as `null`.
    if let Some(fields) = body.as_object_mut() {
        fields.retain(|_, value| !value.is_null());
        if let Some(Value::Object(goal)) = fields.get_mut("goal") {
            goal.retain(|_, value| !value.is_null());
        }
    }
    ToolResult {
        output: serde_json::to_string_pretty(&body).unwrap_or_default(),
        success: true,
        full_output: None,
        images: Vec::new(),
    }
}

fn failed(why: impl Into<String>) -> ToolResult {
    ToolResult {
        output: why.into(),
        success: false,
        full_output: None,
        images: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_answer_reports_usage_only_for_a_goal_just_completed() {
        let mut goal = Goal {
            session_id: "s".into(),
            goal_id: "g".into(),
            objective: "o".into(),
            status: GoalStatus::Complete,
            token_budget: Some(100),
            tokens_used: 40,
            time_used_seconds: 0,
            created_at: 0,
            updated_at: 0,
        };
        let body = |result: ToolResult| -> Value { serde_json::from_str(&result.output).unwrap() };
        let done = body(answer(Some(&goal), true));
        assert_eq!(done["remainingTokens"], 60);
        assert_eq!(done["goal"]["status"], "complete");
        assert!(done["completionBudgetReport"].is_string());
        assert!(body(answer(Some(&goal), false))["completionBudgetReport"].is_null());
        goal.token_budget = None;
        let unbudgeted = body(answer(Some(&goal), true));
        assert!(
            unbudgeted.get("completionBudgetReport").is_none(),
            "{unbudgeted}"
        );
        assert!(
            unbudgeted["goal"].get("tokenBudget").is_none(),
            "{unbudgeted}"
        );
        assert_eq!(body(answer(None, false)), json!({}));
    }
}
