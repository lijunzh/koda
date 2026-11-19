//! A session's goal, as the model is told about it: Codex's goal prompts
//! (`codex-rs/ext/goal`).
//!
//! The goal itself is stored with the session ([`koda_state::db::goals`]);
//! the host runs the loop that keeps it going. This module has what the
//! model reads: the text of each continuation turn, the notice that the
//! budget is used up, and the notice that the user changed the objective.
//!
//! `goal_prompts/*.md` are Codex's `ext/goal/templates/goals/*.md`, Apache
//! License 2.0 (see `NOTICE`), with Koda's tool names put in when they are
//! rendered: `GetGoal` and `UpdateGoal` for `get_goal` and `update_goal`,
//! and `TodoWrite` for `update_plan`.

pub use koda_state::db::goals::{Goal, GoalStatus};

const CONTINUATION: &str = include_str!("goal_prompts/continuation.md");
const BUDGET_LIMIT: &str = include_str!("goal_prompts/budget_limit.md");
const OBJECTIVE_UPDATED: &str = include_str!("goal_prompts/objective_updated.md");

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
    fn the_notices_quote_the_objective_inside_their_tags() {
        let budget = budget_limit_message(&goal(Some(1_000), 1_000));
        assert!(budget.contains("<objective>\nFix &lt;all&gt; the tests &amp; ship\n</objective>"));
        assert!(budget.contains("Time spent pursuing goal: 42 seconds"));
        let updated = objective_updated_message(&goal(None, 5));
        assert!(updated.contains("<untrusted_objective>\nFix &lt;all&gt;"));
        assert!(updated.contains("- Tokens remaining: unknown"));
    }
}
