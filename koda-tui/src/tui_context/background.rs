//! Background processes and agent definitions through Koda's host (6f).
//!
//! The overlay's process rows come from `session/backgroundTerminals/list`;
//! `/stop` (alias `/clean`, as in Codex) stops them all with
//! `session/backgroundTerminals/clean`, during a turn as between turns.
//! Esc and Ctrl+C stop only the turn. `/agents` lists the agent definitions
//! with `agent/list`.

use koda_client::RequestHandle;
use koda_client::protocol::agent::{AgentInfo, AgentListParams, AgentSource};
use koda_client::protocol::background::{
    BackgroundTerminalsCleanParams, BackgroundTerminalsListParams,
};
use koda_client::protocol::methods;
use ratatui::text::{Line, Span};

use super::TuiContext;
use crate::child_activity::ChildActivityTracker;
use crate::scroll_buffer::ScrollBuffer;
use crate::tui_output::{self, BOLD, CYAN, DIM};

impl TuiContext {
    /// List the background processes again when it's time: about once a
    /// second during a turn, or while one is listed.
    pub(crate) async fn refresh_background(&mut self, turn_running: bool) {
        if self.child_activity.list_due(turn_running) {
            list_background(
                &self.client.request_handle(),
                &self.session.id,
                &mut self.child_activity,
            )
            .await;
        }
    }
}

/// Ask the host which background processes run, for the overlay.
pub(crate) async fn list_background(
    requests: &RequestHandle,
    session_id: &str,
    tracker: &mut ChildActivityTracker,
) {
    let params = BackgroundTerminalsListParams {
        session_id: session_id.to_string(),
    };
    match requests
        .request::<methods::SessionBackgroundTerminalsList>(params)
        .await
    {
        Ok(listed) => tracker.set_background(listed.data, listed.agents),
        // Keep the last list; the next tick asks again.
        Err(e) => tracing::debug!("session/backgroundTerminals/list: {e}"),
    }
}

/// `/stop`: stop every background process the session started.
pub(crate) async fn stop_background(
    requests: &RequestHandle,
    session_id: &str,
    buffer: &mut ScrollBuffer,
    tracker: &mut ChildActivityTracker,
) {
    let params = BackgroundTerminalsCleanParams {
        session_id: session_id.to_string(),
    };
    match requests
        .request::<methods::SessionBackgroundTerminalsClean>(params)
        .await
    {
        Ok(cleaned) if cleaned.terminated == 0 => {
            tui_output::dim_msg(buffer, "Nothing is running in the background.".into());
        }
        Ok(cleaned) => {
            let n = cleaned.terminated;
            let noun = if n == 1 { "task" } else { "tasks" };
            tui_output::ok_msg(buffer, format!("Stopped {n} background {noun}."));
        }
        Err(e) => tui_output::err_msg(buffer, format!("Couldn't stop them: {e}")),
    }
    list_background(requests, session_id, tracker).await;
}

/// `/agents`: the sub-agents Koda can invoke, built in or defined in the
/// user's or the project's agent folder.
pub(crate) async fn show_agents(
    requests: &RequestHandle,
    session_id: &str,
    buffer: &mut ScrollBuffer,
) {
    let params = AgentListParams {
        session_id: session_id.to_string(),
    };
    match requests.request::<methods::AgentList>(params).await {
        Ok(listed) => render_agents(buffer, &listed.data),
        Err(e) => tui_output::err_msg(buffer, format!("Couldn't list the agents: {e}")),
    }
}

fn render_agents(buffer: &mut ScrollBuffer, agents: &[AgentInfo]) {
    tui_output::blank(buffer);
    tui_output::emit_line(buffer, Line::styled("  Sub-Agents", BOLD));
    tui_output::blank(buffer);
    if agents.is_empty() {
        tui_output::dim_msg(buffer, "No sub-agents configured.".into());
    }
    for agent in agents {
        let tag = match agent.source {
            AgentSource::User => " [user]",
            AgentSource::Project => " [project]",
            AgentSource::Builtin => "",
        };
        tui_output::emit_line(
            buffer,
            Line::from(vec![
                Span::styled(format!("  {}", agent.name), CYAN),
                Span::raw(format!(" \u{2014} {}", agent.description)),
                Span::styled(tag, DIM),
            ]),
        );
    }
    tui_output::blank(buffer);
    tui_output::dim_msg(
        buffer,
        "Ask Koda to invoke them, or use koda --agent <name>".into(),
    );
    tui_output::dim_msg(
        buffer,
        "Agent files go in .koda/agents/ or ~/.config/koda/agents/".into(),
    );
}
