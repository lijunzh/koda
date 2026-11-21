//! Session commands the TUI answers itself: `/new` (and `/clear`),
//! `/fork`, `/status`, `/rename` and `/init`, as Codex's TUI and Claude
//! Code have them.

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

    /// `/fork`: a copy of this session, which the TUI switches to, as
    /// Codex's `/fork` does. The one left stays stored, for `/resume`.
    pub(crate) async fn fork_session(&mut self) {
        let source = self.session.id.clone();
        self.fork_from(&source, false).await;
    }

    /// Fork `source` on the host (`session/fork`) and switch to the copy.
    /// With `draw`, its earlier turns are drawn, as when the sessions
    /// picker or `koda fork` opens one; after `/fork` they're on screen.
    pub(crate) async fn fork_from(&mut self, source: &str, draw: bool) {
        use koda_client::protocol::session::SessionForkParams;
        let forked = self
            .client
            .request::<methods::SessionFork>(SessionForkParams {
                session_id: source.to_string(),
                ..Default::default()
            })
            .await;
        let forked = match forked {
            Ok(forked) => forked.session,
            Err(e) => {
                tui_output::err_msg(
                    &mut self.scroll_buffer,
                    format!("Couldn't fork the session: {e}"),
                );
                return;
            }
        };
        let left = std::mem::replace(&mut self.host_session, forked.id.clone());
        if left != forked.id {
            let _ = self
                .client
                .request::<methods::SessionUnsubscribe>(
                    koda_client::protocol::session::SessionUnsubscribeParams { session_id: left },
                )
                .await;
        }
        self.session = super::TuiSession {
            id: forked.id.clone(),
            instruction_sources: forked.instruction_sources.clone().unwrap_or_default(),
        };
        if let Some(mode) = forked.mode {
            self.mode = mode;
        }
        self.mcp_stale = true;
        if draw {
            self.renderer.forget_replies();
            super::replay_turns(&mut self.renderer, &forked.turns, &mut self.scroll_buffer);
        }
        let name = forked.name.unwrap_or_else(|| "Fork".to_string());
        let short = &forked.id[..8.min(forked.id.len())];
        tui_output::ok_msg(
            &mut self.scroll_buffer,
            format!("Forked into \"{name}\" ({short}). /resume goes back to the original."),
        );
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

    /// `/rewind [N]`: take the last turn, or the last `N`, out of the
    /// conversation, as Codex's backtrack (Esc Esc) and Claude Code's
    /// `/rewind` do; the earliest one's prompt comes back to the input box
    /// to edit and send again. Files the turns changed stay as they are.
    pub(crate) async fn rewind_session(&mut self, count: &str) {
        use koda_client::protocol::item::Item;
        use koda_client::protocol::session::{
            SessionRewindParams, SessionTurnsListParams, SortDirection,
        };
        let count = count.trim();
        let n: u32 = if count.is_empty() {
            1
        } else {
            match count.parse() {
                Ok(n) if (1..=100).contains(&n) => n,
                _ => {
                    tui_output::dim_msg(
                        &mut self.scroll_buffer,
                        "Usage: /rewind [N]: take the last turn, or the last N (up to 100), \
                         out of the conversation."
                            .into(),
                    );
                    return;
                }
            }
        };
        let turns = match self
            .client
            .request::<methods::SessionTurnsList>(SessionTurnsListParams {
                session_id: self.session.id.clone(),
                cursor: None,
                limit: Some(n),
                sort_direction: Some(SortDirection::Desc),
            })
            .await
        {
            Ok(turns) => turns.data,
            Err(e) => {
                tui_output::err_msg(&mut self.scroll_buffer, format!("{e}"));
                return;
            }
        };
        let Some(target) = turns.get(n as usize - 1) else {
            tui_output::dim_msg(
                &mut self.scroll_buffer,
                format!(
                    "This session has {} turn{}; nothing to rewind that far.",
                    turns.len(),
                    if turns.len() == 1 { "" } else { "s" }
                ),
            );
            return;
        };
        let prompt = target.items.iter().find_map(|item| match &item.item {
            Item::UserMessage { text, .. } => Some(text.clone()),
            _ => None,
        });
        if let Err(e) = self
            .client
            .request::<methods::SessionRewind>(SessionRewindParams {
                session_id: self.session.id.clone(),
                before_turn_id: target.id.clone(),
            })
            .await
        {
            tui_output::err_msg(&mut self.scroll_buffer, format!("{e}"));
            return;
        }
        let what = if n == 1 {
            "the last turn".to_string()
        } else {
            format!("the last {n} turns")
        };
        tui_output::ok_msg(
            &mut self.scroll_buffer,
            format!("Took {what} out of the conversation. Files they changed are as they were."),
        );
        if let Some(prompt) = prompt.filter(|_| self.textarea.text().trim().is_empty()) {
            self.textarea.set_text_clearing_elements("");
            self.textarea.insert_str(&prompt);
            tui_output::dim_msg(
                &mut self.scroll_buffer,
                "Its prompt is back in the input box: edit it, or send it again.".into(),
            );
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
