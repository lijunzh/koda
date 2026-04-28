//! Sub-agent invocation and discovery tools.
//!
//! Exposes `InvokeAgent` and `ListAgents` as tools the LLM can call.
//! Actual sub-agent execution is handled by the event loop since it needs
//! access to config, DB, and the provider.
//!
//! ## Usage patterns
//!
//! - **Delegate a task**: `InvokeAgent { prompt: "write tests for auth.rs" }`
//!   (uses the `task` agent by default)
//! - **Use a specialist**: `InvokeAgent { agent_name: "explore", prompt: "find all error handling" }`
//! - **Fork context**: `InvokeAgent { agent_name: "fork", prompt: "..." }`
//!   (inherits parent's full conversation)
//! - **Background work**: `InvokeAgent { prompt: "...", background: true }`
//!   (returns immediately, results injected when complete)
//!
//! ## When to use sub-agents
//!
//! - Complex multi-step tasks (keeps parent context clean)
//! - Independent parallel work (launch multiple agents at once)
//! - Research that generates lots of noise (grep results, file contents)
//!
//! ## When NOT to use sub-agents
//!
//! - Simple file reads or 2–3 grep queries (overhead > benefit)
//! - Tasks requiring user interaction (sub-agents can't ask questions)

use crate::providers::ToolDefinition;
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Return tool definitions for the LLM.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "InvokeAgent".to_string(),
            description: "Delegate a task to a specialized sub-agent.

EXECUTION MODES (pick one per call):
- Sequential foreground (default): one sub-agent runs, blocks until done.
- Parallel foreground: emit multiple InvokeAgent tool calls in the same
  message and they run concurrently. Each write-capable agent gets its own
  isolated workspace, so parallel write-agents cannot trample each other.
- Background (background=true): returns immediately. Results inject as a
  user message on the next iteration. Use for long-running independent work.
- Forked context (agent_name='fork'): inherits your full conversation
  history. Useful when the sub-agent needs everything you've already loaded.

Use InvokeAgent when:
- The task requires exploring many files or running many searches that would pollute your context
- Work is independent and can run in parallel with your current reasoning
- A specialist persona adds value (explore for search, plan for architecture, verify for testing)

Do NOT use InvokeAgent when:
- A single Read, Grep, or Glob would answer the question (overhead > benefit)
- The task requires real-time back-and-forth with the user (sub-agents have no way to ask questions; AskUser is filtered from their tool set)
- You've already loaded the relevant context (just do the work yourself)

Key rules:
- Sub-agent results are NOT shown to the user — you must summarize them in your reply
- Sub-agents CANNOT spawn other sub-agents. Plan all fan-out at this level; the InvokeAgent tool is filtered from every sub-agent's tool set.
- Identical (agent_name, prompt) calls hit a cache and skip the LLM call. Cheap to retry idempotent tasks; no need to memoize yourself.
- A result starting with '[ERROR: sub-agent ...]' is a structural failure (e.g. iteration cap, workspace setup), not a model answer. Re-strategize rather than treat as content.
- Always write a clear, self-contained prompt — the sub-agent hasn't seen your conversation
- Include specific file paths, function names, and success criteria in your prompt
- Omit agent_name to use the 'task' worker (full write access)"
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "agent_name": {
                        "type": "string",
                        "description": "Name of the sub-agent (from ListAgents). Omit for 'task', use 'fork' to inherit parent context."
                    },
                    "prompt": {
                        "type": "string",
                        "description": "The task to delegate to the sub-agent"
                    },
                    "background": {
                        "type": "boolean",
                        "description": "Run in background and return immediately (default: false). \
                            Results are drained and injected as a user message at the start of \
                            the next iteration — NOT mid-iteration. The bg agent inherits the \
                            parent's trust + sandbox at spawn time and is cancelled on Ctrl+C. \
                            Use for independent long-running tasks that don't block your current work."
                    }
                },
                "required": ["prompt"]
            }),
        },
        ToolDefinition {
            name: "ListAgents".to_string(),
            description: "List available sub-agents. Use detail=true to see system prompts."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "detail": {
                        "type": "boolean",
                        "description": "Show full system prompts"
                    }
                }
            }),
        },
    ]
}

/// Agent info from discovery: name, description, source, and optionally the full prompt.
pub struct AgentInfo {
    /// Agent name (used in `InvokeAgent` tool calls).
    pub name: String,
    /// One-line description shown in `ListAgents` output.
    pub description: String,
    /// Discovery source: `"built-in"`, `"user"`, or `"project"`.
    pub source: &'static str,
    /// Full system prompt content.
    pub system_prompt: String,
}

