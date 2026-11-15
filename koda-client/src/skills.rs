//! `skills/list`: the skills the model gets in a folder, as Codex's request
//! lists them.

use std::path::Path;

use koda_core::agent::KodaAgent;
use koda_core::skills::{SkillRegistry, SkillSource};
use koda_protocol::skills::{
    SkillErrorInfo, SkillMetadata, SkillScope, SkillsListEntry, SkillsListParams,
    SkillsListResponse,
};

/// List the skills for each folder asked about, the host's project when
/// none is. In the host's project, an agent the client built (the TUI's)
/// answers with the skills its model has; otherwise the folder's skills are
/// found as a new session's agent would find them.
pub(crate) fn list(
    params: SkillsListParams,
    project_root: &Path,
    agent: Option<&KodaAgent>,
) -> SkillsListResponse {
    let cwds = if params.cwds.is_empty() {
        vec![project_root.to_path_buf()]
    } else {
        params
            .cwds
            .iter()
            .map(|cwd| project_root.join(cwd))
            .collect()
    };
    let data = cwds
        .into_iter()
        .map(|cwd| match agent {
            Some(agent) if cwd == project_root => entry(&cwd, &agent.tools.skill_registry),
            _ => {
                let mut registry = SkillRegistry::discover(&cwd);
                crate::builtin_skills::add_builtin_skills(&mut registry);
                entry(&cwd, &registry)
            }
        })
        .collect();
    SkillsListResponse { data }
}

fn entry(cwd: &Path, registry: &SkillRegistry) -> SkillsListEntry {
    SkillsListEntry {
        cwd: cwd.display().to_string(),
        skills: registry
            .list()
            .into_iter()
            .map(|meta| SkillMetadata {
                name: meta.name.clone(),
                description: meta.description.clone(),
                path: meta.path.as_ref().map(|p| p.display().to_string()),
                scope: match meta.source {
                    SkillSource::BuiltIn => SkillScope::System,
                    SkillSource::User => SkillScope::User,
                    SkillSource::Project => SkillScope::Repo,
                },
                enabled: true,
                user_invocable: meta.user_invocable,
            })
            .collect(),
        errors: registry
            .errors()
            .iter()
            .map(|error| SkillErrorInfo {
                path: error.path.display().to_string(),
                message: error.message.clone(),
            })
            .collect(),
    }
}
