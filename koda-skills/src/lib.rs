//! Skill discovery and loading.
//!
//! Split out of `koda-core` in 0.7.1, which re-exports this crate as
//! `koda_core::skills`. This crate follows koda-cli's version, and its Rust
//! API makes no semver promise of its own: it is published for
//! `cargo install koda-cli`, not as a library to build on.
//!
//! Skills are `SKILL.md` files with YAML frontmatter that inject expertise
//! into the agent's context. Unlike sub-agents, skills don't spawn a
//! separate inference loop — they're prompt injection, zero extra LLM cost.
//!
//! ## Skill file format
//!
//! ```markdown
//! ---
//! name: code-review
//! description: Expert code review with security focus
//! tags: [review, security]
//! ---
//!
//! You are a code review expert. When reviewing code:
//! 1. Check for security vulnerabilities
//! 2. Verify error handling
//! 3. Assess test coverage
//! ```
//!
//! ## Built-in skills
//!
//! - **code-review** — structured code review with security focus
//! - **security-audit** — OWASP-aligned security analysis
//!
//! ## Custom skills
//!
//! - **Project**: `.koda/skills/<name>/SKILL.md`
//! - **Global**: `~/.config/koda/skills/<name>/SKILL.md`
//!
//! Use `/skills` to browse, or ask Koda to "use the code review skill."
//!
//! Discovery order (later overrides earlier):
//! 1. Built-in skills (embedded in the binary)
//! 2. User-global skills (`~/.config/koda/skills/`)
//! 3. Project-local skills (`.koda/skills/`)

use std::collections::HashMap;
use std::path::Path;

/// Metadata from a SKILL.md frontmatter.
///
/// ## Parity with Claude Code
///
/// These fields map to Claude Code's `FrontmatterData`:
///
/// | CC field | Koda field | Notes |
/// |---|---|---|
/// | `description` | `description` | |
/// | `when_to_use` | `when_to_use` | |
/// | `allowed-tools` | `allowed_tools` | Scoped tool access |
/// | `user-invocable` | `user_invocable` | Default: `true` |
/// | `argument-hint` | `argument_hint` | Usage guidance |
#[derive(Debug, Clone)]
pub struct SkillMeta {
    /// Skill name (derived from filename or frontmatter).
    pub name: String,
    /// One-line description.
    pub description: String,
    /// Searchable tags.
    pub tags: Vec<String>,
    /// Guidance for the model on when to activate this skill.
    /// Surfaced in `ListSkills` output so the model can decide without
    /// hard-coded hints in `instructions.md`.
    pub when_to_use: Option<String>,
    /// Tool names allowed when this skill is active.
    /// Empty = all tools available (default). Non-empty = only these tools.
    /// Mirrors CC's `allowed-tools` frontmatter field.
    pub allowed_tools: Vec<String>,
    /// Whether users can invoke this skill (e.g. via `/skill-name`).
    /// `true` (default) = shown in user-facing skill list.
    /// `false` = model-only, not surfaced in `/skills` but still activatable.
    /// Mirrors CC's `user-invocable` frontmatter field.
    pub user_invocable: bool,
    /// Usage hint shown when listing the skill (e.g. `"<file_path>"`)
    /// Mirrors CC's `argument-hint` frontmatter field.
    pub argument_hint: Option<String>,
    /// Where this skill was discovered.
    pub source: SkillSource,
    /// Its `SKILL.md`; `None` for a built-in skill, which has no file.
    pub path: Option<std::path::PathBuf>,
}

/// A `SKILL.md` that was found but couldn't be loaded, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillError {
    /// The file.
    pub path: std::path::PathBuf,
    /// What's wrong with it.
    pub message: String,
}

/// Where a skill was loaded from.
#[derive(Debug, Clone)]
pub enum SkillSource {
    /// Shipped with koda.
    BuiltIn,
    /// From `~/.config/koda/skills/`.
    User,
    /// From `.koda/skills/` in the project.
    Project,
}

/// A fully loaded skill (metadata + content).
#[derive(Debug, Clone)]
pub struct Skill {
    /// Skill metadata (name, description, tags, source).
    pub meta: SkillMeta,
    /// The full SKILL.md content (after frontmatter).
    pub content: String,
}