/// Discover all agents from all sources, with project > user > built-in priority.
pub fn discover_all_agents(project_root: &Path) -> Vec<AgentInfo> {
    let mut agents: HashMap<String, AgentInfo> = HashMap::new();

    // 1. Built-in agents (lowest priority)
    for (name, config) in crate::config::KodaConfig::builtin_agents() {
        // Skip `default` — it's the main agent, not a sub-agent.
        // Map omitted agent_name to `task` instead (see InvokeAgent schema).
        if name == "default" {
            continue;
        }
        agents.insert(
            name.clone(),
            AgentInfo {
                name,
                description: extract_description(&config.system_prompt),
                source: "built-in",
                system_prompt: config.system_prompt,
            },
        );
    }

    // 2. User agents (~/.config/koda/agents/) — overrides built-ins
    if let Ok(user_dir) = user_agents_dir() {
        load_agents_from_dir(&user_dir, "user", &mut agents);
    }

    // 3. Project agents (<project>/agents/) — highest priority
    let project_dir = project_root.join("agents");
    load_agents_from_dir(&project_dir, "project", &mut agents);

    let mut result: Vec<AgentInfo> = agents.into_values().collect();
    result.sort_by(|a, b| a.name.cmp(&b.name));
    result
}

/// Load agents from a directory into the map (later calls override earlier).
fn load_agents_from_dir(dir: &Path, source: &'static str, agents: &mut HashMap<String, AgentInfo>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(agent_name) = name.strip_suffix(".json") else {
            continue;
        };
        // `default` and `koda` are reserved names for the main agent
        // identity — they are NOT sub-agents and must not appear in
        // discovery output (the `/agents` listing, the prompt's
        // `## Available Sub-Agents` section, or `InvokeAgent` dispatch).
        // Pre-#1098, the prompt builder filtered both names locally;
        // now that all callers route through `discover_all_agents`,
        // the filter belongs here.
        if agent_name == "default" || agent_name == "koda" {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(config) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        let prompt = config["system_prompt"].as_str().unwrap_or("").to_string();
        // Prefer the JSON's explicit `description` field over the
        // heuristic that scrapes the system_prompt. Agent authors
        // who took the trouble to write an explicit description
        // (e.g. for sub-agent dispatch hints to the model) deserve
        // to have it honored. The heuristic is a fallback for agents
        // that don't supply one.
        let description = config["description"]
            .as_str()
            .map(str::to_string)
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| extract_description(&prompt));
        agents.insert(
            agent_name.to_string(),
            AgentInfo {
                name: agent_name.to_string(),
                description,
                source,
                system_prompt: prompt,
            },
        );
    }
}

/// Return the user-level agents directory path.
fn user_agents_dir() -> Result<PathBuf, std::env::VarError> {
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE"))?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("koda")
        .join("agents"))
}

/// Return agent list data for display (used by /agent command and ListAgents tool).
///
/// Returns a list of `(name, description, source)` tuples.
/// The client is responsible for formatting/coloring.
pub fn list_agents(project_root: &Path) -> Vec<(String, String, String)> {
    discover_all_agents(project_root)
        .into_iter()
        .map(|a| {
            (
                a.name.to_string(),
                a.description.to_string(),
                a.source.to_string(),
            )
        })
        .collect()
}

/// Format detailed agent list (for ListAgents with detail=true).
pub fn list_agents_detail(project_root: &Path) -> String {
    let agents = discover_all_agents(project_root);

    if agents.is_empty() {
        return "No sub-agents configured.".to_string();
    }

    let mut output = String::new();
    for a in &agents {
        output.push_str(&format!("## {} [{}]\n", a.name, a.source));
        // Show first 500 chars of prompt as template reference
        let preview: String = a.system_prompt.chars().take(500).collect();
        output.push_str(&preview);
        if a.system_prompt.len() > 500 {
            output.push_str("\n[...truncated]");
        }
        output.push_str("\n\n");
    }
    output
}

/// Extract a clean one-line description from a system prompt.
/// Looks for "Your job is to ..." or falls back to the first sentence.
fn extract_description(prompt: &str) -> String {
    // Try to find "Your job is to ..." pattern
    if let Some(idx) = prompt.find("Your job is to ") {
        let rest = &prompt[idx + "Your job is to ".len()..];
        let end = rest.find('.').unwrap_or(rest.len().min(80));
        let desc: String = rest[..end].chars().take(80).collect();
        return capitalize_first(&desc);
    }

    // Try "You are a ..." pattern — extract the role
    if let Some(idx) = prompt.find("You are a ") {
        let rest = &prompt[idx + "You are a ".len()..];
        let end = rest.find('.').unwrap_or(rest.len().min(60));
        let role: String = rest[..end].chars().take(60).collect();
        return capitalize_first(&role);
    }

    // Fallback: first line, capped
    let first_line = prompt.lines().next().unwrap_or("");
    let capped: String = first_line.chars().take(60).collect();
    capped
}

