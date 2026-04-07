//! Skill discovery and loading.
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
#[derive(Debug, Clone)]
pub struct SkillMeta {
    /// Skill name (derived from filename or frontmatter).
    pub name: String,
    /// One-line description.
    pub description: String,
    /// Searchable tags.
    pub tags: Vec<String>,
    /// Where this skill was discovered.
    pub source: SkillSource,
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
    skills: HashMap<String, Skill>,
}

impl SkillRegistry {
    /// Discover skills from all standard locations.
    pub fn discover(project_root: &Path) -> Self {
        let mut registry = Self::default();

        // 1. Built-in skills (embedded at compile time)
        registry.load_builtin();

        // 2. User-global skills
        if let Ok(config_dir) = crate::db::config_dir() {
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
        ];

        for (name, content) in builtins {
            if let Some(skill) = parse_skill_md(content, SkillSource::BuiltIn) {
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
            let skill_file = entry.path().join("SKILL.md");
            if let Some(skill) = std::fs::read_to_string(&skill_file)
                .ok()
                .and_then(|content| parse_skill_md(&content, source.clone()))
            {
                self.skills.insert(skill.meta.name.clone(), skill);
            }
        }
    }

    /// List all discovered skills (name + description).
    pub fn list(&self) -> Vec<&SkillMeta> {
        let mut metas: Vec<&SkillMeta> = self.skills.values().map(|s| &s.meta).collect();
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

    /// Inject a built-in skill programmatically (e.g. from a downstream CLI).
    ///
    /// This lets host applications (like `koda-cli`) embed their own
    /// documentation as a skill without coupling `koda-core` to any
    /// application-specific content.  Call after [`Self::discover`].
    ///
    /// Overwrites any previously registered skill with the same name.
    pub fn add_builtin(&mut self, name: &str, description: &str, content: &str) {
        let skill = Skill {
            meta: SkillMeta {
                name: name.to_string(),
                description: description.to_string(),
                tags: vec![],
                source: SkillSource::BuiltIn,
            },
            content: content.to_string(),
        };
        self.skills.insert(name.to_string(), skill);
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
fn parse_skill_md(raw: &str, source: SkillSource) -> Option<Skill> {
    let trimmed = raw.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }

    // Find closing ---
    let after_open = &trimmed[3..];
    let close_pos = after_open.find("\n---")?;
    let frontmatter = &after_open[..close_pos].trim();
    let content = after_open[close_pos + 4..].trim_start().to_string();

    // Simple YAML parsing (no serde_yaml dependency)
    let mut name = String::new();
    let mut description = String::new();
    let mut tags = Vec::new();

    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("name:") {
            name = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("description:") {
            description = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("tags:") {
            // Parse [tag1, tag2, tag3]
            let val = val.trim();
            if let Some(inner) = val.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                tags = inner.split(',').map(|t| t.trim().to_string()).collect();
            }
        }
    }

    if name.is_empty() {
        return None;
    }

    Some(Skill {
        meta: SkillMeta {
            name,
            description,
            tags,
            source,
        },
        content,
    })
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
---

# Code Review

Do the review.
"#;
        let skill = parse_skill_md(raw, SkillSource::BuiltIn).unwrap();
        assert_eq!(skill.meta.name, "code-review");
        assert_eq!(skill.meta.description, "Senior code review");
        assert_eq!(skill.meta.tags, vec!["review", "quality"]);
        assert!(skill.content.contains("# Code Review"));
        assert!(skill.content.contains("Do the review."));
    }

    #[test]
    fn test_parse_no_frontmatter() {
        assert!(parse_skill_md("# Just markdown", SkillSource::BuiltIn).is_none());
    }

    #[test]
    fn test_parse_no_name() {
        let raw = "---\ndescription: no name\n---\ncontent";
        assert!(parse_skill_md(raw, SkillSource::BuiltIn).is_none());
    }

    #[test]
    fn test_builtin_skills_load() {
        let mut registry = SkillRegistry::default();
        registry.load_builtin();
        assert!(registry.len() >= 2);
        assert!(registry.activate("code-review").is_some());
        assert!(registry.activate("security-audit").is_some());
    }

    #[test]
    fn test_search() {
        let mut registry = SkillRegistry::default();
        registry.load_builtin();

        let results = registry.search("review");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "code-review");

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
        registry.add_builtin("my-app-docs", "My app user manual", "# My App\n\nDo stuff.");
        assert_eq!(registry.len(), 1);
        let content = registry.activate("my-app-docs").unwrap();
        assert!(content.contains("Do stuff."));
        // Source must be BuiltIn
        let meta = registry.list();
        assert!(matches!(meta[0].source, SkillSource::BuiltIn));
    }

    #[test]
    fn test_add_builtin_overwrites_same_name() {
        let mut registry = SkillRegistry::default();
        registry.add_builtin("docs", "v1", "version one");
        registry.add_builtin("docs", "v2", "version two");
        assert_eq!(registry.len(), 1);
        assert!(registry.activate("docs").unwrap().contains("version two"));
    }

    #[test]
    fn test_list_sorted() {
        let mut registry = SkillRegistry::default();
        registry.load_builtin();

        let list = registry.list();
        assert!(list.len() >= 2);
        assert_eq!(list[0].name, "code-review");
        assert_eq!(list[1].name, "security-audit");
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
}
