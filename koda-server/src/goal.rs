//! A session's goal on the host: `session/goal/*`, and the bookkeeping that
//! keeps an active goal going, as Codex's goal extension does
//! (`codex-rs/ext/goal`).
//!
//! The session's task runs the loop ([`crate::session`]): each time the
//! session goes idle with its goal active, it starts another turn on the
//! goal by itself, unless a client's command is waiting. After every turn
//! it adds what the turn used to the goal, and stops the goal (`blocked`)
//! the way Codex does:
//!
//! - **a turn that failed,** so a failing model call doesn't loop;
//! - **three automatic turns in a row that did nothing:** no text, no
//!   tool call;
//! - **three goal turns in a row whose only tool results were failed
//!   commands:** the shell can't do what the goal needs.
//!
//! An interrupt ends the turn, not the goal, but no turn on the goal starts
//! after it until the user starts one or changes the goal (`/goal resume`),
//! as in Claude Code; the goal goes on once that turn ends. An interrupted
//! turn isn't counted as one that did nothing. The user stops a goal by
//! pausing or clearing it. Reaching the token budget makes the goal `budget_limited`: the
//! running turn is told to wrap up, and no other starts.

use koda_core::engine::{EngineEvent, ToolOutcome, UsageTotals};
use koda_core::goal::{Goal, GoalStatus};
use koda_protocol::goal::{self as wire, SessionGoalSetParams};
use koda_protocol::rpc::ErrorObject;
use koda_state::db::Database;
use koda_state::db::goals::{self, GoalUpdate};

use crate::session::{internal, invalid};

/// Turns in a row, of either kind, after which Codex blocks a goal.
const STRIKES: u8 = 3;

/// A goal as the protocol sends it.
pub(crate) fn to_wire(goal: &Goal) -> wire::Goal {
    wire::Goal {
        session_id: goal.session_id.clone(),
        objective: goal.objective.clone(),
        status: match goal.status {
            GoalStatus::Active => wire::GoalStatus::Active,
            GoalStatus::Paused => wire::GoalStatus::Paused,
            GoalStatus::Blocked => wire::GoalStatus::Blocked,
            GoalStatus::BudgetLimited => wire::GoalStatus::BudgetLimited,
            GoalStatus::Complete => wire::GoalStatus::Complete,
        },
        token_budget: goal.token_budget,
        tokens_used: goal.tokens_used,
        time_used_seconds: goal.time_used_seconds,
        created_at: goal.created_at,
        updated_at: goal.updated_at,
    }
}

fn from_wire(status: wire::GoalStatus) -> GoalStatus {
    match status {
        wire::GoalStatus::Active => GoalStatus::Active,
        wire::GoalStatus::Paused => GoalStatus::Paused,
        wire::GoalStatus::Blocked => GoalStatus::Blocked,
        wire::GoalStatus::BudgetLimited => GoalStatus::BudgetLimited,
        wire::GoalStatus::Complete => GoalStatus::Complete,
    }
}

/// What a `session/goal/set` did.
pub(crate) struct GoalSet {
    /// The goal now.
    pub goal: Goal,
    /// The same goal's objective changed, so a turn on it is told.
    pub objective_changed: bool,
}

/// `session/goal/set`, as Codex's `thread/goal/set`: a new goal when the
/// session has none, else a change to the one it has.
pub(crate) async fn set(
    db: &Database,
    session_id: &str,
    params: SessionGoalSetParams,
) -> Result<GoalSet, ErrorObject> {
    let objective = params.objective.as_deref().map(str::trim);
    if let Some(objective) = objective {
        goals::validate_objective(objective).map_err(invalid)?;
    }
    if let Some(budget) = params.token_budget {
        goals::validate_budget(budget).map_err(invalid)?;
    }
    let status = params.status.map(from_wire);
    if status == Some(GoalStatus::BudgetLimited) {
        return Err(invalid(
            "a goal becomes budgetLimited when its tokens reach its budget; it can't be set so",
        ));
    }
    let stored = |e: anyhow::Error| internal(format!("couldn't store the goal: {e:#}"));
    let existing = db.get_goal(session_id).await.map_err(stored)?;
    let Some(existing) = existing else {
        let Some(objective) = objective else {
            return Err(invalid(
                "the session has no goal; give an objective to set one",
            ));
        };
        let goal = db
            .replace_goal(
                session_id,
                objective,
                status.unwrap_or(GoalStatus::Active),
                params.token_budget.flatten(),
            )
            .await
            .map_err(stored)?;
        return Ok(GoalSet {
            goal,
            objective_changed: false,
        });
    };
    let objective_changed = objective.is_some_and(|objective| objective != existing.objective);
    let update = GoalUpdate {
        objective: objective.map(str::to_string),
        status,
        token_budget: params.token_budget,
        expected_goal_id: Some(existing.goal_id),
    };
    let goal = db
        .update_goal(session_id, update)
        .await
        .map_err(stored)?
        .ok_or_else(|| invalid("the goal changed meanwhile; try again"))?;
    Ok(GoalSet {
        goal,
        objective_changed,
    })
}

