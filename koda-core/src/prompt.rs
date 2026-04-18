//! System prompt construction.
//!
//! Builds the system prompt from agent config, memory, and available tools.
//! The prompt is the single source of truth for what the model knows about
//! Koda's capabilities — it is **generated from code**, not a static file.
//!
//! ## Prompt structure
//!
//! The assembled prompt contains (in order):
//!
//! 1. **Base prompt** — from the agent's `system_prompt` field
//! 2. **Behavioral instructions** — `instructions.md` (how to act)
//! 3. **Environment** — working dir, platform, shell, model
//! 4. **Quick Reference** — auto-generated from `SLASH_COMMANDS` + `ToolDefinition`
//! 5. **Sub-agents** — available agents with descriptions and delegation guidance
//! 6. **Skills** — live listing with `when_to_use` hints; model MUST activate before responding
//! 7. **Memory** — project and global learned facts

use std::path::Path;

use crate::skills::SkillRegistry;

/// Runtime environment context injected into the system prompt.
pub struct EnvironmentInfo<'a> {
    /// Project root / working directory.
    pub project_root: &'a Path,
    /// Model identifier (e.g. "claude-sonnet-4-6", "gpt-4o").
    pub model: &'a str,
    /// Platform (e.g. "macos", "linux").
    pub platform: &'a str,
}

