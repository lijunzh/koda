//! Plan mode in the TUI, as Codex's and Claude Code's: `/plan` and
//! Shift+Tab enter it, the model plans with the user and proposes a plan
//! in a `<proposed_plan>` block, and the TUI then asks whether to
//! implement it in Auto or to keep planning. The host keeps Plan
//! read-only: a call that would change anything is refused.

use super::{CommandOutcome, TuiContext};
use crate::tui_types::MenuContent;
use crate::widgets::{dropdown::DropdownState, plan_menu};
use koda_client::protocol::state::TrustMode;

impl TuiContext {
    /// `/plan [prompt]`: Plan mode, then the prompt, if there is one, as
    /// Codex's `/plan` takes one.
    pub(crate) async fn enter_plan(&mut self, prompt: &str) -> CommandOutcome {
        if self.mode != TrustMode::Plan {
            if let Err(e) = self.set_mode(TrustMode::Plan).await {
                crate::tui_output::err_msg(&mut self.scroll_buffer, format!("{e:#}"));
                return CommandOutcome::Handled;
            }
            crate::tui_output::ok_msg(
                &mut self.scroll_buffer,
                "Plan mode: Koda reads and plans with you, and changes nothing. \
                 Shift+Tab leaves it."
                    .into(),
            );
        } else if prompt.is_empty() {
            crate::tui_output::dim_msg(
                &mut self.scroll_buffer,
                "Already in Plan mode. Shift+Tab leaves it.".into(),
            );
        }
        if prompt.is_empty() {
            return CommandOutcome::Handled;
        }
        self.prepare_inference_start(prompt).await
    }

    /// After a Plan turn that proposed a plan: "Implement this plan?", as
    /// Codex's TUI asks.
    pub(crate) fn offer_to_implement_plan(&mut self) {
        let mut dd = DropdownState::new(plan_menu::choices(), plan_menu::TITLE);
        dd.selected = 0;
        self.menu = MenuContent::PlanImplement(dd);
    }

    /// Leave Plan for `mode` and tell the model to implement the plan, as
    /// Codex's "Yes, implement this plan" does.
    pub(crate) async fn implement_plan(&mut self, mode: TrustMode) {
        if let Err(e) = self.set_mode(mode).await {
            crate::tui_output::err_msg(&mut self.scroll_buffer, format!("{e:#}"));
            return;
        }
        self.pending_command = Some(plan_menu::IMPLEMENT_PLAN.to_string());
    }
}