/// What one turn did, as far as its goal is concerned.
#[derive(Debug, Default, Clone)]
pub(crate) struct TurnActivity {
    /// The model wrote text, thought aloud, or called a tool.
    pub active: bool,
    /// A tool call succeeded.
    pub tool_succeeded: bool,
    /// A command (`Bash`) failed.
    pub command_failed: bool,
    /// The turn ended with an error.
    pub failed: bool,
    /// The turn was interrupted.
    pub interrupted: bool,
    /// The goal evaluator found the goal not met yet, and why
    /// (`goal_judge = "evaluator"`): the next turn's guidance.
    pub check: Option<String>,
    /// What its model calls used, sub-agents included.
    pub usage: UsageTotals,
    /// How long it took.
    pub elapsed_ms: u64,
}

impl TurnActivity {
    /// Note what `event`, one of the turn's own (not a sub-agent's), says.
    pub(crate) fn observe(&mut self, event: &EngineEvent) {
        match event {
            EngineEvent::TextDelta { text } | EngineEvent::ThinkingDelta { text }
                if !text.trim().is_empty() =>
            {
                self.active = true;
            }
            EngineEvent::ToolCallStart { .. } => self.active = true,
            EngineEvent::ToolCallResult { name, outcome, .. } => {
                self.active = true;
                match outcome {
                    ToolOutcome::Success => self.tool_succeeded = true,
                    ToolOutcome::Error if name == "Bash" => self.command_failed = true,
                    _ => {}
                }
            }
            EngineEvent::TurnSummary {
                usage, elapsed_ms, ..
            } => {
                self.usage = usage.clone();
                self.elapsed_ms = *elapsed_ms;
            }
            EngineEvent::GoalJudged {
                verdict: koda_core::goal::GoalVerdict::NotYet,
                reason,
            } => self.check = reason.clone(),
            EngineEvent::TurnEnd { reason, .. } => {
                use koda_core::engine::TurnEndReason;
                self.failed = matches!(reason, TurnEndReason::Error { .. });
                self.interrupted = matches!(reason, TurnEndReason::Cancelled);
            }
            _ => {}
        }
    }
}

/// The counts that block a goal, across turns.
#[derive(Debug, Default)]
pub(crate) struct Strikes {
    /// The goal they count for.
    goal_id: Option<String>,
    /// Automatic turns in a row that did nothing.
    empty: u8,
    /// Goal turns in a row with failed commands and no tool that worked.
    failed_commands: u8,
}

impl Strikes {
    /// Start over: the user changed the goal.
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    /// Count a turn on goal `goal_id`; whether it blocks the goal.
    fn count(&mut self, goal_id: &str, automatic: bool, turn: &TurnActivity) -> bool {
        if self.goal_id.as_deref() != Some(goal_id) {
            *self = Self {
                goal_id: Some(goal_id.to_string()),
                ..Self::default()
            };
        }
        // The user stopped it, so it says nothing about the goal.
        let empty = automatic && !turn.active && !turn.interrupted;
        self.empty = if empty { self.empty + 1 } else { 0 };
        if turn.tool_succeeded {
            self.failed_commands = 0;
        } else if turn.command_failed {
            self.failed_commands += 1;
        }
        self.empty >= STRIKES || self.failed_commands >= STRIKES
    }
}

/// Why Koda stopped a goal.
pub(crate) fn blocked_why(turn: &TurnActivity) -> &'static str {
    if turn.failed {
        "the turn failed"
    } else if turn.command_failed && !turn.tool_succeeded {
        "three goal turns in a row had only failed commands"
    } else {
        "three goal turns in a row did nothing"
    }
}