/// Registry of discovered skills.
#[derive(Debug, Default)]
pub struct SkillRegistry {
    pub(crate) skills: HashMap<String, Skill>,
    /// The skill files found that couldn't be loaded.
    errors: Vec<SkillError>,
}

impl SkillRegistry {
    /// Discover skills from all standard locations.
    pub fn discover(project_root: &Path) -> Self {
        let mut registry = Self::default();

        // 1. Built-in skills (embedded at compile time)
        registry.load_builtin();

        // 2. User-global skills
        if let Ok(config_dir) = koda_config::config_dir() {
            let user_dir = config_dir.join("skills");
            registry.load_directory(&user_dir, SkillSource::User);
        }

        // 3. Project-local skills
        let project_dir = project_root.join(".koda").join("skills");
        registry.load_directory(&project_dir, SkillSource::Project);

        registry
    }

    /// Load built-in skills embedded at compile time.
    fn load_builtin(&mut self) {
        let builtins: &[(&str, &str)] = &[
            (
                "code-review",
                include_str!("../skills/code-review/SKILL.md"),
            ),
            (
                "security-audit",
                include_str!("../skills/security-audit/SKILL.md"),
            ),
            ("simplify", include_str!("../skills/simplify/SKILL.md")),
            ("debug", include_str!("../skills/debug/SKILL.md")),
            ("remember", include_str!("../skills/remember/SKILL.md")),
            (
                "create-agent",
                include_str!("../skills/create-agent/SKILL.md"),
            ),
            (
                "create-skill",
                include_str!("../skills/create-skill/SKILL.md"),
            ),
        ];

        for (name, content) in builtins {
            if let Ok(skill) = parse_skill_md(content, SkillSource::BuiltIn) {
                self.skills.insert(name.to_string(), skill);
            }
        }
    }

    /// Load skills from a directory (each subdirectory with a SKILL.md).
    fn load_directory(&mut self, dir: &Path, source: SkillSource) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            // A folder without a SKILL.md isn't a skill.
            let skill_file = entry.path().join("SKILL.md");
            if !skill_file.is_file() {
                continue;
            }
            let loaded = std::fs::read_to_string(&skill_file)
                .map_err(|e| format!("couldn't read it: {e}"))
                .and_then(|content| parse_skill_md(&content, source.clone()));
            match loaded {
                Ok(mut skill) => {
                    skill.meta.path = Some(skill_file);
                    self.skills.insert(skill.meta.name.clone(), skill);
                }
                Err(message) => {
                    tracing::warn!("skipped skill {}: {message}", skill_file.display());
                    self.errors.push(SkillError {
                        path: skill_file,
                        message,
                    });
                }
            }
        }
    }

    /// The skill files found that couldn't be loaded, and why.
    pub fn errors(&self) -> &[SkillError] {
        &self.errors
    }

    /// List all discovered skills (name + description).
    pub fn list(&self) -> Vec<&SkillMeta> {
        let mut metas: Vec<&SkillMeta> = self.skills.values().map(|s| &s.meta).collect();
        metas.sort_by_key(|m| &m.name);
        metas
    }

    /// List only user-invocable skills (excludes model-only skills).
    ///
    /// Used by the `/skills` REPL command and `ListSkills` when called
    /// by the user (vs. the model discovering skills autonomously).
    pub fn list_user_invocable(&self) -> Vec<&SkillMeta> {
        let mut metas: Vec<&SkillMeta> = self
            .skills
            .values()
            .filter(|s| s.meta.user_invocable)
            .map(|s| &s.meta)
            .collect();
        metas.sort_by_key(|m| &m.name);
        metas
    }

    /// Search skills by query (matches name, description, tags).
    pub fn search(&self, query: &str) -> Vec<&SkillMeta> {
        let q = query.to_lowercase();
        let mut results: Vec<&SkillMeta> = self
            .skills
            .values()
            .filter(|s| {
                s.meta.name.to_lowercase().contains(&q)
                    || s.meta.description.to_lowercase().contains(&q)
                    || s.meta.tags.iter().any(|t| t.to_lowercase().contains(&q))
            })
            .map(|s| &s.meta)
            .collect();
        results.sort_by_key(|m| &m.name);
        results
    }

    /// Activate a skill by name — returns the full content for context injection.
    pub fn activate(&self, name: &str) -> Option<&str> {
        self.skills.get(name).map(|s| s.content.as_str())
    }

    /// Get the full skill metadata + content by name.
    ///
    /// Used when activation needs to inspect `allowed_tools` or other
    /// metadata beyond just the content string.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    /// Inject a built-in skill programmatically (e.g. from a downstream CLI).
    ///
    /// This lets host applications (like `koda-cli`) embed their own
    /// documentation as a skill without coupling `koda-core` to any
    /// application-specific content.  Call after [`Self::discover`].
    ///
    /// `when_to_use` is shown in `ListSkills` output so the model knows
    /// when to activate this skill without hard-coded `instructions.md` hints.
    ///
    /// Overwrites any previously registered skill with the same name.
    pub fn add_builtin(
        &mut self,
        name: &str,
        description: &str,
        when_to_use: Option<&str>,
        content: &str,
    ) {
        let skill = Skill {
            meta: SkillMeta {
                name: name.to_string(),
                description: description.to_string(),
                tags: vec![],
                when_to_use: when_to_use.map(str::to_string),
                allowed_tools: vec![],
                user_invocable: true,
                argument_hint: None,
                source: SkillSource::BuiltIn,
                path: None,
            },
            content: content.to_string(),
        };
        self.skills.insert(name.to_string(), skill);
    }

    /// Add `skill` under its name, replacing one of the same name.
    pub fn insert(&mut self, skill: Skill) {
        self.skills.insert(skill.meta.name.clone(), skill);
    }

    /// Number of discovered skills.
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// Returns `true` if no skills were discovered.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }
}