/// Capitalize the first character of a string.
fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().to_string() + chars.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_definitions_count() {
        let defs = definitions();
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].name, "InvokeAgent");
        assert_eq!(defs[1].name, "ListAgents");
    }

    /// Pin the load-bearing pieces of the InvokeAgent description so future
    /// "tighter wording" refactors don't silently drop the bits the model
    /// needs to dispatch correctly. We don't pin exact wording — just the
    /// concepts that have engineering meaning behind them.
    #[test]
    fn test_invoke_agent_description_documents_all_four_modes() {
        let defs = definitions();
        let desc = &defs[0].description;
        // The four execution modes are the user-facing vocabulary the
        // engine actually implements (sub_agent_dispatch.rs + bg_agent.rs).
        assert!(
            desc.contains("Sequential foreground"),
            "description must name the sequential foreground mode"
        );
        assert!(
            desc.contains("Parallel foreground"),
            "description must name the parallel foreground mode"
        );
        assert!(
            desc.contains("Background") && desc.contains("background=true"),
            "description must explain background dispatch and the parameter"
        );
        assert!(
            desc.contains("Forked context") && desc.contains("agent_name='fork'"),
            "description must name fork mode and its trigger"
        );
    }

    #[test]
    fn test_invoke_agent_description_warns_about_no_nested_invocation() {
        // Sub-agents cannot spawn other sub-agents (DESIGN.md invariant).
        // The model needs to know this so it doesn't try a workaround that
        // hits the empty-tool refusal at runtime.
        let defs = definitions();
        let desc = &defs[0].description;
        assert!(
            desc.contains("CANNOT spawn other sub-agents") || desc.contains("cannot spawn"),
            "description must surface the no-nested-invocation rule"
        );
    }

    #[test]
    fn test_invoke_agent_description_explains_error_marker_convention() {
        // The [ERROR: ...] marker (B18, B21) is structural failure metadata,
        // not a model answer. The model needs to know that so it
        // re-strategizes instead of treating the marker as content.
        let defs = definitions();
        let desc = &defs[0].description;
        assert!(
            desc.contains("[ERROR: sub-agent"),
            "description must explain the [ERROR: marker so the model knows to re-strategize"
        );
    }

    #[test]
    fn test_invoke_agent_description_mentions_result_caching() {
        // SubAgentCache lives on KodaSession and survives across turns.
        // The model should know calls are memoized so it doesn't build its
        // own (worse) memoization on top.
        let defs = definitions();
        let desc = &defs[0].description;
        assert!(
            desc.contains("cache") || desc.contains("memoize"),
            "description must mention result caching so the model doesn't roll its own"
        );
    }

    #[test]
    fn test_invoke_agent_background_param_documents_drain_semantics() {
        // The drain-on-next-iteration timing is load-bearing: the model
        // shouldn't expect mid-iteration results from a bg agent.
        let defs = definitions();
        let bg_desc = defs[0]
            .parameters
            .pointer("/properties/background/description")
            .and_then(|v| v.as_str())
            .expect("background param must have a description");
        assert!(
            bg_desc.contains("next iteration"),
            "background param must explain drain-on-next-iteration timing"
        );
    }

    #[test]
    fn test_list_agents_has_builtins() {
        let dir = TempDir::new().unwrap();
        let result = list_agents(dir.path());
        let builtins: Vec<_> = result
            .iter()
            .filter(|(_, _, src)| src == "built-in")
            .collect();
        assert_eq!(
            builtins.len(),
            4,
            "Expected task/explore/plan/verify built-ins"
        );
        let names: Vec<&str> = result.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(names.contains(&"task"));
        assert!(names.contains(&"explore"));
        assert!(names.contains(&"plan"));
        assert!(names.contains(&"verify"));
        // Default is always excluded from listing
        assert!(!names.contains(&"default"), "Should exclude default agent");
    }

    #[test]
    fn test_list_agents_project_overrides_builtin() {
        let dir = TempDir::new().unwrap();
        let agents_dir = dir.path().join("agents");
        std::fs::create_dir(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("reviewer.json"),
            r#"{"name":"reviewer","system_prompt":"You are a custom project reviewer. Your job is to do project-specific reviews."}"#,
        ).unwrap();
        let result = list_agents(dir.path());
        let reviewer = result.iter().find(|(n, _, _)| n == "reviewer");
        assert!(reviewer.is_some());
        assert_eq!(
            reviewer.unwrap().2,
            "project",
            "Project agent should be tagged"
        );
    }

    #[test]
    fn test_discover_all_agents_has_builtins() {
        let dir = TempDir::new().unwrap();
        let agents = discover_all_agents(dir.path());
        let builtins: Vec<_> = agents.iter().filter(|a| a.source == "built-in").collect();
        assert_eq!(
            builtins.len(),
            4,
            "Expected task/explore/plan/verify built-ins"
        );
        let names: Vec<&str> = builtins.iter().map(|a| a.name.as_str()).collect();
        assert!(names.contains(&"task"));
        assert!(names.contains(&"explore"));
        assert!(names.contains(&"plan"));
        assert!(names.contains(&"verify"));
    }

    /// Pin the contract that `task` is THE general-purpose sub-agent.
    ///
    /// Multiple code paths depend on this convention:
    ///
    /// 1. The `InvokeAgent` tool description tells the model
    ///    "Omit agent_name to use the 'task' worker" — dispatch
    ///    code routes a missing `agent_name` to `task`.
    /// 2. The system prompt's `## Available Sub-Agents` section
    ///    surfaces `task` so the model knows generic delegation
    ///    is available.
    /// 3. The `koda`/`default` slot is the **main agent**, not a
    ///    sub-agent — a model delegating to itself would be
    ///    nonsense (and a recursion footgun). They MUST NOT appear
    ///    in discovery output.
    ///
    /// Renaming `task`, removing it, or accidentally letting
    /// `koda`/`default` leak into the sub-agent listing would each
    /// silently break a different production path. This test fails
    /// loudly if any of those four invariants drift.
    #[test]
    fn task_is_general_purpose_subagent_and_main_agent_is_hidden() {
        let dir = TempDir::new().unwrap();
        let agents = discover_all_agents(dir.path());
        let names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();

        // (1) `task` exists — the omitted-agent_name dispatch target.
        assert!(
            names.contains(&"task"),
            "`task` must be discoverable — it's the fallback worker for `InvokeAgent {{ prompt: ... }}` calls without an `agent_name`. Discovered: {names:?}"
        );

        // (2) `task`'s description signals general-purpose intent so
        // the model picks it for vague delegation.
        let task = agents.iter().find(|a| a.name == "task").unwrap();
        assert!(
            task.description.to_lowercase().contains("general")
                || task.description.to_lowercase().contains("task worker")
                || task.description.to_lowercase().contains("focused"),
            "`task`'s description must signal general-purpose / fallback worker semantics so the model picks it for vague delegation. Got: {:?}",
            task.description
        );

        // (3) Main-agent slots must never surface as sub-agents.
        assert!(
            !names.contains(&"koda"),
            "`koda` is the main agent identity, NOT a sub-agent — listing it invites self-delegation footguns. Discovered: {names:?}"
        );
        assert!(
            !names.contains(&"default"),
            "`default` is the main-agent config slot, NOT a sub-agent. Discovered: {names:?}"
        );

        // (4) The InvokeAgent tool description still pins `'task'`
        // as the omitted-agent_name fallback. If someone renames
        // the agent, the docs and the dispatch behavior must be
        // updated together — this catches half-migrations.
        let invoke_desc = &definitions()[0].description;
        assert!(
            invoke_desc.contains("'task'"),
            "InvokeAgent description must reference `'task'` as the omitted-agent_name fallback worker. If you renamed `task`, update the schema and this test together."
        );
    }

    #[test]
    fn test_list_agents_detail_shows_builtins() {
        let dir = TempDir::new().unwrap();
        let result = list_agents_detail(dir.path());
        assert!(result.contains("[built-in]"));
        assert!(result.contains("task"));
        assert!(result.contains("explore"));
        assert!(result.contains("plan"));
        assert!(result.contains("verify"));
    }

    #[test]
    fn test_extract_description_job_pattern() {
        let desc =
            extract_description("You are a reviewer. Your job is to find bugs and improvements.");
        assert_eq!(desc, "Find bugs and improvements");
    }

    #[test]
    fn test_extract_description_role_pattern() {
        let desc = extract_description("You are a paranoid security auditor.");
        assert_eq!(desc, "Paranoid security auditor");
    }

    #[test]
    fn test_extract_description_fallback() {
        let desc = extract_description("Review all the code carefully.");
        assert_eq!(desc, "Review all the code carefully.");
    }
}