/// Add a finished turn to the goal it worked on, and block the goal if the
/// turn says to. `before` is the goal as the turn started; `automatic` says
/// Koda started the turn. Returns the goal as it is now, if the turn's use
/// was added to one.
pub(crate) async fn after_turn(
    db: &Database,
    session_id: &str,
    before: Option<&Goal>,
    automatic: bool,
    turn: &TurnActivity,
    strikes: &mut Strikes,
) -> anyhow::Result<Option<Goal>> {
    let Some(now) = db.get_goal(session_id).await? else {
        strikes.reset();
        return Ok(None);
    };
    // The turn worked on the goal it started with, if that was being
    // pursued, or on one set during it.
    let worked_on = match before {
        Some(before) if before.goal_id == now.goal_id => {
            matches!(
                before.status,
                GoalStatus::Active | GoalStatus::BudgetLimited
            )
        }
        _ => true,
    };
    if !worked_on {
        return Ok(None);
    }
    let tokens = koda_core::budget::charged(&turn.usage) as i64;
    let seconds = (turn.elapsed_ms / 1000) as i64;
    let Some(goal) = db
        .account_goal(session_id, &now.goal_id, tokens, seconds)
        .await?
    else {
        return Ok(None);
    };
    if goal.status != GoalStatus::Active {
        return Ok(Some(goal));
    }
    let block = strikes.count(&goal.goal_id, automatic, turn) || turn.failed;
    if !block {
        return Ok(Some(goal));
    }
    tracing::info!(
        goal_id = %goal.goal_id,
        "blocking the goal: {}",
        blocked_why(turn)
    );
    strikes.reset();
    let update = GoalUpdate {
        status: Some(GoalStatus::Blocked),
        expected_goal_id: Some(goal.goal_id.clone()),
        ..Default::default()
    };
    Ok(db.update_goal(session_id, update).await?.or(Some(goal)))
}

/// The running turn's goal budget: what it may still spend, and whether it
/// has been told to wrap up.
#[derive(Debug)]
pub(crate) struct BudgetWatch {
    goal: Goal,
    spent: u64,
    told: bool,
}

impl BudgetWatch {
    /// Watch a turn on `goal`, if it has a budget.
    pub(crate) fn new(goal: &Goal) -> Option<Self> {
        goal.token_budget?;
        Some(Self {
            goal: goal.clone(),
            spent: 0,
            told: false,
        })
    }