/// Build the system prompt with instructions, environment, memory, and tool schemas.
///
/// `commands` is a list of `(name, description)` pairs for user-facing slash
/// commands (e.g. `("/help", "Show this help")`).  Pass `&[]` for sub-agents
/// that don't expose a REPL.
///
/// `skill_registry` is used to build the live `## Skills` section so the model
/// sees every available skill — with its `when_to_use` hint — without needing
/// to call `ListSkills` first.
///
/// `mcp_instructions` carries `(server_name, instructions)` pairs harvested
/// from each connected MCP server's `initialize` response (#922). The block
/// is omitted entirely when the slice is empty so non-MCP users pay zero
/// tokens.
pub fn build_system_prompt(
    base_prompt: &str,
    semantic_memory: &str,
    agents_dir: &Path,
    env: &EnvironmentInfo<'_>,
    commands: &[(&str, &str)],
    skill_registry: &SkillRegistry,
    mcp_instructions: &[(String, String)],
) -> String {
    let mut prompt = base_prompt.to_string();

    // Behavioral instructions (CC-aligned, #587)
    prompt.push_str("\n\n");
    prompt.push_str(include_str!("instructions.md"));

    // Environment context
    prompt.push_str("\n\n## Environment\n");
    prompt.push_str(&format!(
        "- Working directory: {}\n",
        env.project_root.display()
    ));
    prompt.push_str(&format!("- Platform: {}\n", env.platform));
    if let Ok(shell) = std::env::var("SHELL") {
        prompt.push_str(&format!("- Shell: {}\n", shell));
    }
    prompt.push_str(&format!("- Model: {}\n", env.model));

    // Capabilities quick-reference (generated from code, replaces static capabilities.md)
    prompt.push_str("\n## Koda Quick Reference\n\n");
    prompt.push_str("Refer to this when the user asks \"what can you do?\" or about features.\n");

    // Commands — generated from the registry passed by the CLI
    if !commands.is_empty() {
        prompt.push_str("\n### Commands (user types these in the REPL)\n\n");
        for &(name, desc) in commands {
            prompt.push_str(&format!("- `{name}` — {desc}\n"));
        }
        prompt.push_str("- `Shift+Tab` — cycle approval mode (auto/confirm)\n");
    }

    // Static behavioral guidance (doesn't drift — hardcoded is fine)
    prompt.push_str(
        "\n### Input\n\n\
         - `@file.rs` attaches file context, `@image.png` for multi-modal analysis\n\
         - `Alt+Enter` inserts a newline for multi-line prompts\n\
         - Piped input: `echo \"explain\" | koda` or `koda -p \"prompt\"` for headless/CI\n",
    );
    prompt.push_str(
        "\n### Approval\n\n\
         Two modes (cycle with Shift+Tab): **auto** (default), **confirm**.\n\
         Hotkeys during tool confirmation: `y` approve, `n` reject, `f` feedback, `a` always.\n",
    );
    prompt.push_str(
        "\n### Git Checkpointing\n\n\
         Auto-snapshots working tree before each turn. `/undo` to rollback.\n",
    );

    // Tool definitions intentionally NOT rendered here — each provider
    // (Anthropic, OpenAI-compat, Gemini) sends the full schema (name +
    // description + parameters) in the API request body. Duplicating it
    // in the prompt was ~1,472 tokens (~37% of the prompt) of pure
    // redundancy. See #925 for the investigation.

    // Sub-agents — dynamic listing with descriptions
    let available_agents = list_available_agents(agents_dir);
    if !available_agents.is_empty() {
        prompt.push_str("\n\n## Available Sub-Agents\n\n");
        prompt.push_str(
            "Use `InvokeAgent` when the task matches an agent's description below. \
             Do NOT invent agent names that are not listed here.\n\n",
        );
        for (name, desc) in &available_agents {
            if let Some(d) = desc {
                prompt.push_str(&format!("- **{name}** — {d}\n"));
            } else {
                prompt.push_str(&format!("- {name}\n"));
            }
        }
        prompt.push_str(
            "\nWhen to use sub-agents:\n\
             - Complex multi-step tasks where you want to keep your context clean\n\
             - Independent parallel work (launch multiple agents in one response)\n\
             - Research that would fill your context with noise (file contents, grep results)\n\
             \n\
             When NOT to use sub-agents:\n\
             - Simple file reads or 2\u{2013}3 grep queries (overhead > direct execution)\n\
             - Tasks that need user interaction (sub-agents can\u{2019}t ask questions)\n\
             \n\
             Sub-agent results are NOT visible to the user — always summarize key findings.\n",
        );
    } else {
        prompt.push_str(
            "\n\nNote: No sub-agents are configured. \
             Do not use the InvokeAgent tool.\n",
        );
    }

    // Skills — live listing so the model sees every skill upfront, no ListSkills call needed.
    let skills = skill_registry.list();
    if skills.is_empty() {
        prompt.push_str(
            "\n## Skills\n\n\
             No skills are currently available. \
             Add custom skills to `.koda/skills/<name>/SKILL.md`.\n",
        );
    } else {
        prompt.push_str(
            "\n## Skills\n\n\
             Expert instruction modules — zero LLM cost, instant activation via `ActivateSkill`.\n\
             IMPORTANT: If the user's request matches a skill below, you MUST call \
             `ActivateSkill` FIRST — before writing any response. \
             Do not answer from training data when a skill covers the topic.\n\n",
        );
        for meta in &skills {
            // Base: "- **name** — description"
            let mut line = format!("- **{}** — {}", meta.name, meta.description);
            // Append when_to_use if present
            if let Some(wtu) = &meta.when_to_use {
                line.push_str(&format!(" — {wtu}"));
            }
            // Append tool scope hint
            if !meta.allowed_tools.is_empty() {
                line.push_str(&format!(" (Tools: {})", meta.allowed_tools.join(", ")));
            }
            // Append argument hint
            if let Some(hint) = &meta.argument_hint {
                line.push_str(&format!(" `{hint}`"));
            }
            // Mark model-only skills
            if !meta.user_invocable {
                line.push_str(" [model-only]");
            }
            line.push('\n');
            prompt.push_str(&line);
        }
        prompt.push_str(
            "\nCustom skills: `.koda/skills/<name>/SKILL.md` (project) \
             or `~/.config/koda/skills/<name>/SKILL.md` (global).\n",
        );
    }

    // MCP server instructions (#922). Each connected server can return a
    // free-form `instructions` string in its `initialize` response telling
    // the model how to use it best ("prefer tool X over Y", "this codebase
    // uses Postgres flavor Z", etc). Render only when at least one server
    // provided non-empty guidance — zero-cost for non-MCP users.
    if !mcp_instructions.is_empty() {
        prompt.push_str("\n\n# MCP Server Instructions\n");
        for (server, instructions) in mcp_instructions {
            prompt.push_str(&format!("\n## {server}\n{instructions}\n"));
        }
    }

    // Memory paths
    prompt.push_str(
        "\n## Memory\n\n\
         Project: `MEMORY.md` (also reads `CLAUDE.md`, `AGENTS.md`) | \
         Global: `~/.config/koda/memory.md`\n",
    );

    // Semantic memory
    if !semantic_memory.is_empty() {
        prompt.push_str(&format!(
            "\n## Project Memory\n\
             The following are learned facts about this project:\n\
             {semantic_memory}"
        ));
    }

    prompt
}

