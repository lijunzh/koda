//! Session commands the TUI answers itself: `/new` (and `/clear`),
//! `/status`, `/rename` and `/init`, as Codex's TUI and Claude Code have
//! them.

use super::{CommandOutcome, TuiContext, TurnInput, settings};
use crate::tui_output;
use koda_client::protocol::methods;
use ratatui::text::{Line, Span};

/// What `/init` asks the model, as Codex's `/init` and Claude Code's do:
/// write the project's instruction file for coding agents.
pub(crate) const INIT_PROMPT: &str = "Create an AGENTS.md file at the root of this project: \
a short guide for coding agents who will work here. Look at the project first: its layout, \
how to build, test and lint it, and the conventions its code and docs follow. Then write what \
an agent needs to work well here: the commands to run, how the code is organised, the \
conventions to keep, and anything that is easy to get wrong. Keep it brief and specific to \
this project, and leave out general advice. Create only that file.";

/// The instruction files `/init` won't overwrite: Koda reads either.
const INSTRUCTION_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md"];

impl TuiContext {
    /// `/new` and `/clear`: a new session on the same model, as Codex's
    /// `/new` and Claude Code's `/clear` start one. The one left stays
    /// stored, for `/resume`.
    pub(crate) async fn start_new_session(&mut self) {
        let named = settings::Named {
            provider: Some(self.settings.model_provider.clone()),
            base_url: Some(self.settings.base_url.clone()),
            model: Some(
                self.settings
                    .alias
                    .clone()
                    .unwrap_or_else(|| self.settings.model.clone()),
            ),
        };
        let opened = super::open_host_session(&self.client, None, &named).await;
        let (opened, _, _) = match opened {
            Ok(opened) => opened,
            Err(e) => {
                tui_output::err_msg(
                    &mut self.scroll_buffer,
                    format!("Couldn't start a new session: {e:#}"),
                );
                return;
            }
        };
        let left = std::mem::replace(&mut self.host_session, opened.id.clone());
        let _ = self
            .client
            .request::<methods::SessionUnsubscribe>(
                koda_client::protocol::session::SessionUnsubscribeParams { session_id: left },
            )
            .await;
        self.session = opened;
        self.renderer.forget_replies();
        self.context_left = 100;
        self.mcp_stale = true;
        self.scroll_buffer.push_lines([
            Line::default(),
            Line::styled(
                "  \u{2500}\u{2500}\u{2500} new session \u{2500}\u{2500}\u{2500}",
                tui_output::DIM,
            ),
            Line::default(),
        ]);
        // The TUI's mode carries over; the host checks it.
        if let Err(e) = self.sync_settings(true).await {
            tui_output::warn_msg(&mut self.scroll_buffer, format!("{e:#}"));
        }
    }

    /// `/status`: what the session runs with, as Codex's `/status` and
    /// Claude Code's show it.
    pub(crate) async fn show_status(&mut self) {
        use koda_client::protocol::session::SessionReadParams;
        let name = self
            .client
            .request::<methods::SessionRead>(SessionReadParams {
                session_id: self.session.id.clone(),
                include_turns: false,
            })
            .await
            .ok()
            .and_then(|read| read.session.name);
        let settings = &self.settings;
        let model = match &settings.alias {
            Some(alias) => format!("{alias} ({})", settings.model),
            None => settings.model.clone(),
        };
        let sandbox = if self.sandbox.available {
            self.sandbox.backend.clone()
        } else {
            format!("{} (unavailable)", self.sandbox.backend)
        };
        let instructions = if self.session.instruction_sources.is_empty() {
            "none".to_string()
        } else {
            self.session.instruction_sources.join(", ")
        };
        let mut rows = vec![
            ("Session", self.session.id.clone()),
            (
                "Name",
                name.unwrap_or_else(|| "(from the first prompt)".into()),
            ),
            ("Model", model),
            (
                "Provider",
                format!("{} at {}", settings.model_provider, settings.base_url),
            ),
            ("Mode", crate::mode::label(self.mode).to_string()),
            ("Sandbox", sandbox),
            (
                "Context",
                format!(
                    "{}% left of {} tokens",
                    self.context_left, settings.context_window
                ),
            ),
            ("Project", self.project_root.display().to_string()),
            ("Instructions", instructions),
            ("Koda", env!("CARGO_PKG_VERSION").to_string()),
        ];
        if let Some(effort) = &settings.effort {
            rows.insert(3, ("Effort", effort.clone()));
        }
        tui_output::emit_line(&mut self.scroll_buffer, Line::default());
        for (label, value) in rows {
            tui_output::emit_line(
                &mut self.scroll_buffer,
                Line::from(vec![
                    Span::styled(format!("  {label:<13}"), tui_output::DIM),
                    Span::raw(value),
                ]),
            );
        }
    }

    /// `/rename <name>`: name the session, as Codex's `/rename` and Claude
    /// Code's do; `/resume` lists it by that name.
    pub(crate) async fn rename_session(&mut self, name: &str) {
        use koda_client::protocol::session::SessionNameSetParams;
        let name = name.trim();
        if name.is_empty() {
            tui_output::dim_msg(
                &mut self.scroll_buffer,
                "Usage: /rename <name>. /resume lists the session by it.".into(),
            );
            return;
        }
        match self
            .client
            .request::<methods::SessionNameSet>(SessionNameSetParams {
                session_id: self.session.id.clone(),
                name: name.to_string(),
            })
            .await
        {
            Ok(_) => tui_output::ok_msg(&mut self.scroll_buffer, format!("Renamed to \"{name}\"")),
            Err(e) => tui_output::err_msg(&mut self.scroll_buffer, format!("{e}")),
        }
    }

    /// `/init`: ask the model to write `AGENTS.md`, as Codex's `/init`
    /// does, unless the project has an instruction file already.
    pub(crate) fn init_project(&mut self) -> CommandOutcome {
        if let Some(found) = INSTRUCTION_FILES
            .iter()
            .find(|file| self.project_root.join(file).exists())
        {
            tui_output::dim_msg(
                &mut self.scroll_buffer,
                format!("{found} already exists; /init leaves it as it is."),
            );
            return CommandOutcome::Handled;
        }
        CommandOutcome::StartInference(TurnInput {
            message: INIT_PROMPT.to_string(),
            images: Vec::new(),
            mentions: Vec::new(),
            typed: "/init".to_string(),
            pastes: Vec::new(),
            pasted_images: Vec::new(),
        })
    }
}