    /// Charge a model call; the notice to send, once, when the goal's
    /// budget is reached.
    pub(crate) fn charge(&mut self, usage: &UsageTotals) -> Option<String> {
        self.spent += koda_core::budget::charged(usage);
        let remaining = self.goal.remaining_tokens()? as u64;
        if self.told || self.spent < remaining {
            return None;
        }
        self.told = true;
        let mut goal = self.goal.clone();
        goal.tokens_used += self.spent as i64;
        goal.status = GoalStatus::BudgetLimited;
        Some(koda_core::goal::budget_limit_message(&goal))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use koda_state::session_store::SessionStore as _;

    async fn db() -> (tempfile::TempDir, Database, String) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("koda.db")).await.unwrap();
        let id = db.create_session("koda", dir.path()).await.unwrap();
        (dir, db, id)
    }

    fn params(objective: Option<&str>) -> SessionGoalSetParams {
        SessionGoalSetParams {
            session_id: String::new(),
            objective: objective.map(str::to_string),
            ..Default::default()
        }
    }

    fn turn(active: bool) -> TurnActivity {
        TurnActivity {
            active,
            usage: UsageTotals {
                prompt_tokens: 100,
                completion_tokens: 20,
                api_calls: 1,
                ..Default::default()
            },
            elapsed_ms: 2_500,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn set_starts_a_goal_then_changes_it() {
        let (_dir, db, id) = db().await;
        let err = set(&db, &id, params(None)).await.err().unwrap();
        assert!(err.message.contains("no goal"), "{}", err.message);
        let first = set(&db, &id, params(Some("  ship it  "))).await.unwrap();
        assert_eq!(first.goal.objective, "ship it");
        assert_eq!(first.goal.status, GoalStatus::Active);
        assert!(!first.objective_changed);
        let paused = set(
            &db,
            &id,
            SessionGoalSetParams {
                status: Some(wire::GoalStatus::Paused),
                token_budget: Some(Some(1_000)),
                ..params(None)
            },
        )
        .await
        .unwrap();
        assert_eq!(paused.goal.status, GoalStatus::Paused);
        assert_eq!(paused.goal.token_budget, Some(1_000));
        assert_eq!(paused.goal.goal_id, first.goal.goal_id);
        let edited = set(&db, &id, params(Some("ship it twice"))).await.unwrap();
        assert!(edited.objective_changed);
        assert_eq!(edited.goal.status, GoalStatus::Paused, "the status is kept");
        assert!(
            set(
                &db,
                &id,
                SessionGoalSetParams {
                    status: Some(wire::GoalStatus::BudgetLimited),
                    ..params(None)
                }
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn a_turn_adds_its_use_to_the_goal_it_worked_on() {
        let (_dir, db, id) = db().await;
        let goal = db
            .replace_goal(&id, "a", GoalStatus::Active, Some(1_000))
            .await
            .unwrap();
        let mut strikes = Strikes::default();
        let now = after_turn(&db, &id, Some(&goal), false, &turn(true), &mut strikes)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((now.tokens_used, now.time_used_seconds), (120, 2));
        // A paused goal's turns aren't its own.
        let paused = db
            .update_goal(
                &id,
                GoalUpdate {
                    status: Some(GoalStatus::Paused),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert!(
            after_turn(&db, &id, Some(&paused), false, &turn(true), &mut strikes)
                .await
                .unwrap()
                .is_none()
        );
        // A goal the model created during the turn is.
        db.delete_goal(&id).await.unwrap();
        db.create_goal(&id, "b", None).await.unwrap();
        let created = after_turn(&db, &id, None, false, &turn(true), &mut strikes)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(created.tokens_used, 120);
    }

    async fn after(
        db: &Database,
        id: &str,
        goal: &Goal,
        strikes: &mut Strikes,
        automatic: bool,
        turn: TurnActivity,
    ) -> GoalStatus {
        after_turn(db, id, Some(goal), automatic, &turn, strikes)
            .await
            .unwrap()
            .unwrap()
            .status
    }

    #[tokio::test]
    async fn three_empty_automatic_turns_block_the_goal_and_a_failed_turn_at_once() {
        let (_dir, db, id) = db().await;
        let goal = db
            .replace_goal(&id, "a", GoalStatus::Active, None)
            .await
            .unwrap();
        let mut strikes = Strikes::default();
        assert_eq!(
            after(&db, &id, &goal, &mut strikes, true, turn(false)).await,
            GoalStatus::Active
        );
        assert_eq!(
            after(&db, &id, &goal, &mut strikes, true, turn(false)).await,
            GoalStatus::Active
        );
        // A user's turn, or one that did something, starts the count over.
        assert_eq!(
            after(&db, &id, &goal, &mut strikes, false, turn(false)).await,
            GoalStatus::Active
        );
        assert_eq!(
            after(&db, &id, &goal, &mut strikes, true, turn(false)).await,
            GoalStatus::Active
        );
        assert_eq!(
            after(&db, &id, &goal, &mut strikes, true, turn(false)).await,
            GoalStatus::Active
        );
        assert_eq!(
            after(&db, &id, &goal, &mut strikes, true, turn(false)).await,
            GoalStatus::Blocked
        );

        db.replace_goal(&id, "b", GoalStatus::Active, None)
            .await
            .unwrap();
        let goal = db.get_goal(&id).await.unwrap().unwrap();
        let failed = TurnActivity {
            failed: true,
            ..turn(true)
        };
        let now = after_turn(
            &db,
            &id,
            Some(&goal),
            true,
            &failed,
            &mut Strikes::default(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(now.status, GoalStatus::Blocked);
    }

    #[test]
    fn an_interrupted_turn_is_not_one_that_did_nothing() {
        let interrupted = TurnActivity {
            interrupted: true,
            ..turn(false)
        };
        let mut strikes = Strikes::default();
        for _ in 0..STRIKES + 1 {
            assert!(!strikes.count("g", true, &interrupted));
        }
        assert_eq!(strikes.empty, 0);
    }

    #[test]
    fn turns_of_failed_commands_alone_strike_and_a_working_tool_clears() {
        let failing = TurnActivity {
            command_failed: true,
            ..turn(true)
        };
        let working = TurnActivity {
            tool_succeeded: true,
            command_failed: true,
            ..turn(true)
        };
        let mut strikes = Strikes::default();
        assert!(!strikes.count("g", false, &failing));
        assert!(!strikes.count("g", true, &failing));
        assert!(!strikes.count("g", true, &working));
        assert!(!strikes.count("g", true, &failing));
        assert!(!strikes.count("g", true, &failing));
        assert!(strikes.count("g", true, &failing));
        // Another goal counts from zero.
        assert!(!strikes.count("h", true, &failing));
    }

    #[test]
    fn the_budget_notice_goes_once_when_the_budget_is_reached() {
        let goal = Goal {
            session_id: "s".into(),
            goal_id: "g".into(),
            objective: "o".into(),
            status: GoalStatus::Active,
            token_budget: Some(1_000),
            tokens_used: 800,
            time_used_seconds: 0,
            created_at: 0,
            updated_at: 0,
        };
        let call = |tokens| UsageTotals {
            prompt_tokens: tokens,
            api_calls: 1,
            ..Default::default()
        };
        let mut watch = BudgetWatch::new(&goal).unwrap();
        assert_eq!(watch.charge(&call(150)), None);
        let notice = watch.charge(&call(60)).unwrap();
        assert!(notice.contains("- Tokens used: 1010"), "{notice}");
        assert_eq!(watch.charge(&call(60)), None);
        assert!(
            BudgetWatch::new(&Goal {
                token_budget: None,
                ..goal
            })
            .is_none()
        );
    }
}