/// Scan the agents/ directory and return available agent names with optional descriptions.
///
/// Returns `(name, Option<description>)` pairs sorted by name.
/// Descriptions come from the `description` field in the agent's JSON config.
/// The default/main agent (`koda`, `default`) is excluded — it is not a sub-agent.
fn list_available_agents(agents_dir: &Path) -> Vec<(String, Option<String>)> {
    let Ok(entries) = std::fs::read_dir(agents_dir) else {
        return Vec::new();
    };
    let mut agents: Vec<(String, Option<String>)> = entries
        .flatten()
        .filter_map(|entry| {
            let file_name = entry.file_name().to_string_lossy().to_string();
            let name = file_name.strip_suffix(".json")?.to_string();
            // Skip the default agent — it's the main agent, not a sub-agent.
            if name == "koda" || name == "default" {
                return None;
            }
            let description = std::fs::read_to_string(entry.path()).ok().and_then(|json| {
                serde_json::from_str::<serde_json::Value>(&json)
                    .ok()
                    .and_then(|v| v["description"].as_str().map(str::to_string))
            });
            Some((name, description))
        })
        .collect();
    agents.sort_by(|a, b| a.0.cmp(&b.0));
    agents
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::SkillRegistry;
    use tempfile::TempDir;

    fn test_env() -> EnvironmentInfo<'static> {
        // Use a leaked path so the reference lives long enough for tests
        let path: &'static Path = Path::new("/test/project");
        EnvironmentInfo {
            project_root: path,
            model: "test-model",
            platform: "test-os",
        }
    }

    #[test]
    fn test_build_system_prompt_no_agents_no_memory() {
        let dir = TempDir::new().unwrap();
        let env = test_env();
        let registry = SkillRegistry::default();
        let result = build_system_prompt(
            "You are helpful.",
            "",
            dir.path(),
            &env,
            &[],
            &registry,
            &[],
        );
        assert!(result.starts_with("You are helpful."));
        assert!(result.contains("Doing Tasks"));
        assert!(result.contains("Koda Quick Reference"));
        assert!(!result.contains("Project Memory"));
    }

    #[test]
    fn test_build_system_prompt_with_memory() {
        let dir = TempDir::new().unwrap();
        let env = test_env();
        let registry = SkillRegistry::default();
        let result = build_system_prompt(
            "You are helpful.",
            "This is a Rust project.",
            dir.path(),
            &env,
            &[],
            &registry,
            &[],
        );
        assert!(result.contains("Project Memory"));
        assert!(result.contains("Rust project"));
    }

    #[test]
    fn test_build_system_prompt_with_agents() {
        let dir = TempDir::new().unwrap();
        // Write an agent JSON with a description
        std::fs::write(
            dir.path().join("scout.json"),
            r#"{"name":"scout","description":"Scouting agent.","system_prompt":"You scout."}"#,
        )
        .unwrap();
        let env = test_env();
        let registry = SkillRegistry::default();
        let result = build_system_prompt("Base.", "", dir.path(), &env, &[], &registry, &[]);
        assert!(result.contains("scout"));
        assert!(result.contains("Scouting agent."));
        assert!(result.contains("Sub-Agents"));
    }

    #[test]
    fn test_build_system_prompt_skips_koda_agent() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("koda.json"),
            r#"{"name":"koda","system_prompt":"main"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("scout.json"),
            r#"{"name":"scout","system_prompt":"scout"}"#,
        )
        .unwrap();
        let env = test_env();
        let registry = SkillRegistry::default();
        let result = build_system_prompt("Base.", "", dir.path(), &env, &[], &registry, &[]);
        // koda (the main agent) must not appear in the sub-agents listing.
        // Check the full result: the agent formatter produces "- **name**" (with desc)
        // or "- name" (without). Neither should match "koda".
        assert!(
            !result.contains("- **koda**") && !result.contains("\n- koda\n"),
            "koda should not appear as a sub-agent: {result}"
        );
        // scout has no description in this JSON, renders as "- scout"
        assert!(
            result.contains("scout"),
            "scout should appear in the sub-agents section: {result}"
        );
    }

    #[test]
    fn test_environment_section_present() {
        let dir = TempDir::new().unwrap();
        let env = test_env();
        let registry = SkillRegistry::default();
        let result = build_system_prompt("Base.", "", dir.path(), &env, &[], &registry, &[]);
        assert!(result.contains("## Environment"));
        assert!(result.contains("/test/project"));
        assert!(result.contains("test-model"));
        assert!(result.contains("test-os"));
    }

    #[test]
    fn test_instructions_included() {
        let dir = TempDir::new().unwrap();
        let env = test_env();
        let registry = SkillRegistry::default();
        let result = build_system_prompt("Base.", "", dir.path(), &env, &[], &registry, &[]);
        // Spot-check key sections from instructions.md
        assert!(result.contains("## Doing Tasks"));
        assert!(result.contains("## Executing Actions"));
        assert!(result.contains("## Using Your Tools"));
        assert!(result.contains("## Output"));
    }

    #[test]
    fn test_commands_generated_from_registry() {
        let dir = TempDir::new().unwrap();
        let env = test_env();
        let registry = SkillRegistry::default();
        let commands = &[("/help", "Show help"), ("/exit", "Quit")];
        let result = build_system_prompt("Base.", "", dir.path(), &env, commands, &registry, &[]);
        assert!(result.contains("`/help`"));
        assert!(result.contains("Show help"));
        assert!(result.contains("`/exit`"));
        assert!(result.contains("Commands (user types these in the REPL)"));
    }

    #[test]
    fn test_no_commands_section_for_sub_agents() {
        let dir = TempDir::new().unwrap();
        let env = test_env();
        let registry = SkillRegistry::default();
        let result = build_system_prompt("Base.", "", dir.path(), &env, &[], &registry, &[]);
        assert!(!result.contains("Commands (user types these in the REPL)"));
    }

    #[test]
    fn test_skills_section_empty_registry() {
        let dir = TempDir::new().unwrap();
        let env = test_env();
        let registry = SkillRegistry::default();
        let result = build_system_prompt("Base.", "", dir.path(), &env, &[], &registry, &[]);
        assert!(result.contains("## Skills"));
        assert!(result.contains("No skills are currently available"));
    }

    #[test]
    fn test_skills_section_lists_skills() {
        let dir = TempDir::new().unwrap();
        let env = test_env();
        let mut registry = SkillRegistry::default();
        registry.add_builtin(
            "code-review",
            "Senior code review",
            Some("Use when asked to review code or a PR."),
            "# Review\nDo it.",
        );
        let result = build_system_prompt("Base.", "", dir.path(), &env, &[], &registry, &[]);
        assert!(result.contains("code-review"));
        assert!(result.contains("Senior code review"));
        assert!(result.contains("Use when asked to review code or a PR."));
        // Must include the blocking requirement instruction
        assert!(result.contains("MUST call `ActivateSkill` FIRST"));
    }

    #[test]
    fn test_skills_section_no_when_to_use() {
        let dir = TempDir::new().unwrap();
        let env = test_env();
        let mut registry = SkillRegistry::default();
        registry.add_builtin("plain", "Plain skill", None, "content");
        let result = build_system_prompt("Base.", "", dir.path(), &env, &[], &registry, &[]);
        assert!(result.contains("**plain**"));
        assert!(result.contains("Plain skill"));
    }

    #[test]
    fn test_skills_section_shows_metadata() {
        use crate::skills::{Skill, SkillMeta, SkillSource};

        let dir = TempDir::new().unwrap();
        let env = test_env();
        let mut registry = SkillRegistry::default();
        // Inject a skill with all metadata fields populated
        registry.skills.insert(
            "scoped".to_string(),
            Skill {
                meta: SkillMeta {
                    name: "scoped".to_string(),
                    description: "Scoped skill".to_string(),
                    tags: vec![],
                    when_to_use: Some("Use for scoped work".to_string()),
                    allowed_tools: vec!["Read".to_string(), "Grep".to_string()],
                    user_invocable: false,
                    argument_hint: Some("<file_path>".to_string()),
                    source: SkillSource::BuiltIn,
                },
                content: "scoped content".to_string(),
            },
        );
        let result = build_system_prompt("Base.", "", dir.path(), &env, &[], &registry, &[]);
        assert!(result.contains("**scoped**"), "skill name");
        assert!(result.contains("Scoped skill"), "description");
        assert!(result.contains("Use for scoped work"), "when_to_use");
        assert!(result.contains("(Tools: Read, Grep)"), "allowed_tools");
        assert!(result.contains("`<file_path>`"), "argument_hint");
        assert!(result.contains("[model-only]"), "user_invocable=false");
    }

    #[test]
    fn test_agents_sorted_alphabetically() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("zebra.json"),
            r#"{"name":"zebra","system_prompt":"z"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("alpha.json"),
            r#"{"name":"alpha","system_prompt":"a"}"#,
        )
        .unwrap();
        let env = test_env();
        let registry = SkillRegistry::default();
        let result = build_system_prompt("Base.", "", dir.path(), &env, &[], &registry, &[]);
        let alpha_pos = result.find("alpha").unwrap();
        let zebra_pos = result.find("zebra").unwrap();
        assert!(alpha_pos < zebra_pos, "agents should be sorted A→Z");
    }

    // ── MCP server instructions block (#922) ─────────────────────────────

    #[test]
    fn test_no_mcp_instructions_omits_block() {
        let dir = TempDir::new().unwrap();
        let env = test_env();
        let registry = SkillRegistry::default();
        let result = build_system_prompt("Base.", "", dir.path(), &env, &[], &registry, &[]);
        assert!(
            !result.contains("# MCP Server Instructions"),
            "empty MCP slice must produce no block (zero-cost for non-MCP users)"
        );
    }

    #[test]
    fn test_mcp_instructions_renders_when_present() {
        let dir = TempDir::new().unwrap();
        let env = test_env();
        let registry = SkillRegistry::default();
        let mcp = vec![
            (
                "playwright".to_string(),
                "Prefer locator-based queries over CSS selectors.".to_string(),
            ),
            (
                "postgres".to_string(),
                "Always use parameterized queries.".to_string(),
            ),
        ];
        let result = build_system_prompt("Base.", "", dir.path(), &env, &[], &registry, &mcp);
        assert!(result.contains("# MCP Server Instructions"));
        assert!(result.contains("## playwright"));
        assert!(result.contains("locator-based queries"));
        assert!(result.contains("## postgres"));
        assert!(result.contains("parameterized queries"));
        // Block must come BEFORE ## Memory in the assembled prompt.
        let mcp_pos = result.find("# MCP Server Instructions").unwrap();
        let memory_pos = result.find("## Memory").unwrap();
        assert!(mcp_pos < memory_pos, "MCP block belongs before Memory");
    }

    #[test]
    fn test_mcp_instructions_renders_each_server_distinctly() {
        // Regression guard: the renderer must produce one ## <name> subheader
        // per server so model can attribute guidance correctly.
        let dir = TempDir::new().unwrap();
        let env = test_env();
        let registry = SkillRegistry::default();
        let mcp = vec![
            ("alpha".to_string(), "first".to_string()),
            ("beta".to_string(), "second".to_string()),
        ];
        let result = build_system_prompt("Base.", "", dir.path(), &env, &[], &registry, &mcp);
        assert_eq!(
            result.matches("# MCP Server Instructions").count(),
            1,
            "top-level header should appear exactly once"
        );
        assert!(result.contains("## alpha\nfirst"));
        assert!(result.contains("## beta\nsecond"));
    }

    /// Measurement-only test for #920. Renders a realistic system prompt with
    /// all built-in skills + bundled sub-agents loaded, then prints a per-section
    /// breakdown (chars + estimated tokens at ~4 chars/token).
    ///
    /// Run with: `cargo test -p koda-core --lib measure_system_prompt -- --ignored --nocapture`
    ///
    /// Re-run after each prompt-trim PR to verify savings.
    #[test]
    #[ignore]
    fn measure_system_prompt() {
        // Realistic setup: bundled agents from koda-core/agents/ + all built-in skills
        let project_root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let agents_dir = project_root.join("agents");
        // SkillRegistry::discover() is the public entry point that loads
        // built-ins + user/project skills. Pass project_root so it doesn't
        // pick up koda-core's own .koda/skills directory if any.
        let registry = SkillRegistry::discover(project_root);

        // Realistic env
        let env = EnvironmentInfo {
            project_root,
            model: "claude-sonnet-4-6",
            platform: "macos",
        };

        // Tool count for the setup banner. Tools are sent in the API request
        // body, NOT rendered in the prompt (#925) — we just want to show how
        // many tools the model gets so the prompt size is interpretable.
        let tool_count = crate::tools::ToolRegistry::new(project_root.to_path_buf(), 200_000)
            .get_definitions(&[], &[])
            .len();

        // Realistic slash commands
        let commands = &[
            ("/help", "Show command help"),
            ("/skills", "List available skills"),
            ("/agents", "List available sub-agents"),
            ("/memory", "Show project + global memory"),
            ("/compact", "Compact conversation history"),
        ];

        let prompt = build_system_prompt(
            "You are koda, a helpful coding agent.",
            "",
            &agents_dir,
            &env,
            commands,
            &registry,
            &[],
        );

        // ── Section breakdown by header position ─────────────────────────
        // Order matches the actual assembly order in build_system_prompt.
        let markers: &[&str] = &[
            "## Doing Tasks", // from instructions.md (CC-aligned behavioral)
            "## Environment",
            "## Available Sub-Agents",
            "## Skills",
            "## Memory",
        ];

        // Find positions; require the marker to appear at the start of a
        // line (preceded by '\n') so headers like '## Skills' don't false-match
        // inside a subsection like '## Skills and Sub-Agents' in instructions.md.
        // The earlier version used naive prompt.find() and produced wildly
        // inaccurate per-section attribution — see #925 PR description.
        let mut positions: Vec<(&str, usize)> = markers
            .iter()
            .filter_map(|m| {
                let needle = format!("\n{m}\n");
                prompt.find(&needle).map(|p| (*m, p + 1)) // +1 to skip leading \n
            })
            .collect();
        // Sort by actual position in the prompt — marker-list order doesn't
        // necessarily match assembly order.
        positions.sort_by_key(|&(_, pos)| pos);

        // Compute spans between markers; the last span runs to end-of-prompt.
        // The span BEFORE the first marker is the base prompt.
        let total_chars = prompt.chars().count();
        let total_tokens_est = total_chars / 4;

        eprintln!("\n========== SYSTEM PROMPT MEASUREMENT (#920) ==========");
        eprintln!(
            "Setup: koda default agent, model=claude-sonnet-4-6, {} bundled agents loaded, {} built-in skills, {} tools (sent via API, not in prompt), {} commands",
            std::fs::read_dir(&agents_dir)
                .map(|d| d.filter_map(|e| e.ok()).count())
                .unwrap_or(0),
            registry.len(),
            tool_count,
            commands.len()
        );
        eprintln!(
            "\nTOTAL: {} chars \u{2248} {} tokens (~4 chars/token)\n",
            total_chars, total_tokens_est
        );
        eprintln!(
            "{:<28} {:>8} {:>10} {:>8}",
            "Section", "chars", "tokens~", "% total"
        );
        eprintln!("{}", "-".repeat(60));

        if let Some(&(_, first_pos)) = positions.first() {
            // Base prompt (everything before first marker).
            let base_chars = first_pos;
            let base_tokens = base_chars / 4;
            let pct = (base_chars as f64 / total_chars as f64) * 100.0;
            eprintln!(
                "{:<28} {:>8} {:>10} {:>7.1}%",
                "Base prompt", base_chars, base_tokens, pct
            );
        }

        for (i, &(name, pos)) in positions.iter().enumerate() {
            let end = positions
                .get(i + 1)
                .map(|&(_, p)| p)
                .unwrap_or(prompt.len());
            let span = end - pos;
            let toks = span / 4;
            let pct = (span as f64 / total_chars as f64) * 100.0;
            eprintln!("{:<28} {:>8} {:>10} {:>7.1}%", name, span, toks, pct);
        }

        eprintln!("\n========== END MEASUREMENT ==========\n");

        // Sanity: prompt should be non-empty + contain expected sections.
        assert!(total_chars > 1000, "prompt suspiciously short");
        assert!(prompt.contains("## Skills"));
    }
}