/// Parse a SKILL.md file with YAML frontmatter.
///
/// Format:
/// ```text
/// ---
/// name: code-review
/// description: Senior code review
/// tags: [review, quality]
/// ---
///
/// # Skill content here...
/// ```
fn parse_skill_md(raw: &str, source: SkillSource) -> Result<Skill, String> {
    let trimmed = raw.trim_start();
    if !trimmed.starts_with("---") {
        return Err("it doesn't start with a `---` frontmatter block".to_string());
    }

    // Find closing ---
    let after_open = &trimmed[3..];
    let close_pos = after_open
        .find("\n---")
        .ok_or("its frontmatter has no closing `---`")?;
    let frontmatter = &after_open[..close_pos].trim();
    let content = after_open[close_pos + 4..].trim_start().to_string();

    // Simple YAML parsing (no serde_yaml dependency).
    // Supported keys: name, description, tags, when_to_use, allowed_tools,
    //   user_invocable, argument_hint.
    // Multi-line YAML values and complex types are intentionally not supported.
    let mut name = String::new();
    let mut description = String::new();
    let mut tags = Vec::new();
    let mut when_to_use: Option<String> = None;
    let mut allowed_tools: Vec<String> = Vec::new();
    let mut user_invocable = true;
    let mut argument_hint: Option<String> = None;

    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("name:") {
            name = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("description:") {
            description = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("when_to_use:") {
            when_to_use = Some(val.trim().to_string());
        } else if let Some(val) = line
            .strip_prefix("allowed_tools:")
            .or_else(|| line.strip_prefix("allowed-tools:"))
        {
            // Parse [Tool1, Tool2] or comma-separated
            let val = val.trim();
            if let Some(inner) = val.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                allowed_tools = inner
                    .split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect();
            } else if !val.is_empty() {
                allowed_tools = val
                    .split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect();
            }
        } else if let Some(val) = line
            .strip_prefix("user_invocable:")
            .or_else(|| line.strip_prefix("user-invocable:"))
        {
            user_invocable = val.trim() != "false";
        } else if let Some(val) = line
            .strip_prefix("argument_hint:")
            .or_else(|| line.strip_prefix("argument-hint:"))
        {
            let val = val.trim();
            if !val.is_empty() {
                argument_hint = Some(val.to_string());
            }
        } else if let Some(val) = line.strip_prefix("tags:") {
            // Parse [tag1, tag2, tag3]
            let val = val.trim();
            if let Some(inner) = val.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                tags = inner.split(',').map(|t| t.trim().to_string()).collect();
            }
        }
    }

    if name.is_empty() {
        return Err("its frontmatter has no `name:`".to_string());
    }

    Ok(Skill {
        meta: SkillMeta {
            name,
            description,
            tags,
            when_to_use,
            allowed_tools,
            user_invocable,
            argument_hint,
            source,
            path: None,
        },
        content,
    })
}

