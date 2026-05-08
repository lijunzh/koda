//! Sub-agent invocation and discovery tools.
//!
//! Exposes `InvokeAgent` and `ListAgents` as tools the LLM can call.
//! `SpawnAgent` (the codex-v2 peer-spawn shape) is a synonym defined
//! in `tools/spawn_agent.rs`; both names route through the same
//! dispatch path. Actual sub-agent execution is handled by the event
//! loop since it needs access to config, DB, and the provider.
//!
//! ## Usage patterns
//!
//! - **Delegate a task**: `InvokeAgent { agent_name: "task", prompt: "write tests for auth.rs" }`
//! - **Use a specialist**: `InvokeAgent { agent_name: "explore", prompt: "find all error handling" }`
//! - **Fork context**: `InvokeAgent { agent_name: "fork", prompt: "..." }`
//!   (inherits parent's full conversation)
//!
//! All sub-agents run in the **background**. The tool returns IMMEDIATELY
//! with a task_id; results auto-inject as a user message on a future
//! iteration. If you genuinely need to block until completion, call
//! `WaitForMail` — every bg-agent exit sends a completion mail to the
//! parent's mailbox via the bridge from #1336 (`notify_parent_mailbox`),
//! so `WaitForMail` is the single tool the model needs to wait on
//! background work.
//!
//! `agent_name` is **required** — see #1232 §5 for rationale. The previous
//! `background:bool` flag was removed in #1163 (Lean A: koda matches
//! Codex's `spawn_agent` and Claude Code's `TaskCreate` model — one shape
//! per call, no foreground / blocking variant).
//!
//! Pre-#1325 Phase 5b also exposed `WaitTask` / `ListBackgroundTasks` /
//! `CancelTask` for managing in-flight bg work; Phase 5b retired the
//! trio in favor of `WaitForMail`. See `tools/mod.rs` for the migration
//! story.
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
            description: "Spawn a sub-agent to work on a task in the background.

Returns IMMEDIATELY with a task_id. The sub-agent runs concurrently; \
you keep working in parallel. Results inject as a user message at the \
start of a future iteration via auto-drain \u{2014} you do NOT need to call \
`WaitForMail` unless you have nothing else useful to do.

EXECUTION MODEL

- All sub-agents run in the **background**. There is no foreground / blocking \
  mode \u{2014} spawn the agent, get a task_id back, and continue your own reasoning. \
  This matches Codex's `spawn_agent` and Claude Code's `TaskCreate` model: one \
  shape per call, no `background:bool` flag to think about.
- `SpawnAgent` is an alias for `InvokeAgent` with a codex-compatible argument \
  shape (`task_name` + `message` instead of `agent_name` + `prompt`). Same \
  dispatch path, same execution model. Use whichever your skill manifest \
  exposes.
- Emit multiple `InvokeAgent` calls in the same assistant message to fan out \
  N agents in parallel. Each write-capable agent gets its own isolated \
  workspace, so parallel write-agents cannot trample each other.
- After spawning, KEEP WORKING. Do follow-up searches, edit files, summarize \
  progress \u{2014} anything useful. Results inject naturally on a future iteration. \
  Calling `WaitForMail` immediately after spawning defeats the purpose.
