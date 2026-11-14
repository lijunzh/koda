//! `/memory` through Koda's host (6e): the instruction files the session's
//! prompt carries, from `session/start`'s `instructionSources`, and Koda's
//! notes, read and saved with `session/memory/read` and
//! `session/memory/add`.

use koda_client::protocol::memory::{MemoryScope, SessionMemoryAddParams, SessionMemoryReadParams};
use koda_client::protocol::methods;
use ratatui::text::{Line, Span};

use super::TuiContext;
use crate::tui_output::{self, BOLD, CYAN};

impl TuiContext {
    /// `/memory`, `/memory add <text>` or `/memory global <text>`.
    pub(crate) async fn memory_command(&mut self, arg: Option<&str>) {
        let save = |rest: &str, scope| (scope, rest.trim().to_string());
        let saving = match arg {
            Some(text) if text.starts_with("global ") || text == "global" => {
                Some(save(&text["global".len()..], MemoryScope::Global))
            }
            Some(text) if text.starts_with("add ") || text == "add" => {
                Some(save(&text["add".len()..], MemoryScope::Project))
            }
            _ => None,
        };
        match saving {
            Some((scope, text)) => self.save_note(scope, text).await,
            None => self.show_memory().await,
        }
    }

    async fn save_note(&mut self, scope: MemoryScope, text: String) {
        let (usage, saved) = match scope {
            MemoryScope::Project => (
                "Usage: /memory add <text>",
                "Saved to Koda's notes for this project",
            ),
            MemoryScope::Global => (
                "Usage: /memory global <text>",
                "Saved to Koda's global notes",
            ),
        };
        if text.is_empty() {
            tui_output::warn_msg(&mut self.scroll_buffer, usage.into());
            return;
        }
        let added = self
            .client
            .request::<methods::SessionMemoryAdd>(SessionMemoryAddParams {
                session_id: self.host_session.clone(),
                scope,
                text,
            })
            .await;
        match added {
            Ok(_) => tui_output::ok_msg(&mut self.scroll_buffer, saved.into()),
            Err(e) => tui_output::err_msg(&mut self.scroll_buffer, format!("Error: {e}")),
        }
    }

    async fn show_memory(&mut self) {
        let notes = self
            .client
            .request::<methods::SessionMemoryRead>(SessionMemoryReadParams {
                session_id: self.host_session.clone(),
            })
            .await;
        let buffer = &mut self.scroll_buffer;
        tui_output::blank(buffer);
        tui_output::emit_line(buffer, Line::styled("  \u{1f43b} Memory", BOLD));
        tui_output::blank(buffer);
        let sources = &self.session.instruction_sources;
        if sources.is_empty() {
            tui_output::dim_msg(
                buffer,
                "Instructions: (none \u{2014} add an AGENTS.md to the project)".into(),
            );
        }
        for (i, source) in sources.iter().enumerate() {
            // A project file by its path in the project, as Codex's
            // `/status` lists them; anything else in full.
            let shown = std::path::Path::new(source)
                .strip_prefix(&self.project_root)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| source.clone());
            let label = if i == 0 {
                "  Instructions: "
            } else {
                "                "
            };
            tui_output::emit_line(
                buffer,
                Line::from(vec![Span::raw(label), Span::styled(shown, CYAN)]),
            );
        }
        match notes {
            Ok(read) => {
                for note in read.notes {
                    let label = match note.scope {
                        MemoryScope::Project => "  Notes:        ",
                        MemoryScope::Global => "  Global notes: ",
                    };
                    tui_output::emit_line(
                        buffer,
                        Line::from(vec![Span::raw(label), Span::styled(note.path, CYAN)]),
                    );
                }
            }
            Err(e) => tui_output::err_msg(buffer, format!("Koda's notes: {e}")),
        }
        tui_output::blank(buffer);
        tui_output::dim_msg(buffer, "Commands:".into());
        tui_output::dim_msg(
            buffer,
            "  /memory add <text>      Save to Koda's notes for this project".into(),
        );
        tui_output::dim_msg(
            buffer,
            "  /memory global <text>   Save to Koda's global notes".into(),
        );
        tui_output::blank(buffer);
        tui_output::dim_msg(
            buffer,
            "Tip: the LLM can also call MemoryWrite to save insights automatically.".into(),
        );
    }
}
