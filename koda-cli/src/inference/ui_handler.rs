//! Engine→UI rendering during inference.
//!
//! One free function called from
//! [`super::select_loop::TuiContext::run_inference_turn`]'s
//! `SelectArm::Ui` arm (and again inside the bounded drain that
//! amortises a flood of events into one redraw).
//!
//! Variant-specific arms handle approval prompts, ask-user, and the
//! loop-cap pill. The fall-through arm is the firehose: every other
//! engine event passes through `TuiRenderer::render_to_buffer`, with a
//! mandatory tap into `ChildActivityTracker` for `ChildAgentActivity`
//! events so the bg-activity overlay stays live (#1207, #1210).

use crate::scroll_buffer::ScrollBuffer;
use crate::sink::UiEvent;
use crate::tui_types::{MenuContent, PromptMode};

use koda_core::engine::EngineEvent;
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

/// Handle a UI event during inference (field-level borrows).
pub(super) fn handle_inference_ui_inline(
    ui_event: UiEvent,
    buffer: &mut ScrollBuffer,
    menu: &mut MenuContent,
    prompt_mode: &mut PromptMode,
    renderer: &mut crate::tui_render::TuiRenderer,
    child_activity: &mut crate::child_activity::ChildActivityTracker,
) {
    match ui_event {
        UiEvent::Engine(EngineEvent::AskUserRequest {
            id,
            question,
            options,
        }) => {
            *prompt_mode = PromptMode::WizardInput {
                label: "Answer".into(),
                mask: false,
            };
            *menu = MenuContent::AskUser {
                id,
                question,
                options,
            };
        }
        UiEvent::Engine(EngineEvent::ApprovalRequest {
            id,
            tool_name,
            detail,
            preview,
            ..
        }) => {
            if preview.is_some() {
                renderer.preview_shown = true;
            }
            if let Some(ref prev) = preview {
                let diff_lines = crate::diff_render::render_lines(prev);
                let gutter = crate::diff_render::GUTTER_WIDTH;
                for line in diff_lines {
                    buffer.push_with_gutter(line, gutter);
                }
            }
            *menu = MenuContent::Approval {
                id,
                tool_name,
                detail,
            };
        }
        UiEvent::Engine(EngineEvent::LoopCapReached { cap, recent_tools }) => {
            buffer.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("\u{26a0} Hard cap reached ({cap} iterations)"),
                    Style::default().fg(Color::Yellow),
                ),
            ]));
            for name in &recent_tools {
                buffer.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(
                        format!("\u{25cf} {name}"),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
            *menu = MenuContent::LoopCap;
        }
        UiEvent::Engine(event) => {
            // Tap bg-activity events into the live tracker BEFORE the
            // renderer no-ops them (#1207 dropped scroll output for
            // ChildAgentActivity; #1210 routes them to the activity
            // overlay instead). Done here so every fall-through engine
            // event passes through the same tap point — no risk of a
            // future variant-specific arm bypassing it.
            if let EngineEvent::ChildAgentActivity { task_id, kind, .. } = &event {
                child_activity.record_activity(*task_id, kind);
            }
            renderer.render_to_buffer(event, buffer);
        }
    }
}