- **Don't peek. Don't race. Trust the notification.** (Pattern adopted from \
  Claude Code's `AgentTool`.) After spawning a background sub-agent, you \
  know nothing about what it found. Never fabricate or predict its results \
  in any format \u{2014} not as prose, summary, or structured output. The \
  notification arrives as a user-role message in a later turn; it is never \
  silently filled in. If the user asks mid-wait \"what did the agent find?\", \
  give status (\"Still running, last status was X\"), not a fabricated answer.
- Use `WaitForMail` ONLY when:
    1. You have genuinely run out of useful concurrent work, AND
    2. The next step strictly depends on the sub-agent's output.
  Otherwise let auto-drain do its job. `WaitForMail` blocks the current \
  turn until any mail arrives in your mailbox \u{2014} every bg-agent exit \
  sends a completion mail (the bridge from #1336), so it will unblock as \
  soon as the first sub-agent finishes.
- **NEVER call `WaitForMail` twice in a row.** If the first call timed out \
  with `bg_agents_in_flight > 0`, end your turn cleanly. The bridge is \
  reliable; auto-drain WILL deliver the result on a future iteration. \
  Repeated polling burns wall-clock + tokens and serializes work that was \
  supposed to run in parallel.
- `agent_name='fork'` inherits your full conversation context. Useful when \
  the sub-agent needs everything you've already loaded.

WHEN TO USE InvokeAgent

- The task requires exploring many files or running many searches that would pollute your context
- Work is independent and can run in parallel with your current reasoning
- A specialist persona adds value (`explore` for search, `plan` for architecture, `verify` for testing)

AGENT SELECTION

- For read-only exploration / search / analysis, prefer `explore` (faster, cheaper, no isolated workspace)
- For implementation that needs file writes, use `task` (general-purpose worker; isolated worktree)

WHEN NOT TO USE InvokeAgent

- A single Read, Grep, or Glob would answer the question (overhead > benefit)
- The task requires real-time back-and-forth with the user (sub-agents have no way to ask questions; AskUser is filtered from their tool set)
- You've already loaded the relevant context (just do the work yourself)

KEY RULES

- Sub-agent activity is HIDDEN from the user's transcript by default. The user sees \
  a one-line summary on completion (or the full output on failure). You must \
  summarize sub-agent results in your own reply if the user needs to see them.
- Sub-agents CANNOT spawn other sub-agents. Plan all fan-out at this level; \
  the `InvokeAgent` tool is filtered from every sub-agent's tool set.
- Identical (agent_name, prompt) pairs hit a cache and skip the LLM call. \
  Cheap to retry idempotent tasks; no need to memoize yourself.
- A task_id starting with '[ERROR: sub-agent ...]' is a structural failure \
  (e.g. workspace setup, isolation issue), not a model answer. Re-strategize \
  rather than treat as content.
- Always write a clear, self-contained prompt \u{2014} the sub-agent hasn't seen \
  your conversation. Include specific file paths, function names, and success criteria.
- `agent_name` is REQUIRED. Pick a specialist from the 'Available Sub-Agents' \
  list in your system prompt (typically: explore, plan, task, verify). Use \
  `fork` to inherit the full parent context."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "agent_name": {
                        "type": "string",
                        "description": "REQUIRED. Name of the sub-agent to dispatch to. \
                            Pick a specialist from the 'Available Sub-Agents' list in your system \
                            prompt \u{2014} commonly `explore` (read-only search/analysis), `plan` \
                            (architecture / step-by-step design), `task` (general-purpose worker, \
                            full write access), `verify` (adversarial review). Use `fork` to \
                            inherit the full parent conversation context.\n\n\
                            **#1232 \u{00a7}5**: this field is required \u{2014} there is no default. \
                            Pre-fix the field silently defaulted to `task`, so every InvokeAgent \
                            call routed to the generic worker even when the model's prompt was \
                            written for a specialist (\"Rust code architect\", \"security \
                            specialist\", etc.). Forcing the choice surfaces routing intent at \
                            the call site (Zen of Python: explicit is better than implicit)."
                    },
                    "prompt": {
                        "type": "string",
                        "description": "The task to delegate to the sub-agent"
                    }
                },
                "required": ["agent_name", "prompt"]
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
        // (Pre-#1232 §5 there was also no "omitted agent_name" path
        // routing here — dispatch now requires the field.)
        if name == "default" {
            continue;
        }
        // **#1232 §5 (drive-by)**: prefer the explicit `description`
        // field over the heuristic `extract_description(system_prompt)`.
        // The four built-in JSONs (`explore`, `plan`, `task`, `verify`)
        // all carry rich, model-facing one-liners in their
        // `description` fields, but pre-fix this branch ignored them
        // and showed the heuristic's first-sentence guess instead
        // (often "You are a ..."). The disk-load branch in
        // `load_agents_from_dir` already does this fallback dance
        // — this aligns the built-in branch to match.
        let description = config
            .description
            .as_deref()
            .filter(|d| !d.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| extract_description(&config.system_prompt));
        agents.insert(
            name.clone(),
            AgentInfo {
                name,
                description,
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

// =============================================================
// Tool trait implementations (#1265 item 5, PR-8/N).
//
// `ListAgents` is read-only (it scans the agents directory).
//
// `InvokeAgent` is **special** — it's intercepted by
// `tool_dispatch.rs` before reaching the registry, because it needs
// to spawn a sub-agent (which the registry can't do). The trait
// impl below preserves the pre-#1265 "this branch should not be
// reached in normal flow" failure path: the dispatch fast path
// will only ever invoke this if something has gone seriously
// wrong upstream.
// =============================================================

use crate::tools::{Tool, ToolEffect, ToolExecCtx, ToolResult};
use async_trait::async_trait;

/// `ListAgents` — enumerate sub-agents from project + user dirs.
pub struct ListAgentsTool;

#[async_trait]
impl Tool for ListAgentsTool {
    fn name(&self) -> &'static str {
        "ListAgents"
    }
    fn definition(&self) -> ToolDefinition {
        definitions()
            .into_iter()
            .find(|d| d.name == "ListAgents")
            .expect("agent::definitions() must contain ListAgents")
    }
    fn classify(&self, _args: &serde_json::Value) -> ToolEffect {
        ToolEffect::ReadOnly
    }
    async fn execute(&self, ctx: &ToolExecCtx<'_>, args: &serde_json::Value) -> ToolResult {
        let detail = args["detail"].as_bool().unwrap_or(false);
        let output = if detail {
            list_agents_detail(ctx.project_root)
        } else {
            let agents = list_agents(ctx.project_root);
            if agents.is_empty() {
                "No sub-agents configured.".to_string()
            } else {
                agents
                    .iter()
                    .map(|(name, desc, source)| {
                        if source == "built-in" {
                            format!("  {name} \u{2014} {desc}")
                        } else {
                            format!("  {name} \u{2014} {desc} [{source}]")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        };
        ToolResult {
            output,
            success: true,
            full_output: None,
        }
    }
}

/// `InvokeAgent` — intercepted by `tool_dispatch.rs`. This trait
/// impl exists only to make the catalog complete; it preserves the
/// pre-#1265 "this branch should not be reached in normal flow"
/// behavior (success=false with a self-explanatory message).
pub struct InvokeAgentTool;

#[async_trait]
impl Tool for InvokeAgentTool {
    fn name(&self) -> &'static str {
        "InvokeAgent"
    }
    fn definition(&self) -> ToolDefinition {
        definitions()
            .into_iter()
            .find(|d| d.name == "InvokeAgent")
            .expect("agent::definitions() must contain InvokeAgent")
    }
    fn classify(&self, _args: &serde_json::Value) -> ToolEffect {
        // Sub-agents inherit the parent's approval mode; classification
        // here is a placeholder — dispatch never asks.
        ToolEffect::ReadOnly
    }
    async fn execute(&self, _ctx: &ToolExecCtx<'_>, _args: &serde_json::Value) -> ToolResult {
        ToolResult {
            output: "InvokeAgent is handled by the inference loop.".to_string(),
            success: false,
            full_output: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Trait invariants (#1265 PR-8) ─────────────────────────

    #[test]
    fn list_agents_tool_metadata() {
        let t = ListAgentsTool;
        assert_eq!(t.name(), "ListAgents");
        assert_eq!(t.definition().name, "ListAgents");
        assert_eq!(
            t.classify(&serde_json::json!({})),
            crate::tools::ToolEffect::ReadOnly,
        );
    }

    /// `InvokeAgent` is intercepted upstream of the registry.
    /// If dispatch ever falls through to the trait impl, it must
    /// preserve the pre-#1265 "should not be reached" failure path:
    /// success=false with the same message verbatim.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn invoke_agent_unreached_path_returns_failure() {
        let t = InvokeAgentTool;
        let tmp = tempfile::tempdir().unwrap();
        let cache = crate::tools::FileReadCache::default();
        let fs = koda_sandbox::fs::LocalFileSystem;
        let caps = crate::output_caps::OutputCaps::for_context(100_000);
        let bg = crate::tools::bg_process::BgRegistry::new();
        let trust = crate::trust::TrustMode::Safe;
        let policy = koda_sandbox::SandboxPolicy::default();
        let skills = crate::skills::SkillRegistry::default();
        let agent_path = crate::agent::AgentPath::root();
        let ctx = crate::tools::ToolExecCtx::for_test(
            tmp.path(),
            &cache,
            &fs,
            &caps,
            &bg,
            &trust,
            &policy,
            &skills,
            &agent_path,
        );
        let r = t.execute(&ctx, &serde_json::json!({})).await;
        assert!(!r.success);
        assert_eq!(r.output, "InvokeAgent is handled by the inference loop.");
    }

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
    ///
    /// **#1163 (Lean A)**: koda no longer has a foreground/blocking mode.
    /// All sub-agents run in the background. The description must say so
    /// clearly, surface the auto-drain mechanic, and explain when the
    /// blocking tool IS appropriate (rather than its old role as a
    /// foreground replacement).
    ///
    /// **#1325 Phase 5b**: the blocking tool is now `WaitForMail`
    /// (mailbox bridge from #1336), not the retired `WaitTask`.
    #[test]
    fn test_invoke_agent_description_documents_spawn_only_model() {
        let defs = definitions();
        let desc = &defs[0].description;
        // The spawn-only execution model — no fg / blocking variant.
        assert!(
            desc.contains("background") || desc.contains("Background"),
            "description must declare sub-agents run in the background"
        );
        assert!(
            !desc.contains("Sequential foreground")
                && !desc.contains("Parallel foreground")
                && !desc.contains("foreground (default)"),
            "description must NOT name foreground modes — #1163 deleted them"
        );
        assert!(
            desc.contains("task_id") || desc.contains("task ID"),
            "description must surface the task_id return contract so the \
             model knows it's spawn-and-poll, not blocking"
        );
        assert!(
            desc.contains("parallel") || desc.contains("concurrent"),
            "description must explain that multiple InvokeAgent calls in \
             one assistant message fan out concurrently — the only way to \
             parallelize after the fg mode was removed"
        );
        // Fork is still a thing.
        assert!(
            desc.contains("fork"),
            "description must name the fork agent and its context-inheritance role"
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

    /// **#1163 (Lean A)**: drain semantics now live in the top-level
    /// description (since there's no `background` parameter to anchor
    /// them to). The drain-on-future-iteration timing is still load-
    /// bearing — the model shouldn't expect mid-iteration results from
    /// a freshly-spawned sub-agent.
    #[test]
    fn test_invoke_agent_top_level_documents_drain_semantics() {
        let defs = definitions();
        let desc = &defs[0].description;
        assert!(
            desc.contains("future iteration") || desc.contains("next iteration"),
            "description must explain drain-on-next-iteration timing; \
             post-#1163 wording uses 'future iteration' since the drain \
             can land any iteration after the spawn, not strictly the next one"
        );
    }

    /// **#1163 (Lean A)**: the schema must NOT carry a `background`
    /// property anymore. Pre-#1163 the field was `required`; removing
    /// it from the schema is the API contract change models will see
    /// first. A regression that re-adds `background` (e.g. a copy-paste
    /// from the v0.3 schema) would re-introduce the asymmetric
    /// foreground/background behaviour this PR was specifically built to
    /// delete.
    #[test]
    fn test_invoke_agent_schema_does_not_carry_background_param() {
        let defs = definitions();
        let props = defs[0]
            .parameters
            .pointer("/properties")
            .and_then(|v| v.as_object())
            .expect("InvokeAgent schema must declare /properties");
        assert!(
            !props.contains_key("background"),
            "#1163: `background` parameter was deleted — sub-agents always \
             run in the background. Found properties: {:?}",
            props.keys().collect::<Vec<_>>()
        );
        let required = defs[0]
            .parameters
            .pointer("/required")
            .and_then(|v| v.as_array())
            .expect("InvokeAgent schema must declare a `required` array");
        let names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            names.contains(&"prompt") && names.contains(&"agent_name"),
            "`prompt` and `agent_name` must remain required (regression guard). \
             Got required = {names:?}"
        );
        assert!(
            !names.contains(&"background"),
            "#1163: `background` must NOT be in the required list — the \
             field was deleted. Got required = {names:?}"
        );
    }

    /// #1201 A1: nudge the model away from "spawn agent → immediately
    /// WaitTask". That pattern silences the parent for the entire wait
    /// and turns the non-blocking dispatch back into a blocking call.
    ///
    /// **#1163 (Lean A) update**: with the `background` parameter deleted,
    /// the nudge can only live in the top-level description. Both the
    /// blocking-tool reference and the auto-drain mechanic must remain
    /// there.
    ///
    /// **#1325 Phase 5b update**: the blocking tool is now `WaitForMail`,
    /// not `WaitTask` (the bg-task management trio was retired — see
    /// `tools/mod.rs` for the migration story). The anti-pattern the
    /// description guards against is identical ("spawn then immediately
    /// block"); only the tool name changed.
    #[test]
    fn test_invoke_agent_description_discourages_immediate_wait() {
        let defs = definitions();
        let desc = &defs[0].description;
        assert!(
            desc.contains("WaitForMail"),
            "top-level description must reference WaitForMail by name so the \
             model can recognize the spawn-then-immediately-wait \
             anti-pattern (#1201 A1 / #1325 Phase 5b)"
        );
        assert!(
            !desc.contains("WaitTask"),
            "top-level description must NOT reference WaitTask \u{2014} that tool \
             was retired in #1325 Phase 5b. A regression that re-introduces \
             the name would route the model to a tool that no longer exists."
        );
        assert!(
            desc.contains("auto-drain"),
            "top-level description must name the auto-drain path so the \
             model knows results arrive without explicit waiting (#1201 A1)"
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

        // (4) The InvokeAgent tool description still pins `task`
        // as the omitted-agent_name fallback. If someone renames
        // the agent, the docs and the dispatch behavior must be
        // updated together — this catches half-migrations.
        // Accept either single-quoted ('task') or backticked (`task`)
        // form — #1163 switched the description to backticks for
        // consistency, but earlier copies used single quotes.
        let invoke_desc = &definitions()[0].description;
        assert!(
            invoke_desc.contains("'task'") || invoke_desc.contains("`task`"),
            "InvokeAgent description must reference `task` as the omitted-agent_name fallback worker. If you renamed `task`, update the schema and this test together."
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
