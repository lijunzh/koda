//! A session's goal, as the model is told about it: Codex's goal prompts
//! (`codex-rs/ext/goal`).
//!
//! The goal itself is stored with the session ([`koda_state::db::goals`]);
//! the host runs the loop that keeps it going. This module has what the
//! model reads: the text of each continuation turn, the notice that the
//! budget is used up, and the notice that the user changed the objective.
//!
//! `continuation.md`, `budget_limit.md` and `objective_updated.md` are
//! Codex's `ext/goal/templates/goals/*.md`, Apache License 2.0 (see
//! `NOTICE`), with Koda's tool names put in when they are rendered:
//! `GetGoal` and `UpdateGoal` for `get_goal` and `update_goal`, and
//! `TodoWrite` for `update_plan`.
//!
//! With `goal_judge = "evaluator"`, Claude Code's way, a check after each
//! turn decides (`goal_judge.rs`). Its question (`evaluator.md`) and
//! the part of the continuation that replaces Codex's audits
//! (`evaluated_completion.md`) are Koda's own: Claude Code's prompts aren't
//! published. They follow its documented behaviour: the check reads the
//! conversation, can't run anything, and answers `{ok, reason,
//! impossible}`.

pub use koda_state::db::goals::{Goal, GoalStatus};

const CONTINUATION: &str = include_str!("goal_prompts/continuation.md");
const BUDGET_LIMIT: &str = include_str!("goal_prompts/budget_limit.md");
const OBJECTIVE_UPDATED: &str = include_str!("goal_prompts/objective_updated.md");
const EVALUATOR: &str = include_str!("goal_prompts/evaluator.md");
const EVALUATED_COMPLETION: &str = include_str!("goal_prompts/evaluated_completion.md");

/// Where Codex's continuation turns to the model's own completion and
/// blocked audits, which a goal under the evaluator leaves out.
const AUDITS: &str = "Completion audit:";

/// What the evaluator found after a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalVerdict {
    /// The goal holds.
    Met,
    /// Not yet: work remains, or the evidence is missing.
    NotYet,
    /// It can never be satisfied.
    Impossible,
}

/// The model's goal tools, by name.
pub const GOAL_TOOL_NAMES: [&str; 3] = ["GetGoal", "CreateGoal", "UpdateGoal"];

/// What starts each turn Koda runs on an active goal by itself: Codex's
/// continuation prompt, with the objective after it as user-provided data.
pub fn continuation_message(goal: &Goal) -> String {
    let text = render(
        CONTINUATION,
        &[
            ("tokens_used", goal.tokens_used.to_string()),
            ("token_budget", budget(goal)),
            (
                "remaining_tokens",
                goal.remaining_tokens()
                    .map_or_else(|| "unbounded".to_string(), |left| left.to_string()),
            ),
        ],
    );
    format!(
        "{text}\n<external_goal_objective>\n{}\n</external_goal_objective>",
        escape(&goal.objective)
    )
}

/// What starts each turn Koda runs by itself on a goal the evaluator judges:
/// Codex's continuation without its completion and blocked audits, which
/// are the model's own, then what the evaluator wants instead and its last
/// finding (`check`), and the objective as user-provided data.
pub fn evaluated_continuation_message(goal: &Goal, check: Option<&str>) -> String {
    let codex = continuation_message(goal);
    let working = codex
        .split_once(AUDITS)
        .map_or(codex.as_str(), |(working, _)| working)
        .replace(
            " and leave the goal active until the blocked audit threshold is met",
            " and leave the goal active",
        );
    let check = check.map_or_else(String::new, |reason| {
        format!(
            "\nThe last check found the goal not achieved yet:\n<goal_check>\n{}\n</goal_check>\n",
            escape(reason)
        )
    });
    format!(
        "{}\n\n{EVALUATED_COMPLETION}{check}\n<external_goal_objective>\n{}\n</external_goal_objective>",
        working.trim_end(),
        escape(&goal.objective)
    )
}

/// The evaluator's question, asked after the turn's own conversation.
pub fn evaluator_message(goal: &Goal) -> String {
    render(EVALUATOR, &[("objective", escape(&goal.objective))])
}

/// Read the evaluator's answer: `{"ok", "reason", "impossible"}`, as
/// Claude Code's prompt hooks answer, found anywhere in the reply. `None`
/// when there is no such object.
pub fn parse_verdict(reply: &str) -> Option<(GoalVerdict, Option<String>)> {
    #[derive(serde::Deserialize)]
    struct Answer {
        ok: bool,
        #[serde(default)]
        reason: Option<String>,
        #[serde(default)]
        impossible: bool,
    }
    // The last object that reads as an answer: a model may think aloud, or
    // quote the format, before it.
    let answer = reply.match_indices('{').rev().find_map(|(start, _)| {
        let mut stream = serde_json::Deserializer::from_str(&reply[start..]).into_iter::<Answer>();
        stream.next()?.ok()
    })?;
    let verdict = match (answer.ok, answer.impossible) {
        (true, _) => GoalVerdict::Met,
        (false, true) => GoalVerdict::Impossible,
        (false, false) => GoalVerdict::NotYet,
    };
    let reason = answer
        .reason
        .map(|reason| reason.trim().to_string())
        .filter(|reason| !reason.is_empty());
    Some((verdict, reason))
}

/// What a turn is told once its goal's budget is used up: wrap up.
pub fn budget_limit_message(goal: &Goal) -> String {
    render(
        BUDGET_LIMIT,
        &[
            ("objective", escape(&goal.objective)),
            ("time_used_seconds", goal.time_used_seconds.to_string()),
            ("tokens_used", goal.tokens_used.to_string()),
            ("token_budget", budget(goal)),
        ],
    )
}

