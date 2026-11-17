//! `/skills [query]` through Koda's host (6g): the skills from `skills/list`
//! that a person picks, filtered by name and description, and the skill files
//! that couldn't be loaded, as Codex's TUI reports them.

use koda_client::RequestHandle;
use koda_client::protocol::methods;
use koda_client::protocol::skills::{SkillMetadata, SkillScope, SkillsListParams};
use ratatui::text::{Line, Span};

use crate::scroll_buffer::ScrollBuffer;
use crate::tui_output::{self, BOLD, CYAN, DIM};

/// `/skills`, or `/skills <query>`.
pub(crate) async fn show_skills(
    requests: &RequestHandle,
    query: Option<&str>,
    buffer: &mut ScrollBuffer,
) {
    let listed = match requests
        .request::<methods::SkillsList>(SkillsListParams::default())
        .await
    {
        Ok(listed) => listed,
        Err(e) => {
            tui_output::err_msg(buffer, format!("Couldn't list the skills: {e}"));
            return;
        }
    };
    let query = query.map(str::trim).filter(|q| !q.is_empty());
    let (skills, errors) = listed.data.into_iter().fold(
        (Vec::new(), Vec::new()),
        |(mut skills, mut errors), entry| {
            skills.extend(entry.skills);
            errors.extend(entry.errors);
            (skills, errors)
        },
    );
    // Model-only skills (`user_invocable: false`) are the model's to pick.
    let shown: Vec<&SkillMetadata> = skills
        .iter()
        .filter(|skill| skill.user_invocable && query.is_none_or(|q| matches(skill, q)))
        .collect();

    tui_output::blank(buffer);
    tui_output::emit_line(buffer, Line::styled("  \u{1f4da} Skills", BOLD));
    tui_output::blank(buffer);
    if shown.is_empty() {
        match query {
            Some(q) => tui_output::dim_msg(buffer, format!("No skills matching '{q}'.")),
            None => tui_output::dim_msg(buffer, "No skills available.".into()),
        }
    }
    for skill in shown {
        let tag = match skill.scope {
            SkillScope::User => " [user]",
            SkillScope::Repo => " [project]",
            SkillScope::System => "",
        };
        tui_output::emit_line(
            buffer,
            Line::from(vec![
                Span::styled(format!("  {}", skill.name), CYAN),
                Span::raw(format!(" \u{2014} {}", skill.description)),
                Span::styled(tag, DIM),
            ]),
        );
    }
    if !errors.is_empty() {
        tui_output::blank(buffer);
        for error in &errors {
            tui_output::warn_msg(buffer, format!("Skipped {}: {}", error.path, error.message));
        }
    }

    tui_output::blank(buffer);
    tui_output::dim_msg(
        buffer,
        "Ask Koda to activate a skill, or use ActivateSkill tool directly.".into(),
    );
    tui_output::dim_msg(
        buffer,
        "Create your own: .koda/skills/<name>/SKILL.md or ~/.config/koda/skills/<name>/SKILL.md"
            .into(),
    );
}

/// Whether `query` appears in the skill's name or description, ignoring case.
fn matches(skill: &SkillMetadata, query: &str) -> bool {
    let query = query.to_lowercase();
    skill.name.to_lowercase().contains(&query) || skill.description.to_lowercase().contains(&query)
}