/// Whether `c` can be part of a skill's name in a `$name` mention, as
/// Codex's mention parser has it: ASCII letters and digits, `_`, `-`, `:`.
pub fn is_mention_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':')
}

/// Shell variables a `$` in a message more likely means than a skill, which
/// Codex's parser skips.
fn is_common_env_var(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "PATH"
            | "HOME"
            | "USER"
            | "SHELL"
            | "PWD"
            | "TMPDIR"
            | "TEMP"
            | "TMP"
            | "LANG"
            | "TERM"
            | "XDG_CONFIG_HOME"
    )
}

/// The names a message mentions with `$name`, in order, each once, as
/// Codex reads `$` mentions: a `$` followed by name characters
/// ([`is_mention_name_char`]), anywhere in the text, except common shell
/// variables such as `$PATH`. Whether a name is a skill is the caller's to
/// check.
pub fn mentioned_names(text: &str) -> Vec<&str> {
    let mut names: Vec<&str> = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find('$') {
        let after = &rest[at + 1..];
        let end = after
            .char_indices()
            .find(|(_, c)| !is_mention_name_char(*c))
            .map_or(after.len(), |(i, _)| i);
        let name = &after[..end];
        if !name.is_empty() && !is_common_env_var(name) && !names.contains(&name) {
            names.push(name);
        }
        rest = &after[end..];
    }
    names
}