/// What a turn is told when the user changes the objective during it.
pub fn objective_updated_message(goal: &Goal) -> String {
    render(
        OBJECTIVE_UPDATED,
        &[
            ("objective", escape(&goal.objective)),
            ("tokens_used", goal.tokens_used.to_string()),
            ("token_budget", budget(goal)),
            (
                "remaining_tokens",
                goal.remaining_tokens()
                    .map_or_else(|| "unknown".to_string(), |left| left.to_string()),
            ),
        ],
    )
}

fn budget(goal: &Goal) -> String {
    goal.token_budget
        .map_or_else(|| "none".to_string(), |budget| budget.to_string())
}

/// Fill a template's `{{ name }}` slots, and put in Koda's tool names.
fn render(template: &str, values: &[(&str, String)]) -> String {
    let mut text = template
        .replace("update_goal", "UpdateGoal")
        .replace("get_goal", "GetGoal")
        .replace("update_plan", "TodoWrite");
    for (name, value) in values {
        text = text.replace(&format!("{{{{ {name} }}}}"), value);
    }
    text
}

/// Keep the objective from closing the tag it sits in.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn goal(budget: Option<i64>, used: i64) -> Goal {
        Goal {
            session_id: "s".into(),
            goal_id: "g".into(),
            objective: "Fix <all> the tests & ship".into(),
            status: GoalStatus::Active,
            token_budget: budget,
            tokens_used: used,
            time_used_seconds: 42,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn every_slot_is_filled_and_the_tools_have_koda_names() {
        for text in [
            continuation_message(&goal(Some(1_000), 250)),
            continuation_message(&goal(None, 0)),
            budget_limit_message(&goal(Some(1_000), 1_000)),
            objective_updated_message(&goal(None, 5)),
        ] {
            assert!(!text.contains("{{"), "{text}");
            assert!(!text.contains("update_goal"), "{text}");
            assert!(!text.contains("get_goal"), "{text}");
            assert!(!text.contains("update_plan"), "{text}");
        }
    }

    #[test]
    fn the_continuation_counts_the_budget_and_carries_the_objective_as_data() {
        let text = continuation_message(&goal(Some(1_000), 250));
        assert!(text.contains("- Tokens remaining: 750"), "{text}");
        assert!(text.contains("call UpdateGoal with status \"complete\""));
        assert!(text.ends_with(
            "<external_goal_objective>\nFix &lt;all&gt; the tests &amp; ship\n</external_goal_objective>"
        ));
        let unbounded = continuation_message(&goal(None, 0));
        assert!(unbounded.contains("- Token budget: none"), "{unbounded}");
        assert!(unbounded.contains("- Tokens remaining: unbounded"));
    }

    #[test]
    fn under_the_evaluator_the_continuation_drops_the_models_audits() {
        let text =
            evaluated_continuation_message(&goal(Some(1_000), 250), Some("Run <the> tests."));
        assert!(text.starts_with("Continue working toward the active thread goal."));
        assert!(text.contains("- Tokens remaining: 750"));
        assert!(!text.contains("UpdateGoal"), "{text}");
        assert!(!text.contains("Completion audit"), "{text}");
        assert!(!text.contains("blocked audit"), "{text}");
        assert!(text.contains("a separate check reads this conversation"));
        assert!(text.contains("<goal_check>\nRun &lt;the&gt; tests.\n</goal_check>"));
        assert!(text.ends_with("</external_goal_objective>"));
        let first = evaluated_continuation_message(&goal(None, 0), None);
        assert!(!first.contains("<goal_check>"), "{first}");
    }

    #[test]
    fn the_evaluator_is_asked_about_the_objective_as_data() {
        let text = evaluator_message(&goal(None, 0));
        assert!(text.contains("Do not call tools."));
        assert!(
            text.contains(
                "<goal_condition>\nFix &lt;all&gt; the tests &amp; ship\n</goal_condition>"
            )
        );
        assert!(!text.contains("{{"));
    }

    #[test]
    fn a_verdict_is_read_from_the_last_answer_in_the_reply() {
        assert_eq!(
            parse_verdict(r#"{"ok": true, "reason": "All tests pass."}"#),
            Some((GoalVerdict::Met, Some("All tests pass.".into())))
        );
        assert_eq!(
            parse_verdict(
                "The format is {\"ok\": true}. Checking...\n```json\n{\"ok\": false, \"reason\": \"lint fails\", \"impossible\": false}\n```"
            ),
            Some((GoalVerdict::NotYet, Some("lint fails".into())))
        );
        assert_eq!(
            parse_verdict(r#"{"ok": false, "reason": " ", "impossible": true}"#),
            Some((GoalVerdict::Impossible, None))
        );
        assert_eq!(parse_verdict("It looks done to me."), None);
        assert_eq!(parse_verdict(r#"{"done": true}"#), None);
    }

    #[test]
    fn the_notices_quote_the_objective_inside_their_tags() {
        let budget = budget_limit_message(&goal(Some(1_000), 1_000));
        assert!(budget.contains("<objective>\nFix &lt;all&gt; the tests &amp; ship\n</objective>"));
        assert!(budget.contains("Time spent pursuing goal: 42 seconds"));
        let updated = objective_updated_message(&goal(None, 5));
        assert!(updated.contains("<untrusted_objective>\nFix &lt;all&gt;"));
        assert!(updated.contains("- Tokens remaining: unknown"));
    }
}
