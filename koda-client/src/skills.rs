//! `skills/list`: the skills the model gets in a folder, as Codex's request
//! lists them.

use std::path::Path;

use koda_core::skills::{SkillRegistry, SkillSource};
use koda_protocol::skills::{
    SkillErrorInfo, SkillMetadata, SkillScope, SkillsListEntry, SkillsListParams,
    SkillsListResponse,
};

/// List the skills for each folder asked about, the host's project when
/// none is: the skills a new session's agent there would find.
pub(crate) fn list(params: SkillsListParams, project_root: &Path) -> SkillsListResponse {
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
        .map(|cwd| {
            let mut registry = SkillRegistry::discover(&cwd);
            crate::builtin_skills::add_builtin_skills(&mut registry);
            entry(&cwd, &registry)
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
