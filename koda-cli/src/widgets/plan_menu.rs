//! The choice offered when a Plan turn ends with a proposed plan, as
//! Codex's TUI asks "Implement this plan?": implement it in Safe or in
//! Auto, or keep planning.

use super::dropdown::DropdownItem;
use koda_client::protocol::state::TrustMode;

/// What the model is sent to implement the plan, Codex's words.
pub const IMPLEMENT_PLAN: &str = "Implement the plan.";

/// The question above the choices, Codex's words.
pub const TITLE: &str = "Implement this plan?";

/// One choice: the mode to implement the plan in, or `None` to keep
/// planning.
#[derive(Clone, Debug)]
pub struct PlanChoice {
    pub label: &'static str,
    pub description: &'static str,
    pub mode: Option<TrustMode>,
}

impl DropdownItem for PlanChoice {
    fn label(&self) -> &str {
        self.label
    }
    fn description(&self) -> String {
        self.description.to_string()
    }
    fn matches_filter(&self, filter: &str) -> bool {
        self.label.to_lowercase().contains(&filter.to_lowercase())
    }
}

/// The choices: Safe, Auto when the sandbox makes it available, then
/// staying in Plan.
pub fn choices(auto_available: bool) -> Vec<PlanChoice> {
    let mut choices = vec![PlanChoice {
        label: "Yes, implement it in Safe",
        description: "Asks before each change",
        mode: Some(TrustMode::Safe),
    }];
    if auto_available {
        choices.push(PlanChoice {
            label: "Yes, implement it in Auto",
            description: "Changes the project without asking, in the sandbox",
            mode: Some(TrustMode::Auto),
        });
    }
    choices.push(PlanChoice {
        label: "No, keep planning",
        description: "Stay in Plan mode and reply to the model",
        mode: None,
    });
    choices
}

/// Whether a reply proposes a plan: a `<proposed_plan>` tag on a line of
/// its own, as the Plan turn's instructions ask and Codex's TUI reads it.
pub fn proposes_plan(reply: &str) -> bool {
    reply.lines().any(|line| line.trim() == "<proposed_plan>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_is_offered_only_with_the_sandbox() {
        let modes = |auto| choices(auto).iter().map(|c| c.mode).collect::<Vec<_>>();
        assert_eq!(
            modes(true),
            [Some(TrustMode::Safe), Some(TrustMode::Auto), None]
        );
        assert_eq!(modes(false), [Some(TrustMode::Safe), None]);
    }

    #[test]
    fn a_plan_is_its_tag_on_a_line_of_its_own() {
        assert!(proposes_plan(
            "Here it is.\n\n<proposed_plan>\n# Plan\n</proposed_plan>\n"
        ));
        assert!(proposes_plan("  <proposed_plan>  \nx\n"));
        assert!(!proposes_plan("I'll wrap it in a `<proposed_plan>` block."));
        assert!(!proposes_plan("No plan yet: which database?"));
    }
}