/// A skill as the model gets it when a person names it, Codex's block: its
/// name, where it came from and its instructions.
pub fn mention_block(skill: &Skill) -> String {
    let path = skill
        .meta
        .path
        .as_ref()
        .map_or_else(|| "built-in".to_string(), |p| p.display().to_string());
    format!(
        "<skill>\n<name>{}</name>\n<path>{path}</path>\n{}\n</skill>",
        skill.meta.name,
        skill.content.trim_end()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_skill_md() {
        let raw = r#"---
name: code-review
description: Senior code review
tags: [review, quality]
when_to_use: Use when asked to review code or a PR.
---

# Code Review

Do the review.
"#;
        let skill = parse_skill_md(raw, SkillSource::BuiltIn).unwrap();
        assert_eq!(skill.meta.name, "code-review");
        assert_eq!(skill.meta.description, "Senior code review");
        assert_eq!(skill.meta.tags, vec!["review", "quality"]);
        assert_eq!(
            skill.meta.when_to_use.as_deref(),
            Some("Use when asked to review code or a PR.")
        );
        assert!(skill.meta.allowed_tools.is_empty());
        assert!(skill.meta.user_invocable);
        assert!(skill.meta.argument_hint.is_none());
        assert!(skill.content.contains("# Code Review"));
        assert!(skill.content.contains("Do the review."));
    }

    #[test]
    fn test_parse_allowed_tools() {
        let raw = "---\nname: scoped\ndescription: Scoped skill\ntags: []\nallowed_tools: [Read, Grep, Glob]\n---\ncontent";
        let skill = parse_skill_md(raw, SkillSource::BuiltIn).unwrap();
        assert_eq!(skill.meta.allowed_tools, vec!["Read", "Grep", "Glob"]);
    }

    #[test]
    fn test_parse_allowed_tools_hyphenated() {
        let raw = "---\nname: scoped\ndescription: Scoped skill\ntags: []\nallowed-tools: [Read, Write]\n---\ncontent";
        let skill = parse_skill_md(raw, SkillSource::BuiltIn).unwrap();
        assert_eq!(skill.meta.allowed_tools, vec!["Read", "Write"]);
    }

    #[test]
    fn test_parse_user_invocable_false() {
        let raw = "---\nname: model-only\ndescription: hidden\ntags: []\nuser_invocable: false\n---\ncontent";
        let skill = parse_skill_md(raw, SkillSource::BuiltIn).unwrap();
        assert!(!skill.meta.user_invocable);
    }

    #[test]
    fn test_parse_user_invocable_hyphenated() {
        let raw = "---\nname: model-only\ndescription: hidden\ntags: []\nuser-invocable: false\n---\ncontent";
        let skill = parse_skill_md(raw, SkillSource::BuiltIn).unwrap();
        assert!(!skill.meta.user_invocable);
    }

    #[test]
    fn test_parse_user_invocable_default_true() {
        let raw = "---\nname: visible\ndescription: shown\ntags: []\n---\ncontent";
        let skill = parse_skill_md(raw, SkillSource::BuiltIn).unwrap();
        assert!(skill.meta.user_invocable);
    }

    #[test]
    fn test_parse_argument_hint() {
        let raw = "---\nname: pdf\ndescription: Generate PDF\ntags: []\nargument_hint: <file_path>\n---\ncontent";
        let skill = parse_skill_md(raw, SkillSource::BuiltIn).unwrap();
        assert_eq!(skill.meta.argument_hint.as_deref(), Some("<file_path>"));
    }

    #[test]
    fn test_parse_argument_hint_hyphenated() {
        let raw = "---\nname: pdf\ndescription: Generate PDF\ntags: []\nargument-hint: <output_dir>\n---\ncontent";
        let skill = parse_skill_md(raw, SkillSource::BuiltIn).unwrap();
        assert_eq!(skill.meta.argument_hint.as_deref(), Some("<output_dir>"));
    }

    #[test]
    fn test_list_user_invocable_excludes_model_only() {
        let mut registry = SkillRegistry::default();
        registry.add_builtin("user-skill", "for users", None, "content");
        // Manually insert a model-only skill
        registry.skills.insert(
            "model-skill".to_string(),
            Skill {
                meta: SkillMeta {
                    name: "model-skill".to_string(),
                    description: "model only".to_string(),
                    tags: vec![],
                    when_to_use: None,
                    allowed_tools: vec![],
                    user_invocable: false,
                    argument_hint: None,
                    source: SkillSource::BuiltIn,
                    path: None,
                },
                content: "secret".to_string(),
            },
        );
        assert_eq!(registry.list().len(), 2);
        assert_eq!(registry.list_user_invocable().len(), 1);
        assert_eq!(registry.list_user_invocable()[0].name, "user-skill");
    }

    #[test]
    fn test_get_returns_full_skill() {
        let mut registry = SkillRegistry::default();
        registry.add_builtin("test", "desc", None, "body");
        let skill = registry.get("test").unwrap();
        assert_eq!(skill.meta.name, "test");
        assert_eq!(skill.content, "body");
    }

    #[test]
    fn test_parse_when_to_use_absent() {
        let raw = "---\nname: minimal\ndescription: minimal skill\ntags: []\n---\ncontent";
        let skill = parse_skill_md(raw, SkillSource::BuiltIn).unwrap();
        assert!(skill.meta.when_to_use.is_none());
    }

    #[test]
    fn test_parse_no_frontmatter() {
        assert!(parse_skill_md("# Just markdown", SkillSource::BuiltIn).is_err());
    }

    #[test]
    fn test_parse_no_name() {
        let raw = "---\ndescription: no name\n---\ncontent";
        assert!(parse_skill_md(raw, SkillSource::BuiltIn).is_err());
    }

    #[test]
    fn test_builtin_skills_load() {
        let mut registry = SkillRegistry::default();
        registry.load_builtin();
        assert!(registry.len() >= 2);
        assert!(registry.activate("code-review").is_some());
        assert!(registry.activate("security-audit").is_some());
        assert!(registry.activate("simplify").is_some());
        assert!(registry.activate("debug").is_some());
        assert!(registry.activate("remember").is_some());
        assert!(registry.activate("create-agent").is_some());
        assert!(registry.activate("create-skill").is_some());
    }

    /// Pin the create-agent + create-skill bundled skills so they don't get
    /// silently broken. Each assertion maps to a specific user-facing failure:
    /// missing front-matter field => skill won't load; missing key guidance
    /// in the body => generated agents/skills will have known footguns.
    #[test]
    fn test_creation_skills_are_complete() {
        let mut registry = SkillRegistry::default();
        registry.load_builtin();

        // ── create-agent ────────────────────────────────────────
        let agent = registry
            .get("create-agent")
            .expect("create-agent skill must load");
        assert!(
            agent.meta.when_to_use.is_some(),
            "create-agent needs when_to_use for auto-activation"
        );
        assert!(
            !agent.meta.allowed_tools.is_empty(),
            "create-agent should scope its tools (least privilege)"
        );
        // The body must include the trust footgun warning — this is
        // the #1 thing that breaks generated agents if missing (#1250).
        // Pre-#1250 the equivalent footgun was `write_access`; post-#1250
        // the canonical mechanism is the `trust` field.
        let agent_body = registry.activate("create-agent").unwrap();
        assert!(
            agent_body.contains("trust"),
            "create-agent must teach the trust field (post-#1250)"
        );
        assert!(
            agent_body.contains("footgun") || agent_body.contains("silently"),
            "create-agent must warn about the trust default-deny footgun"
        );
        // Both scope paths documented + correct personal path.
        assert!(
            agent_body.contains(".koda/agents/"),
            "create-agent must document project-scope path"
        );
        assert!(
            agent_body.contains("~/.config/koda/agents/"),
            "create-agent must document personal-scope path (~/.config/koda/, NOT ~/.koda/)"
        );
        // Reference to a canonical example so the model can crib.
        assert!(
            agent_body.contains("koda-core/agents/explore.json"),
            "create-agent must point at a reference example"
        );

        // ── create-skill ────────────────────────────────────────
        let skill = registry
            .get("create-skill")
            .expect("create-skill skill must load");
        assert!(
            skill.meta.when_to_use.is_some(),
            "create-skill needs when_to_use for auto-activation"
        );
        assert!(
            !skill.meta.allowed_tools.is_empty(),
            "create-skill should scope its tools (least privilege)"
        );
        let skill_body = registry.activate("create-skill").unwrap();
        // Frontmatter fields the model must teach.
        assert!(
            skill_body.contains("when_to_use"),
            "create-skill must teach the when_to_use field"
        );
        assert!(
            skill_body.contains("allowed_tools"),
            "create-skill must teach allowed_tools scoping"
        );
        // Both scope paths documented + correct personal path.
        assert!(
            skill_body.contains(".koda/skills/"),
            "create-skill must document project-scope path"
        );
        assert!(
            skill_body.contains("~/.config/koda/skills/"),
            "create-skill must document personal-scope path"
        );
        // Reference to a canonical example so the model can crib.
        assert!(
            skill_body.contains("koda-skills/skills/code-review/SKILL.md")
                || skill_body.contains("koda-skills/skills/debug/SKILL.md"),
            "create-skill must point at a reference example"
        );
    }

    #[test]
    fn test_search() {
        let mut registry = SkillRegistry::default();
        registry.load_builtin();

        let results = registry.search("review");
        // code-review, simplify, and remember all contain "review" in their metadata
        assert!(!results.is_empty());
        assert!(results.iter().any(|s| s.name == "code-review"));

        let results = registry.search("security");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "security-audit");
    }

    #[test]
    fn test_search_by_tag() {
        let mut registry = SkillRegistry::default();
        registry.load_builtin();

        let results = registry.search("owasp");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "security-audit");
    }

    #[test]
    fn test_add_builtin_injects_skill() {
        let mut registry = SkillRegistry::default();
        registry.add_builtin(
            "my-app-docs",
            "My app user manual",
            Some("Use when the user asks about the app."),
            "# My App\n\nDo stuff.",
        );
        assert_eq!(registry.len(), 1);
        let content = registry.activate("my-app-docs").unwrap();
        assert!(content.contains("Do stuff."));
        // Source must be BuiltIn
        let meta = registry.list();
        assert!(matches!(meta[0].source, SkillSource::BuiltIn));
        assert_eq!(
            meta[0].when_to_use.as_deref(),
            Some("Use when the user asks about the app.")
        );
    }

    #[test]
    fn test_add_builtin_overwrites_same_name() {
        let mut registry = SkillRegistry::default();
        registry.add_builtin("docs", "v1", None, "version one");
        registry.add_builtin("docs", "v2", None, "version two");
        assert_eq!(registry.len(), 1);
        assert!(registry.activate("docs").unwrap().contains("version two"));
    }

    #[test]
    fn test_list_sorted() {
        let mut registry = SkillRegistry::default();
        registry.load_builtin();

        let list = registry.list();
        let names: Vec<&str> = list.iter().map(|s| s.name.as_str()).collect();
        // Sorted alphabetically: code-review, create-agent, create-skill,
        // debug, remember, security-audit, simplify
        assert!(list.len() >= 7);
        assert_eq!(names[0], "code-review");
        assert_eq!(names[1], "create-agent");
        assert_eq!(names[2], "create-skill");
        assert_eq!(names[3], "debug");
        assert_eq!(names[4], "remember");
        assert_eq!(names[5], "security-audit");
        assert_eq!(names[6], "simplify");
    }

    #[test]
    fn test_directory_discovery() {
        let tmp = tempfile::TempDir::new().unwrap();
        let skill_dir = tmp.path().join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: test\ntags: []\n---\n# Test",
        )
        .unwrap();

        let mut registry = SkillRegistry::default();
        registry.load_directory(tmp.path(), SkillSource::Project);
        assert_eq!(registry.len(), 1);
        assert!(registry.activate("my-skill").is_some());
    }

    #[test]
    fn a_skill_file_that_doesnt_parse_is_reported_with_its_reason() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good");
        let broken = dir.path().join("broken");
        let not_a_skill = dir.path().join("notes");
        for d in [&good, &broken, &not_a_skill] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(
            good.join("SKILL.md"),
            "---\nname: good\ndescription: Works\n---\nBody",
        )
        .unwrap();
        std::fs::write(
            broken.join("SKILL.md"),
            "---\ndescription: no name\n---\nBody",
        )
        .unwrap();

        let mut registry = SkillRegistry::default();
        registry.load_directory(dir.path(), SkillSource::Project);

        let loaded = registry.get("good").unwrap();
        assert_eq!(
            loaded.meta.path.as_deref(),
            Some(good.join("SKILL.md").as_path())
        );
        assert_eq!(
            registry.errors(),
            [SkillError {
                path: broken.join("SKILL.md"),
                message: "its frontmatter has no `name:`".into(),
            }]
        );
    }

    #[test]
    fn dollar_mentions_are_read_as_codex_reads_them() {
        assert_eq!(
            mentioned_names("use $review-pr on this, then $lint:fix and $review-pr again"),
            vec!["review-pr", "lint:fix"]
        );
        assert_eq!(mentioned_names("$deploy"), vec!["deploy"]);
        assert_eq!(
            mentioned_names("costs $5"),
            vec!["5"],
            "the caller checks it's a skill"
        );
        assert!(
            mentioned_names("echo $PATH and $home").is_empty(),
            "shell variables"
        );
        assert!(mentioned_names("a lone $ sign, $ and $").is_empty());
        assert_eq!(mentioned_names("(see $notes.)"), vec!["notes"]);
        assert_eq!(
            mentioned_names("héllo $café"),
            vec!["caf"],
            "ASCII names only"
        );
    }

    #[test]
    fn a_mentioned_skill_is_sent_in_codexs_block() {
        let raw = "---\nname: checklist\ndescription: d\ntags: []\n---\nCheck the tests.\n\n";
        let mut skill = parse_skill_md(raw, SkillSource::Project).unwrap();
        assert_eq!(
            mention_block(&skill),
            "<skill>\n<name>checklist</name>\n<path>built-in</path>\nCheck the tests.\n</skill>"
        );
        skill.meta.path = Some("/p/.koda/skills/checklist/SKILL.md".into());
        assert!(mention_block(&skill).contains("<path>/p/.koda/skills/checklist/SKILL.md</path>"));
    }
}
