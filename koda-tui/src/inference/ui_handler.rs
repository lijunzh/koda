//! The host's requests during inference become the TUI's dialogs.
//!
//! One free function called from
//! [`super::select_loop::TuiContext::run_inference_turn`] for each request
//! of the running turn: an approval (its diff drawn above the dialog), a
//! question from the model, and the loop cap. The dialog keeps the
//! request's id, and its answer goes back as the response to it
//! ([`super::TurnCommand::Answer`]).

use crate::notifications::Notification;
use crate::scroll_buffer::ScrollBuffer;
use crate::tui_types::{MenuContent, PromptMode};

use koda_client::protocol::ServerRequest;
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

/// Show `request` as its dialog (field-level borrows).
pub(super) fn show_request(
    request: &ServerRequest,
    buffer: &mut ScrollBuffer,
    menu: &mut MenuContent,
    prompt_mode: &mut PromptMode,
    renderer: &mut crate::tui_render::TuiRenderer,
) {
    match request {
        ServerRequest::UserInput { id, params } => {
            crate::notifications::notify(Notification::UserInputRequested {
                question: params.question.clone(),
            });
            *prompt_mode = PromptMode::WizardInput {
                label: "Answer".into(),
                mask: false,
            };
            *menu = MenuContent::AskUser {
                id: id.clone(),
                question: params.question.clone(),
                options: params.options.clone(),
                form: None,
            };
        }
        ServerRequest::McpServerElicitation { id, params } => {
            let form = crate::elicitation_form::ElicitationForm::new(id.clone(), params);
            crate::notifications::notify(Notification::UserInputRequested {
                question: format!("{}: {}", params.server_name, params.message),
            });
            *prompt_mode = PromptMode::WizardInput {
                label: "Answer".into(),
                mask: false,
            };
            *menu = MenuContent::AskUser {
                id: id.clone(),
                question: form.question(),
                options: form.options(),
                form: Some(Box::new(form)),
            };
        }
        ServerRequest::ToolCallApproval { id, params } => {
            crate::notifications::notify(Notification::ApprovalRequested {
                subject: format!("{}: {}", params.tool, params.detail),
            });
            renderer.approval(&params.item_id, &params.changes, buffer);
            *menu = MenuContent::Approval {
                id: id.clone(),
                tool_name: params.tool.clone(),
                detail: params.detail.clone(),
                allow_for_session: params.allow_for_session,
            };
        }
        ServerRequest::LoopCap { id, params } => {
            buffer.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("\u{26a0} Hard cap reached ({} iterations)", params.cap),
                    Style::default().fg(Color::Yellow),
                ),
            ]));
            for name in &params.recent_tools {
                buffer.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(
                        format!("\u{25cf} {name}"),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
            *menu = MenuContent::LoopCap { id: id.clone() };
            crate::notifications::notify(Notification::ApprovalRequested {
                subject: format!("Koda hit the iteration cap ({})", params.cap),
            });
        }
    }
}
