//! Skills: `SKILL.md` files that give the model expertise on request.
//! `skills/list` is Codex's, in its shape, with the fields Koda can fill.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `skills/list`. Codex's also takes `forceReload`; Koda finds the model's
/// skills once, when its agent is built, so a reload would list skills the
/// model can't use.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillsListParams {
    /// The folders to list skills for; without any, the host's project.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cwds: Vec<String>,
}

/// The answer to `skills/list`: one entry per folder asked about.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkillsListResponse {
    /// The entries, in the order asked.
    pub data: Vec<SkillsListEntry>,
}

/// The skills for one folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkillsListEntry {
    /// The folder.
    pub cwd: String,
    /// Its skills, by name.
    pub skills: Vec<SkillMetadata>,
    /// The skill files found there that couldn't be loaded.
    pub errors: Vec<SkillErrorInfo>,
}

/// Where a skill comes from, in Codex's names. A project's overrides a
/// user's, which overrides a built-in one of the same name. Codex also has
/// `admin`, for skills an administrator installs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum SkillScope {
    /// The user's `<config>/skills/`.
    User,
    /// The project's `.koda/skills/`.
    Repo,
    /// Koda's own.
    System,
}

/// One skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkillMetadata {
    /// The name `ActivateSkill` takes.
    pub name: String,
    /// What it's for, in one line.
    pub description: String,
    /// Its `SKILL.md`. Optional in Koda: a built-in skill has no file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Where it comes from.
    pub scope: SkillScope,
    /// Whether the model may use it. Always true: Koda can't disable a
    /// skill yet.
    pub enabled: bool,
    /// Whether a person picks it, as opposed to only the model
    /// (`user_invocable: false` in its frontmatter). Koda's own; Codex has
    /// none.
    pub user_invocable: bool,
}

/// A skill file that couldn't be loaded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkillErrorInfo {
    /// The file.
    pub path: String,
    /// What's wrong with it.
    pub message: String,
}
