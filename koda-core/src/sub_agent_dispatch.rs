//! Sub-agent invocation and lifecycle management.
//!
//! Extracted from `tool_dispatch.rs` — handles `InvokeAgent` execution,
//! worktree provisioning, and sub-agent caching.
//! Each sub-agent gets its own session, provider, and (optionally) worktree
//! for isolation. Results are cached by `(agent_name, prompt_hash)`.

use crate::approval_flow::{CommandInbox, request_approval};
use crate::config::AgentIsolation;
use crate::db::Role;
use crate::engine::{ApprovalDecision, EngineCommand, EngineEvent};
use crate::memory;
use crate::persistence::Persistence;
use crate::preview;
use crate::prompt::build_system_prompt;
use crate::providers::ChatMessage;
use crate::tool_dispatch::execute_one_tool;
use crate::tools::{self, ToolRegistry};
use crate::trust::{self, ToolApproval, TrustMode, derive_child_trust};
use crate::turn_context::ToolExecutionContext;

use anyhow::{Context, Result};
use koda_sandbox::{CwdProvider, GitWorktreeProvider, WorkspaceProvider};

use std::sync::atomic::{AtomicU32, Ordering};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Process-wide allocator for sub-agent invocation IDs.
///
/// Phase E of #996. Each `execute_sub_agent` call (foreground or
/// background) draws a fresh id; that id becomes the **spawner tag**
/// for any background work this sub-agent registers, and is the key
/// used to cancel that work when the sub-agent exits.
///
/// Top-level inference uses `None` (no invocation id). Sub-agents at
/// any nesting depth use `Some(N)`.
///
/// `u32::MAX` invocations is comfortably more than any single Code
/// Puppy session needs; we don't bother with wrap-around handling.
/// Starts at 1 so `0` can stay reserved for "unset" should the type
/// ever change.
static NEXT_INVOCATION_ID: AtomicU32 = AtomicU32::new(1);

/// Default per-sub-agent turn cap when the agent JSON does not set
/// `max_iterations` explicitly (#1135).
///
/// Matches the `DEFAULT_MAX_TURNS = 30` used by `gemini-cli`'s agent
/// runtime — see `packages/core/src/agents/types.ts:51`. Codex does
/// not cap sub-agents at all ("trust the model"), which is what koda
/// did between #1110 and this change. The bug from #1135 (read-only
/// explorer agents spinning for 100+s on broad prompts) is exactly
/// the failure mode the no-cap regime can't catch: non-identical but
/// non-progressing tool calls slip past `LoopDetector` and only stop
/// when the user gives up and hits Ctrl-C.
///
/// 30 turns is enough headroom for any reasonable read-only
/// exploration on a moderate codebase (the offending session in
/// #1135 used ~26 calls); long-running write agents that need more
/// can opt up via `"max_iterations": N` in their JSON. Mirrors
/// gemini's per-agent overrides (cli-help: 10, generalist: 20,
/// browser: 50, codebase-investigator: 50).
///
/// **NOTE**: this only applies to sub-agents. The top-level koda
/// agent's cap (currently `KodaConfig::max_iterations = 200` per
/// `config.rs:247`) is unchanged — that path runs interactively and
/// already has the `EngineEvent::LoopCapReached` extension UX.
pub(crate) const DEFAULT_SUB_AGENT_MAX_TURNS: u32 = 30;

/// Keeps the roles valid between a tool result and the grace-turn reminder.
/// Nothing was interrupted here, so `INTERRUPTED_TURN_SENTINEL` would tell the
/// model to carry on just before the reminder tells it to stop.
const GRACE_TURN_BRIDGE: &str = "[Turn budget reached.]";

/// Allocate a fresh invocation id. See [`NEXT_INVOCATION_ID`].
pub(crate) fn next_invocation_id() -> u32 {
    NEXT_INVOCATION_ID.fetch_add(1, Ordering::Relaxed)
}

/// Parse the **required** `agent_name` field out of an `InvokeAgent`
/// argument object.
///
/// **#1232 §5**: extracted from `execute_sub_agent` so the missing-
/// field / wrong-type / empty-string / unknown-agent branches can be
/// unit-tested without spinning up the full sub-agent dispatch
/// plumbing. The schema declares `agent_name` required (see
/// `tools/agent.rs::definitions`) but schema compliance is best-
/// effort across LLMs. Pre-fix the field silently defaulted to
/// `"task"` (`unwrap_or("task")`) so every InvokeAgent call routed
/// to the generic worker even when the model's prompt was written
/// for a specialist ("Rust code architect", "security specialist",
/// etc. — see the bug-review session that opened the issue: 10/10
/// calls omitted the field).
///
/// On any failure the error message lists the available agents
/// (discovered via `tools::agent::discover_all_agents`) so the model
/// can self-correct on the next turn. Discovery is project-aware
/// (built-in → user → project precedence), so a project that adds
/// custom agents under `<root>/agents/` sees them in the hint too.
///
/// Accepts only non-empty strings. Missing, null, empty, wrong-type,
/// and unknown-agent cases all bail with actionable text.
pub(crate) fn parse_agent_name_required(
    args: &serde_json::Value,
    project_root: &std::path::Path,
) -> anyhow::Result<String> {
    let raw = match args.get("agent_name") {
        Some(serde_json::Value::String(s)) => s,
        Some(serde_json::Value::Null) | None => {
            anyhow::bail!(
                "InvokeAgent: 'agent_name' is required \u{2014} no default. {hint}",
                hint = available_agents_hint(project_root),
            );
        }
        Some(other) => {
            let kind = match other {
                serde_json::Value::Null => "null",
                serde_json::Value::Bool(_) => "a boolean",
                serde_json::Value::Number(_) => "a number",
                serde_json::Value::Array(_) => "an array",
                serde_json::Value::Object(_) => "an object",
                serde_json::Value::String(_) => unreachable!("matched above"),
            };
            anyhow::bail!(
                "InvokeAgent: 'agent_name' must be a string, got {kind}. {hint}",
                hint = available_agents_hint(project_root),
            );
        }
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        anyhow::bail!(
            "InvokeAgent: 'agent_name' must be a non-empty string. {hint}",
            hint = available_agents_hint(project_root),
        );
    }
    Ok(trimmed.to_string())
}

/// Build the "Available agents: ..." suffix used by
/// `parse_agent_name_required`'s error messages.
///
/// Lists every discovered agent's name plus `fork` (the special
/// context-inheriting pseudo-agent that doesn't appear in discovery
/// because it isn't a real agent file). Sorted, comma-separated, so
/// the model gets a stable hint string it can copy-paste from.
fn available_agents_hint(project_root: &std::path::Path) -> String {
    let mut names: Vec<String> = crate::tools::agent::discover_all_agents(project_root)
        .into_iter()
        .map(|a| a.name)
        .collect();
    // `fork` is dispatched specially in `execute_sub_agent` and is
    // never a real agent on disk — surface it here so the model knows
    // it's a valid choice for context-inheriting work.
    names.push("fork".to_string());
    names.sort();
    names.dedup();
    if names.len() == 1 {
        // Should be unreachable in practice (built-ins always present)
        // but be defensive.
        format!("Available agent: {}.", names[0])
    } else {
        format!(
            "Available agents (call ListAgents for descriptions): {}.",
            names.join(", "),
        )
    }
}

/// Execute a sub-agent in its own isolated event loop.
///
/// When `parent_cache` is provided, the sub-agent shares the parent's
/// file-read cache so reads by one agent benefit all others.
///
/// Results are cached in `sub_agent_cache` keyed by `(agent_name, prompt_hash)`.
/// On cache hit, returns immediately without any LLM calls.
///
/// **#1265 item 4 PR-3**: takes the parent's [`ToolExecutionContext`]
/// instead of 11 individual ambient args. The parent's `tools` field
/// is intentionally unused — sub-agents construct their own
/// `sub_tools` registry from `sub_config.allowed_tools`.
#[tracing::instrument(skip_all, fields(agent_name, cached = false))]
pub(crate) fn execute_sub_agent<'a>(
    parent_tx: ToolExecutionContext<'a>,
    arguments: &'a str,
    cmd_rx: &'a mut mpsc::Receiver<EngineCommand>,
    parent_cache: Option<crate::tools::FileReadCache>,
    // Phase 5 PR-4 of #934: parent's effective sandbox policy. The
    // child policy is composed onto this so the child can only narrow,
    // never widen — see [`koda_sandbox::SandboxPolicy::compose`] for
    // the per-field rules. Pass `&SandboxPolicy::strict_default()`
    // when there is no meaningful parent (top-level invocation).
    parent_sandbox_policy: &'a koda_sandbox::SandboxPolicy,
    // **#1108 P2a**: parent's `InvokeAgent` tool_call_id. The
    // sub-agent's events are persisted to `session_events` with this
    // id as `parent_tool_call_id`, so the transcript renderer folds
    // them under the parent's `InvokeAgent` tool result.
    parent_tool_call_id: Option<&'a str>,
) -> impl std::future::Future<Output = Result<String>> + Send + 'a {
    async move {
        // #1265 item 4 PR-3: rebind context fields to the same names
        // the body has used since pre-refactor, so the rest of this
        // 700-line function reads exactly as before. The parent's
        // `tools` registry is intentionally NOT bound — sub-agents
        // build their own from `sub_config.allowed_tools`.
        let crate::turn_context::TurnContext {
            project_root,
            config: parent_config,
            db,
            session_id: parent_session_id,
            sink,
            sub_agent_cache,
            bg_agents,
            mode,
            session_allows,
            ..
        } = *parent_tx.turn;
        let cancel = parent_tx.turn.cancel.clone();
        let parent_spawner = parent_tx.caller_spawner;

        // Phase E of #996: allocate this invocation's id up-front. It
        // becomes the `caller_spawner` for every tool call inside
        // this sub-agent's loop, AND the `spawner` tag the cleanup
        // hook below uses to reap any orphaned bg work.
        let my_invocation_id = next_invocation_id();
        let args: serde_json::Value = serde_json::from_str(arguments)?;
        // **#1232 §5**: `agent_name` is a required field. Pre-fix the
        // dispatcher used `args["agent_name"].as_str().unwrap_or("task")`,
        // which silently routed every missing-field call to the
        // generic worker even when the model's prompt was written for
        // a specialist ("Rust code architect", "security specialist",
        // ...). The bug-review session that opened the issue showed
        // 10/10 InvokeAgent calls hit this path.
        //
        // Validation is extracted into a free function so the
        // missing / wrong-type / empty / bad-name branches can be
        // unit-tested without spinning up the full dispatch
        // plumbing. The error message lists available agents so the
        // model can self-correct on the next turn.
        let agent_name_owned = parse_agent_name_required(&args, project_root)?;
        let agent_name = agent_name_owned.as_str();
        tracing::Span::current().record("agent_name", agent_name);
        let prompt = args["prompt"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing 'prompt'"))?;
        let is_fork = agent_name == "fork";

        // Emit `SubAgentStart` before the cache lookup, so the parent's
        // sink observes the dispatch whether it resolves from the cache
        // or runs the loop. headless clients (and the e2e tests in
        // `e2e_agent_test.rs`) rely on it as the canonical "a sub-agent
        // was just dispatched" signal.
        sink.emit_from(
            parent_tool_call_id,
            EngineEvent::SubAgentStart {
                agent_name: agent_name.to_string(),
            },
        );

        // Cache lookup before anything else: a hit emits its Info on the
        // parent's sink and skips the registry, the worktree and the
        // provider call (`test_sub_agent_cache_hit_skips_llm` asserts
        // both).
        if let Some(cached) = sub_agent_cache.get(agent_name, prompt) {
            sink.emit_from(
                parent_tool_call_id,
                EngineEvent::Info {
                    message: format!("  \u{26a1} {agent_name}: cache hit, skipping LLM call"),
                },
            );
            tracing::Span::current().record("cached", true);
            return Ok(cached);
        }
        // Register the sub-agent in the shared `ChildAgentRegistry` so
        // the TUI overlay can render its row (#1232: a sub-agent that
        // ran for 1009 s with no progress signal). The guard is held
        // until this function returns, on success, a `?`-bubbled error
        // or a panic, and its `Drop` removes the entry; without RAII an
        // error path would leave a phantom "running" row.
        //
        // The cache-hit early-return above intentionally skips this: a
        // cache hit returns in microseconds, far below the overlay's
        // render cadence.
        let (fg_emitter, _fg_guard) =
            bg_agents.register_fg_with_emitter(agent_name, prompt, &cancel, parent_spawner);
        // Push `Pending` immediately so the overlay row appears
        // before the LLM call. Sending it explicitly (rather than
        // relying on the channel's initial value) means every
        // consumer that reads the event queue sees the transition.
        fg_emitter.send(crate::child_agent::AgentStatus::Pending);
        // Wrap the parent's sink so tool/info events fan out to the
        // overlay's `ChildAgentActivity` stream in addition to the
        // normal forwarded path. The wrapper borrows `sink` for the
        // duration of this scope; the original `sink` reference is
        // shadowed below so every existing emit site downstream
        // routes through the wrapper without code changes.
        let fg_sink = crate::engine::sink::FgForwardingSink::new(
            sink,
            fg_emitter.clone(),
            parent_tool_call_id.map(str::to_string),
        );
        let sink: &dyn crate::engine::EngineSink = &fg_sink;
        // Now that the registry entry exists, transition to
        // `Running { iter: 0 }` so the overlay shows the agent as
        // active rather than queued; the loop below pushes
        // `Running { iter }` on every iteration.
        fg_emitter.send(crate::child_agent::AgentStatus::Running { iter: 0 });
        // Note on terminal status: we deliberately do NOT emit a
        // `Completed`/`Errored` `ChildTaskUpdate` on return. Reason:
        // the function has ~half a dozen `return Ok(...)` sites and
        // many `?`-bubbled errors; instrumenting each is fragile and
        // refactoring the inference body into an inner async fn just
        // to capture a Result was ruled out as scope creep. Instead
        // we lean on the `_fg_guard`'s `Drop` impl, which removes
        // the entry from the registry at the instant the function
        // returns. The overlay's `build_rows` prunes activity for
        // missing task ids on the very next frame — visually
        // identical to receiving a terminal event. headless
        // clients that subscribe to the raw `ChildTaskUpdate`
        // stream see the event flow simply stop for that task_id.

        // Fork inherits parent config; named agents load their own persona
        // but fall back to the parent's provider and model for anything not
        // explicitly set in the agent JSON.
        //
        // Inheritance rules (applied only when the agent JSON leaves a field None):
        //
        // provider + base_url — inherited from parent when the agent JSON sets
        //   neither. If the agent sets its own provider or base_url (e.g. a
        //   test-only "mock" agent or a specialist routed to a different endpoint)
        //   we respect that and leave it alone.
        //
        // model — inherited from parent only when (a) the agent JSON left it
        //   unset AND (b) we are also inheriting the provider. Cross-provider
        //   model names are not portable ("gemini-2.0-flash" means nothing on
        //   Anthropic), so if the agent has its own provider we leave the model
        //   resolved from that provider's defaults.
        // #1135: per-agent turn cap. Computed alongside `sub_config`
        // because for non-fork agents we need the **raw** JSON to
        // distinguish "explicit max_iterations" from "defaulted to 200
        // in `KodaConfig::for_agent_invocation`". A tuple return keeps
        // both values in scope without a redundant disk read.
        let (sub_config, max_turns, isolation) = if is_fork {
            // Fork inherits the parent config verbatim, *except* for
            // trust — which must come from the **runtime** mode
            // (see `derive_child_trust` doc).
            //
            // **#1022 B17**: was `debug_assert!`, which is *compiled
            // out* in release builds. The fork-trust invariant is a
            // security-relevant property (a future change that
            // accidentally narrowed/widened trust between the clone
            // and use would silently ship). Promoted to `assert!`
            // — the runtime cost is a single enum equality check, so
            // there's no reason to weaken the guarantee for release.
            //
            // **#1022 B19**: pre-fix asserted `cfg.trust ==
            // parent_config.trust` after a clone, which checked the
            // wrong thing — `parent_config.trust` is the *startup*
            // value of the trust mode and ignores `/safe`/`/auto`
            // toggles. The actual invariant is "fork runs at the
            // parent's *runtime* trust", and the runtime mode is
            // `mode`. Now we explicitly write `cfg.trust` from
            // `derive_child_trust(mode, mode)` (= `mode`) and
            // assert against `mode`. The clone-then-overwrite
            // pattern is intentional: keeps the rest of
            // `parent_config` (model, base_url, system prompt
            // overrides, ...) verbatim while making the trust
            // derivation explicit and uniform with the named path.
            let mut cfg = parent_config.clone();
            cfg.trust = derive_child_trust(mode, mode);
            assert!(
                cfg.trust == mode,
                "fork must inherit parent's runtime trust mode exactly"
            );
            // Forks inherit the parent's cap (parent could be top-level
            // koda at 200 or another sub-agent at 30). We don't impose
            // the sub-agent default on a fork because the model is
            // continuing the parent's work, not starting a fresh task.
            let max_turns = cfg.max_iterations;
            (cfg, max_turns, AgentIsolation::None)
        } else {
            // Load the raw JSON first to see what the agent explicitly set.
            let raw = crate::config::KodaConfig::load_agent_json(project_root, agent_name)
                .with_context(|| format!("Failed to load sub-agent: {agent_name}"))?;

            let mut cfg = crate::config::KodaConfig::load(project_root, agent_name)
                .with_context(|| format!("Failed to load sub-agent: {agent_name}"))?;

            let agent_has_own_provider = raw.provider.is_some() || raw.base_url.is_some();

            if !agent_has_own_provider {
                // Inherit parent's provider, base_url, and (if unset) model.
                // All three travel together: model names are provider-scoped.
                let model_override = raw.model.is_none().then(|| parent_config.model.clone());
                cfg = cfg.with_overrides(
                    Some(parent_config.base_url.clone()),
                    model_override,
                    Some(parent_config.provider_type.to_string()),
                );
            }
            // else: agent opted into its own provider — use its resolved config
            // as-is. The agent JSON is responsible for any model it needs.

            // Inherit trust: child can never exceed parent's *runtime*
            // trust (#845, #1022 B19). Same pattern as Codex's
            // `apply_spawn_agent_runtime_overrides()` which copies the
            // parent's runtime sandbox_policy onto the child.
            //
            // **#1022 B19**: pre-fix used `parent_config.trust` as the
            // ceiling. That field is the *startup* trust value;
            // `cycle_trust`/`set_trust` mutate the SharedTrustMode
            // atomic but never the config field. So a user who
            // started in Auto and hit `/safe` would get sub-agents
            // clamped against the stale Auto, allowing the child to
            // run with broader privileges than the parent's *current*
            // mode. Real escalation. The runtime mode is `mode`,
            // threaded through `execute_one_tool` from the inference
            // loop — that's the only authoritative value. The helper
            // `derive_child_trust` exists to make this antipattern
            // greppable (see its doc).
            //
            // **#1246**: `cfg.trust` here is the agent's **declared**
            // mode, sourced from the optional `"trust"` field in agent
            // JSON (defaults to `Safe` for back-compat with pre-#1246
            // agent files). The built-in `explore` agent
            // declares `"trust": "plan"` so it gets kernel-enforced
            // read-only via the sandbox — strictly stronger than the
            // soft `disallowed_tools` gate (which a creative model
            // could try to bash around via `cat > foo`).
            //
            // **The parallel-fan-out invariant** (the architectural
            // payoff of #1246): two or more sub-agents that BOTH end
            // up with `effective == Plan` cannot mutate any file by
            // construction. So a parent can fan out N read-only
            // sub-agents simultaneously WITHOUT git worktrees, WITHOUT
            // copy-on-write filesystems, and WITHOUT any conflict-
            // resolution machinery — the absence of writes IS the
            // conflict-prevention. This property is load-bearing for
            // any future bg-dispatch scheduler that wants to widen
            // fan-out aggressively for `explore`/`plan`-style agents.
            // The pinning test for this invariant lives in
            // `config.rs::agent_json_trust_interacts_with_derive_child_trust_correctly`.
            let child_trust = cfg.trust;
            cfg.trust = derive_child_trust(mode, cfg.trust);
            if cfg.trust != child_trust {
                tracing::info!(
                    agent = agent_name,
                    parent = %mode,
                    child = %child_trust,
                    effective = %cfg.trust,
                    "sub-agent trust clamped to match parent",
                );
            }

            // #1135: read from the **raw** JSON so we can tell
            // "explicit `max_iterations: N`" apart from "defaulted
            // to 200 by `KodaConfig::for_agent_invocation`". Without
            // this distinction every sub-agent would inherit the
            // 200 default and the gemini-pattern fix would do
            // nothing for the agents that triggered #1135 (which
            // don't set the field).
            let max_turns = raw.max_iterations.unwrap_or(DEFAULT_SUB_AGENT_MAX_TURNS);
            (cfg, max_turns, raw.isolation)
        };

        let sub_session = {
            let sid = db
                .create_session(&sub_config.agent_name, project_root)
                .await?;
            db.set_metadata(
                &sid,
                crate::persistence::PARENT_SESSION_KEY,
                parent_session_id,
            )
            .await?;
            // Fork: copy parent conversation history into the new session.
            //
            // **#1022 B20**: was a per-row loop — N×(`insert_message`
            // + `mark_message_complete` for assistant rows), each
            // call its own fsync, on the synchronous fork hot path.
            // For a 200-message parent that's ~600 round-trips and
            // hundreds of ms of disk wait. Now a single transaction
            // via `copy_messages_into_session` (one fsync at COMMIT,
            // `completed_at` written inline for assistant rows).
            if is_fork {
                let parent_history = db.load_context(parent_session_id).await?;
                db.copy_messages_into_session(&sid, &parent_history).await?
            }
            sid
        };

        db.insert_message(&sub_session, &Role::User, Some(prompt), None, None, None)
            .await?;

        let semantic_memory = if sub_config.skip_memory {
            String::new()
        } else {
            memory::load(project_root)?
        };

        let provider = crate::providers::create_provider(&sub_config);
        // A sub-agent writes in the project, like the main agent and like
        // Claude Code's and Codex's sub-agents, so its edits land where the user
        // expects. Two cases get a git worktree instead: an agent that asks for
        // one, and any writer in Safe mode. Safe promises that nothing changes
        // without the user's OK and a sub-agent can't ask, so its work waits on
        // a branch for the user to merge. Where a worktree is needed and can't
        // be had, the sub-agent doesn't start: the project is no substitute.
        let needs_isolation = needs_worktree(
            sub_config.trust,
            edits_files(&sub_config.allowed_tools, &sub_config.disallowed_tools),
            isolation,
            mode,
        );
        // Phase 1 of #1022: explicit `+ Send + Sync` on the trait object.
        // The supertrait bound `WorkspaceProvider: Send + Sync` constrains
        // *implementors*, but Rust trait objects don't auto-inherit those
        // bounds — `Box<dyn WorkspaceProvider>` is `!Send` without the
        // explicit annotation, which makes the whole `execute_sub_agent`
        // future `!Send` and unspawnable.
        let workspace: Box<dyn WorkspaceProvider + Send + Sync> = if needs_isolation {
            Box::new(GitWorktreeProvider::new(project_root, agent_name))
        } else {
            Box::new(CwdProvider::new(project_root))
        };
        let effective_root = match workspace.provision(&sub_session).await {
            Ok(path) => {
                if path != project_root {
                    sink.emit(EngineEvent::Info {
                        message: format!(
                            "  \u{1f333} {agent_name}: isolated in {}",
                            path.display()
                        ),
                    });
                }
                path
            }
            Err(e) if needs_isolation => {
                // **#1022 B21**: pre-fix this branch silently fell
                // back to `project_root.to_path_buf()`. With write
                // tools requested, that drops the sub-agent into
                // the parent's unisolated working tree — two
                // parallel sub-agents would race on the same
                // files, exactly the corruption mode the workspace
                // provider exists to prevent. Worse, the only
                // signal was a `tracing::warn!` invisible to most
                // headless / TUI runs.
                //
                // Now: short-circuit with a structural-failure
                // marker (same `[ERROR:` shape as the iteration-cap
                // marker from B18) so the parent agent sees the
                // failure as a sub-agent result and can adapt
                // (retry without write tools, do the work itself,
                // surface to the user). Also cache it so a
                // verbatim re-dispatch with the same prompt
                // doesn't pay the failed-provision cost twice —
                // mirrors the iteration-cap caching policy.
                let reason = e.to_string();
                tracing::warn!("Workspace provision failed for sub-agent '{agent_name}': {reason}");
                sink.emit(EngineEvent::Info {
                    message: format!(
                        "  \u{26a0}\u{fe0f}  {agent_name}: workspace isolation failed, not dispatching ({reason})"
                    ),
                });
                let marker = workspace_provision_failure_marker(agent_name, &reason);
                sub_agent_cache.put(agent_name, prompt, &marker);
                return Ok(marker);
            }
            Err(e) => {
                // Read-only sub-agent (no write tools): isolation
                // wasn't requested, so falling back to project_root
                // is the *intended* behavior — there's no
                // race-on-files corruption mode without write
                // tools. Today this branch is unreachable because
                // `CwdProvider::provision` is infallible, but the
                // explicit arm documents intent and survives a
                // future read-only provider that *can* fail.
                tracing::warn!(
                    "Workspace provision failed (read-only sub-agent '{agent_name}'): {e}"
                );
                project_root.to_path_buf()
            }
        };
        let effective_root_ref = effective_root.as_path();

        let tools = {
            let registry = ToolRegistry::with_trust(
                effective_root.clone(),
                sub_config.max_context_tokens,
                sub_config.trust,
            );
            // Phase 5 PR-4 of #934: compose the parent's effective policy
            // with the child's. Per-field rules in [`SandboxPolicy::compose`]
            // (denies union, allows parent-wins, limits min, trust strictest)
            // ensure the child can never widen the parent's surface — only
            // narrow it. PR-2 installed the child policy verbatim; PR-4
            // makes the install additive over the parent so chains of
            // sub-agents accumulate restrictions monotonically.
            let composed_policy = crate::sandbox::compose_child_policy(
                parent_sandbox_policy,
                sub_config.trust,
                effective_root_ref,
            );
            let registry = registry.with_sandbox_policy(composed_policy);
            match parent_cache {
                Some(cache) => registry.with_shared_cache(cache),
                None => registry,
            }
        };
        let tool_defs = {
            let mut denied = sub_config.disallowed_tools.clone();
            // #1022 B7 (revised): sub-agents cannot spawn sub-agents.
            // Period. Originally only `is_fork` blocked `InvokeAgent`,
            // but that left a sharp edge: named sub-agents could call
            // it, the call fell through to a registry stub returning
            // `"InvokeAgent is handled by the inference loop."` with
            // `success=false`, and the model would hallucinate around
            // the bogus error.
            //
            // Allowing real recursion was the alternative considered,
            // but it requires a depth cap (~hundreds of KB of `async
            // fn` state per level), `Box::pin` on a mutually-recursive
            // future, threading `depth: u32` through five functions,
            // and the resulting design has no use case worth the
            // surface area. Codex matches this stance — their
            // sub-agents can't spawn sub-agents either. The master
            // agent at depth 0 can fire as many parallel/background
            // workers as it wants; workers complete their task and
            // report back. Flat by design.
            //
            // Filtering at the tool-def level keeps the model from
            // ever seeing the tool. The sub-agent dispatch loop also
            // contains a defense-in-depth refusal in case a rogue or
            // scripted model emits `InvokeAgent` or `SpawnAgent`
            // regardless.
            if !denied.contains(&"InvokeAgent".to_string()) {
                denied.push("InvokeAgent".to_string());
            }
            // #1325 Phase 5a: SpawnAgent re-maps to InvokeAgent at dispatch
            // time, so it must be denied here too. Without this a sub-agent
            // could call SpawnAgent → tool_dispatch re-maps → execute_sub_agent
            // → unbounded mutual recursion, silently bypassing the flat-design
            // invariant above. Deny both names so the model never sees either.
            if !denied.contains(&"SpawnAgent".to_string()) {
                denied.push("SpawnAgent".to_string());
            }
            // #1022 B8: AskUser requires a live `cmd_rx` connected to the
            // user. Sub-agents have a detached channel (foreground sub-agents
            // get `&mut mpsc::channel(1).1` from the parent dispatch path,
            // bg agents get an even more detached one). Filter the tool out
            // entirely so the model never tries to call it. Without this
            // filter the call falls through to the registry stub and the
            // sub-agent gets `"AskUser is handled by the inference loop."`
            // back as a tool result — which the model then dutifully
            // hallucinates around.
            if !denied.contains(&"AskUser".to_string()) {
                denied.push("AskUser".to_string());
            }
            // Phase E of #996 wired `caller_spawner` through the
            // dispatch layer, so the bg-task tools (ListBackgroundTasks /
            // CancelTask / WaitTask) now correctly scope to the calling
            // sub-agent's own invocation id — a sub-agent only sees
            // bg work *it* spawned. The earlier blanket denylist that
            // hid these tools from sub-agents has been removed.
            tools.get_definitions(&sub_config.allowed_tools, &denied)
        };
        let env = crate::prompt::EnvironmentInfo {
            project_root: effective_root_ref,
            model: &sub_config.model,
            platform: std::env::consts::OS,
        };
        let system_prompt = build_system_prompt(
            &instructions_for(&sub_config.system_prompt, effective_root != project_root),
            &env,
            &[], // sub-agents have no REPL commands
            &tools.skill_registry,
        );

        // #1265 item 4 PR-3: build the sub-agent's own dispatch
        // context once. Used by both `execute_one_tool` callsites in
        // the per-iteration loop below — PR-2 had to construct this
        // inline twice with identical args. Hoisted here because
        // `sub_config`, `sub_session`, and `tools` are all bound by
        // this point and don't change across iterations.
        let sub_turn = crate::turn_context::TurnContext::new(
            project_root,
            &sub_config,
            db,
            &sub_session,
            sink,
            cancel.clone(),
            sub_agent_cache,
            bg_agents,
            mode,
            &tools,
            session_allows,
        );
        let sub_tx =
            crate::turn_context::ToolExecutionContext::new(&sub_turn, Some(my_invocation_id));

        // #1135: gemini-pattern grace turn. When `iter > max_turns` we
        // append a user reminder to the message list ("you have one
        // final chance — give your best answer NOW, no more tools")
        // and run exactly one more turn. Whatever the model returns on
        // that turn is the final answer (tool calls in the grace
        // response are ignored, not dispatched). Tracked here rather
        // than via `iter`-arithmetic alone because the loop body must
        // know which iteration was "the grace one" to decide whether
        // to dispatch tools or short-circuit to a return.
        //
        // Mirrors gemini-cli's `executeFinalWarningTurn` in
        // `local-executor.ts:436`.
        let mut grace_turn_done = false;

        // #1232 §3a: pre-flight context-budget check.
        //
        // Estimate `system_prompt + tool_defs + user_prompt` size against
        // the resolved `max_context_tokens` for this sub-agent. Bail with
        // an actionable breakdown when over budget instead of letting the
        // user see a raw `400 "Context size has been exceeded"` from
        // upstream (the actual UX in the bug-review session that opened
        // #1232: 4/10 sub-agents failed this way with no usable hint).
        //
        // The estimate is heuristic and conservative — it doesn't account
        // for the response budget or any future tool-call traffic, but it
        // catches the common "baseline already exceeds the window"
        // failure mode that drove the issue. Subsequent in-loop estimates
        // (`estimate_tokens(&messages)` further down) handle the
        // grow-during-the-conversation case.
        let memory_context = crate::prompt::project_memory_context(&semantic_memory);
        let first_message = match &memory_context {
            Some(memory) => format!("{memory}\n\n{prompt}"),
            None => prompt.to_string(),
        };
        let preflight = crate::inference_helpers::estimate_subagent_preflight(
            &system_prompt,
            &tool_defs,
            &first_message,
            sub_config.max_context_tokens,
        );
        tracing::debug!(
            agent = agent_name,
            preflight = %preflight.summary(),
            "sub-agent context pre-flight"
        );
        if preflight.is_over_budget() {
            // Surface to the inference loop's normal narrative so the
            // master TUI / headless clients all see the same
            // structured signal alongside the bubbled error.
            sink.emit(EngineEvent::Info {
                message: format!(
                    "  \u{1f6d1} {agent_name}: context pre-flight failed ({})",
                    preflight.summary()
                ),
            });
            // Best-effort cleanup mirrors the cancel path below — release
            // any provisioned workspace before bubbling so we don't leak
            // a tempdir on every over-budget sub-agent invocation.
            release_workspace(
                workspace.as_ref(),
                &sub_session,
                &effective_root,
                agent_name,
                sink,
            )
            .await;
            anyhow::bail!(
                "Sub-agent '{agent_name}' context exceeds model window: {summary}. \
                 Reduce the prompt, drop tools (set `disallowed_tools` on the agent), \
                 or pick a model with a larger context window.",
                summary = preflight.summary(),
            );
        }

        // An async block so every `?` in the loop ends up below, where the
        // workspace goes back and any work left in it stays findable.
        let result: Result<String> = async {
            for iter in 1u32.. {
                // #1110: sub-agents have no hardcoded iteration cap. Termination
                // is driven by the model (clean stop, no tool calls), `LoopDetector`
                // (consecutive identical calls -> feedback -> hard stop), parent
                // cancellation, or context exhaustion. Per DESIGN.md P3:
                // "Let the model drive [...] don't scaffold around weakness."
                //
                // Push the live iteration counter so the TUI overlay and
                // headless clients reflect real progress instead of the
                // `iter: 0` placeholder sent on entry.
                fg_emitter.send(crate::child_agent::AgentStatus::Running { iter });
                // Respect parent cancellation (#286)
                if cancel.is_cancelled() {
                    // Release workspace on cancellation.
                    release_workspace(
                        workspace.as_ref(),
                        &sub_session,
                        &effective_root,
                        agent_name,
                        sink,
                    )
                    .await;
                    return anyhow::Ok("[cancelled by parent]".to_string());
                }
                let history = db.load_context(&sub_session).await?;
                let mut messages = crate::inference_helpers::assemble_messages(
                    &ChatMessage::text("system", &system_prompt),
                    &history,
                );
                crate::inference_helpers::prepend_project_memory(
                    &mut messages,
                    memory_context.as_deref(),
                );

                sink.emit(EngineEvent::SpinnerStart {
                    message: format!("  \u{1f9a5} {agent_name} thinking..."),
                });

                // #1135: append the grace-turn reminder when this iteration
                // is the one that exceeded the cap. The reminder is stuffed
                // into the OUTGOING `messages` only — it is not persisted
                // to the DB, so a future replay won't see it twice. The
                // strict "text only, no tools" framing matches gemini's
                // wording (`local-executor.ts::getFinalWarningMessage`).
                //
                // It goes out as `user`: only the leading system prompt reaches
                // the model as `system`. Anthropic would drop a later one and
                // Gemini would let it replace the system prompt.
                if iter > max_turns && !grace_turn_done {
                    grace_turn_done = true;
                    let warning = format!(
                        "You have reached the maximum number of turns ({max_turns}) for this \
                         sub-agent invocation. You have ONE final chance to complete the task. \
                         You MUST respond with your best answer NOW as plain text. DO NOT call \
                         any more tools \u{2014} any tool calls in this response will be ignored. \
                         Summarize what you found, what you would do next if you had more turns, \
                         and explain that your investigation was interrupted by the budget."
                    );
                    crate::inference_helpers::repair_alternation(&mut messages, GRACE_TURN_BRIDGE);
                    messages.push(ChatMessage::text("user", &warning));
                    tracing::warn!(
                        agent = agent_name,
                        iter,
                        max_turns,
                        "sub-agent reached max_turns; running grace turn"
                    );
                    sink.emit(EngineEvent::Info {
                        message: format!(
                            "  \u{23f3} {agent_name}: reached max turns ({max_turns}); requesting final summary"
                        ),
                    });
                }

                let chat = provider.chat(&messages, &tool_defs, &sub_config.model_settings);
                let Some(response) = unless_cancelled(&cancel, chat).await else {
                    sink.emit(EngineEvent::SpinnerStop);
                    release_workspace(
                        workspace.as_ref(),
                        &sub_session,
                        &effective_root,
                        agent_name,
                        sink,
                    )
                    .await;
                    return Ok("[cancelled by parent]".to_string());
                };
                let response = response?;
                sink.emit(EngineEvent::ModelUsage {
                    usage: (&response.usage).into(),
                });
                sink.emit(EngineEvent::SpinnerStop);

                let tool_calls_json = if response.tool_calls.is_empty() {
                    None
                } else {
                    Some(serde_json::to_string(&response.tool_calls)?)
                };

                // **koda#1101**: capture msg_id and mark the assistant
                // message complete. Pre-fix this call discarded the row
                // ID and never marked complete, so `load_context` (which
                // filters out `(role = 'assistant' AND completed_at IS
                // NULL)`) dropped every sub-agent assistant turn from
                // the next iteration's history. The orphan tool-result
                // rows then got pruned by `prune_mismatched_tool_calls`,
                // leaving the sub-agent with `[system, user]` only — it
                // re-issued the same tool call every iteration until the
                // cap. Mirrors the parent inference loop's pattern at
                // `inference.rs::mark_message_complete`. The contract is
                // pinned by `db::tests::load_context_excludes_incomplete_assistant_messages`.
                let assistant_msg_id = db
                    .insert_message(
                        &sub_session,
                        &Role::Assistant,
                        response.content.as_deref(),
                        tool_calls_json.as_deref(),
                        None,
                        Some(&response.usage),
                    )
                    .await?;
                db.mark_message_complete(assistant_msg_id).await?;
                if let Some(reasoning) = response.reasoning.as_deref() {
                    db.update_message_thinking_content(assistant_msg_id, reasoning)
                        .await?;
                }

                // #1135: if this iteration was the grace turn, return
                // immediately regardless of whether the model called tools.
                // The grace contract is exactly one extra LLM call —
                // dispatching its tool calls would defeat the budget.
                // Falls through to the normal empty-tool-calls return path
                // when the model complied (typical case after the strong
                // "text only" framing); short-circuits with a marker if the
                // model defied the instruction and tried to keep tooling.
                if grace_turn_done {
                    let result = match response.content.as_deref() {
                        Some(text) if !text.trim().is_empty() => text.to_string(),
                        _ => format!(
                            "[max_turns reached: agent exceeded the {max_turns}-turn budget \
                             and did not produce a final summary on its grace turn. \
                             {n} pending tool call(s) were dropped.]",
                            n = response.tool_calls.len(),
                        ),
                    };
                    let hint = release_workspace(
                        workspace.as_ref(),
                        &sub_session,
                        &effective_root,
                        agent_name,
                        sink,
                    )
                    .await;
                    let result = with_workspace_hint(result, hint);
                    sub_agent_cache.put(agent_name, prompt, &result);
                    return Ok(result);
                }

                if response.tool_calls.is_empty() {
                    // **#1369**: pre-fix this branch did
                    //     `response.content.unwrap_or_else(|| "(no output)".to_string())`
                    // and returned. That worked when the model produced a
                    // final summary, but a sub-agent loop can also exit
                    // here with `content == None` (or `Some("")`) when the
                    // model emits a bare stop signal after a long tool
                    // chain — a documented Gemini behavior, and reachable
                    // from any provider per `providers/{anthropic,gemini,
                    // openai_compat}.rs` which all normalize empty text
                    // to `None`. The literal sentinel `"(no output)"`
                    // then surfaced as the sub-agent's "answer", and the
                    // parent (especially after #1366 phase 1's sync
                    // dispatch) treated that string as the agent's
                    // conclusion — either re-invoking in a loop or acting
                    // on garbage. Fix mirrors `claude_code_src/src/tools/
                    // AgentTool/agentToolUtils.ts::finalizeAgentTool`
                    // (~line 297, also vendored under `peers/`): if the
                    // final assistant turn is empty, walk backward through
                    // history for the most recent assistant text. Falls
                    // through to a structured marker only if the entire
                    // run produced no assistant text at all (which means
                    // something is genuinely wrong, not just a quiet last
                    // turn). #1370 will layer a stricter `complete_task`
                    // contract on top of this; the scan-back stays as the
                    // robust fallback.
                    let result = match response.content.as_deref() {
                        Some(text) if !text.trim().is_empty() => text.to_string(),
                        _ => recover_last_assistant_text(&messages).unwrap_or_else(|| {
                            format!(
                                "[sub-agent '{agent_name}' finished after {iter} turn(s) without \
                                 producing any text response. The model may have hit a \
                                 provider-specific stop condition (e.g. Gemini's bare-stop after \
                                 long tool chains). Try rephrasing the prompt, simplifying the \
                                 task, or switching models.]"
                            )
                        }),
                    };
                    // Release workspace; surface branch hint if agent left changes.
                    let hint = release_workspace(
                        workspace.as_ref(),
                        &sub_session,
                        &effective_root,
                        agent_name,
                        sink,
                    )
                    .await;
                    let result = with_workspace_hint(result, hint);
                    // Cache the result for future identical calls
                    sub_agent_cache.put(agent_name, prompt, &result);
                    return Ok(result);
                }

                for tc in &response.tool_calls {
                    sink.emit(EngineEvent::ToolCallStart {
                        id: tc.id.clone(),
                        name: tc.function_name.clone(),
                        args: serde_json::from_str(&tc.arguments).unwrap_or_default(),
                        is_sub_agent: true,
                    });

                    // Sub-agents inherit the parent's approval mode
                    let parsed_args: serde_json::Value =
                        serde_json::from_str(&tc.arguments).unwrap_or_default();

                    // #1022 B7 (revised): defense-in-depth refusal of
                    // `InvokeAgent` and `AskUser`. Both are filtered from
                    // the sub-agent's `tool_defs` above, so a well-behaved
                    // model never emits them. A misbehaving or scripted
                    // model still might — short-circuit here with a clear
                    // refusal message instead of falling through to
                    // `execute_one_tool` (which would happily recurse for
                    // InvokeAgent) or the registry stub (which returns
                    // confusing `success=false` boilerplate).
                    if tc.function_name == "InvokeAgent" {
                        let refusal = "InvokeAgent is not available inside a sub-agent. \
                                       Sub-agents are autonomous workers and cannot spawn \
                                       further sub-agents. Complete the task directly with \
                                       the tools you have, or report back what additional \
                                       dispatch the parent agent should perform.";
                        db.insert_message(
                            &sub_session,
                            &Role::Tool,
                            Some(refusal),
                            None,
                            Some(&tc.id),
                            None,
                        )
                        .await?;
                        continue;
                    }
                    // #1325 Phase 5a: SpawnAgent re-maps to InvokeAgent at
                    // dispatch time (`tool_dispatch.rs::execute_one_tool`),
                    // so a rogue or scripted model emitting `SpawnAgent`
                    // would otherwise drive the same unbounded recursion
                    // the InvokeAgent block above prevents. Refuse with
                    // the same shape so the deny-list filter at line 1356
                    // and this defense-in-depth refusal stay symmetric.
                    if tc.function_name == "SpawnAgent" {
                        let refusal = "SpawnAgent is not available inside a sub-agent. \
                                       Sub-agents are autonomous workers and cannot spawn \
                                       further sub-agents. Complete the task directly with \
                                       the tools you have, or report back what additional \
                                       dispatch the parent agent should perform.";
                        db.insert_message(
                            &sub_session,
                            &Role::Tool,
                            Some(refusal),
                            None,
                            Some(&tc.id),
                            None,
                        )
                        .await?;
                        continue;
                    }
                    if tc.function_name == "AskUser" {
                        let refusal = "AskUser is not available inside a sub-agent. \
                                       Sub-agents have no channel to the user; the parent \
                                       agent gathers any required input before delegating. \
                                       Proceed with the information you already have or \
                                       report what's missing.";
                        db.insert_message(
                            &sub_session,
                            &Role::Tool,
                            Some(refusal),
                            None,
                            Some(&tc.id),
                            None,
                        )
                        .await?;
                        continue;
                    }

                    // #1022 B14: pre-flight validation — catch obvious errors
                    // (missing path, bad regex, file-cache violations) before
                    // we burn an approval prompt or execute. The top-level
                    // sequential dispatcher does the same; without this the
                    // sub-agent would hit the same class of errors *after*
                    // the user had already approved.
                    let validation_error = tools::validate::validate_with_registry(
                        &tools,
                        &tc.function_name,
                        &parsed_args,
                        effective_root_ref,
                    )
                    .await;

                    let output = if let Some(error) = validation_error {
                        format!("Validation error: {error}")
                    } else {
                        // #1250: sub-agent context. The dead approval channel
                        // (line 173) means `NeedsConfirmation` is meaningless
                        // here. `check_tool_for_sub_agent` resolves it per the
                        // safe-side rule: mutating ops auto-approve (the agent
                        // was invoked to do work), destructive ops block.
                        let mut approval = trust::check_tool_for_sub_agent_with_registry(
                            &tc.function_name,
                            &parsed_args,
                            mode,
                            Some(effective_root_ref),
                            &tools,
                        );
                        // A sub-agent may stop the background processes it
                        // started; `kill` alone would be blocked as destructive.
                        if approval == ToolApproval::Blocked
                            && mode != crate::trust::TrustMode::Plan
                            && tc.function_name == "Bash"
                            && crate::tools::shell::signals_only_own_background(
                                &parsed_args,
                                &tools.bg_registry,
                                sub_tx.caller_spawner,
                            )
                        {
                            approval = ToolApproval::AutoApprove;
                        }

                        // A `kill` of the agent's own processes inside a longer
                        // command can't work in the sandbox: say how instead.
                        let own_kill_hint = (mode != crate::trust::TrustMode::Plan
                            && tc.function_name == "Bash")
                            .then(|| {
                                crate::tools::shell::own_kill_in_longer_command(
                                    &parsed_args,
                                    &tools.bg_registry,
                                    sub_tx.caller_spawner,
                                )
                            })
                            .flatten();
                        if let Some(hint) = own_kill_hint {
                            hint
                        } else {
                        match approval {
                            ToolApproval::AutoApprove => {
                                // #1022 B6 + B7: route through `execute_one_tool`
                                // (instead of calling `tools.execute()` directly)
                                // so that:
                                //   - mutating tool calls invalidate the
                                //     `SubAgentCache` (B6) — otherwise an
                                //     identical follow-up `InvokeAgent` returns
                                //     a stale cached result.
                                //   - nested `InvokeAgent` from inside this
                                //     sub-agent dispatches recursively into
                                //     `execute_sub_agent` (B7), instead of
                                //     hitting the registry stub that returns
                                //     "InvokeAgent is handled by the inference
                                //     loop." with success=false.
                                //   - Bash output streams through the parent
                                //     sink (free visibility win).
                                tool_output(tc, sub_tx, &cancel).await
                            }
                            ToolApproval::Blocked => {
                                let detail =
                                    tools::describe_action(&tc.function_name, &parsed_args);
                                let diff_preview = preview::compute(
                                    &tc.function_name,
                                    &parsed_args,
                                    effective_root_ref,
                                )
                                .await;
                                sink.emit(EngineEvent::ActionBlocked {
                                    tool_name: tc.function_name.clone(),
                                    detail,
                                    preview: diff_preview,
                                });
                                "[safe mode] Action blocked.".to_string()
                            }
                            ToolApproval::NeedsConfirmation => {
                                let detail =
                                    tools::describe_action(&tc.function_name, &parsed_args);
                                let diff_preview = preview::compute(
                                    &tc.function_name,
                                    &parsed_args,
                                    effective_root_ref,
                                )
                                .await;
                                let effect = crate::trust::resolve_tool_effect_with_registry(
                                    &tc.function_name,
                                    &parsed_args,
                                    &tools,
                                );
                                // Sub-agents read a closed channel, so nothing
                                // can be queued here.
                                let mut no_steers = Vec::new();
                                match request_approval(
                                    sink,
                                    &mut CommandInbox::new(cmd_rx, &mut no_steers),
                                    &cancel,
                                    &tc.function_name,
                                    &detail,
                                    diff_preview,
                                    effect,
                                    false,
                                )
                                .await
                                {
                                    Some(
                                        ApprovalDecision::Approve
                                        | ApprovalDecision::ApproveForSession,
                                    ) => tool_output(tc, sub_tx, &cancel).await,
                                    Some(ApprovalDecision::Reject) => {
                                        "[rejected by user]".to_string()
                                    }
                                    Some(ApprovalDecision::RejectWithFeedback { feedback }) => {
                                        format!("[rejected: {feedback}]")
                                    }
                                    Some(ApprovalDecision::RejectAuto { reason }) => {
                                        // #1022 B15: same shape as the existing
                                        // sub-agent auto-reject below, so the model
                                        // sees a uniform "no human, here's why"
                                        // signal regardless of whether the auto-
                                        // rejection came from headless policy or
                                        // from a closed approval channel.
                                        format!("[auto-rejected: {reason}]")
                                    }
                                    None => {
                                        // #1022 B10: `request_approval` returns `None`
                                        // when the command channel is closed (sub-agents
                                        // don't have a live channel to the user) or
                                        // cancelled. Distinguish the two so the model
                                        // gets actionable signal instead of a generic
                                        // "[cancelled]" that looked like the user
                                        // hit Ctrl+C.
                                        if cancel.is_cancelled() {
                                            "[cancelled]".to_string()
                                        } else {
                                            format!(
                                                "[auto-rejected: '{tool}' requires user \
                                                 confirmation but this sub-agent has no \
                                                 channel to the user. The parent agent \
                                                 must pre-approve destructive operations \
                                                 or run the tool itself.]",
                                                tool = tc.function_name,
                                            )
                                        }
                                    }
                                }
                            }
                        }
                        }
                    };

                    db.insert_message(
                        &sub_session,
                        &Role::Tool,
                        Some(&output),
                        None,
                        Some(&tc.id),
                        None,
                    )
                    .await?;
                }
            }
            // #1110 + #1135: unreachable in practice. The `for iter in 1u32..`
            // range is unbounded; exits are:
            //   - `return Ok(content)` on a clean (no-tool-calls) response,
            //   - `return Ok(grace_summary)` after the #1135 grace turn,
            //   - `return Ok("[cancelled by parent]")` on parent cancellation,
            //   - `?` on a provider/persistence failure.
            // The grace-turn path guarantees termination within `max_turns + 1`
            // iterations even when the model refuses to stop on its own — the
            // class of bug from #1135.
            unreachable!("sub-agent loop exits via return; the iteration range is unbounded");
        }
        .await;

        if result.is_err() {
            release_workspace(
                workspace.as_ref(),
                &sub_session,
                &effective_root,
                agent_name,
                sink,
            )
            .await;
        }
        result
    }
}

/// Give back a sub-agent's workspace and say what became of any work in it.
/// Returns the same words for the parent model, which has to know too.
async fn release_workspace(
    workspace: &(dyn WorkspaceProvider + Send + Sync),
    slot_id: &str,
    path: &std::path::Path,
    agent_name: &str,
    sink: &dyn crate::engine::EngineSink,
) -> Option<String> {
    match workspace.release(slot_id, path).await {
        Ok(Some(hint)) => {
            sink.emit(EngineEvent::Info {
                message: format!("  \u{1f335} {agent_name}: {hint}"),
            });
            Some(hint)
        }
        Ok(None) => None,
        Err(e) => {
            tracing::warn!("Workspace release failed for '{agent_name}': {e}");
            None
        }
    }
}

/// What a sub-agent in a worktree has to know. Git keeps a worktree's index and
/// refs under the main repo's `.git`, outside the sandbox's write root, and that's
/// deliberate: a sub-agent that could write refs could move `main` in Safe.
const WORKTREE_NOTE: &str = "You are working in a git worktree of your own. Koda commits \
     what you leave here to a branch when you finish, so don't run `git add` or `git commit`: \
     the sandbox blocks them.";

/// An agent's instructions, plus what its workspace changes about them.
fn instructions_for(instructions: &str, in_worktree: bool) -> String {
    if in_worktree {
        format!("{instructions}\n\n{WORKTREE_NOTE}")
    } else {
        instructions.to_string()
    }
}

/// Whether an agent's tool lists leave it any of the file-writing tools. An empty
/// allow list means every tool, as in `get_definitions`.
fn edits_files(allowed: &[String], disallowed: &[String]) -> bool {
    ["Write", "Edit"].iter().any(|tool| {
        let allowed = allowed.is_empty() || allowed.iter().any(|t| t == tool);
        allowed && !disallowed.iter().any(|t| t == tool)
    })
}

/// Whether a sub-agent gets a git worktree rather than the project itself.
///
/// `trust` is the sub-agent's own, and in Plan it can't change anything. An agent
/// that asks for a worktree gets one whatever its tools, since Bash can write too.
/// Safe only moves agents with file tools: one that only runs commands stays in
/// the project, since a worktree has no build output to test against.
fn needs_worktree(
    trust: TrustMode,
    edits_files: bool,
    isolation: AgentIsolation,
    mode: TrustMode,
) -> bool {
    trust != TrustMode::Plan
        && (isolation == AgentIsolation::Worktree || (mode == TrustMode::Safe && edits_files))
}

/// A sub-agent's answer, with where its work went if that isn't the project.
fn with_workspace_hint(result: String, hint: Option<String>) -> String {
    match hint {
        Some(hint) => format!("{result}\n\n{hint}"),
        None => result,
    }
}

/// `work`'s result, or `None` once `cancel` fires.
///
/// A sub-agent holds a workspace that only its own cancel path gives back, so
/// it can't sit on something that doesn't listen for the cancel. If it did,
/// the turn's [`crate::inference::CANCEL_GRACE`] would drop it, workspace and
/// all.
async fn unless_cancelled<T>(
    cancel: &CancellationToken,
    work: impl std::future::Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => None,
        result = work => Some(result),
    }
}

/// Run one tool for a sub-agent. A cancel drops the call, which kills any
/// command it's running, and the loop's cancel check does the rest.
async fn tool_output(
    tc: &crate::providers::ToolCall,
    tx: crate::turn_context::ToolExecutionContext<'_>,
    cancel: &CancellationToken,
) -> String {
    match unless_cancelled(cancel, execute_one_tool(tc, tx)).await {
        Some((_id, result, _success, _full)) => result,
        None => "[cancelled]".to_string(),
    }
}

/// isolated sub-agent's workspace provider can't provision.
///
/// Replaces the pre-fix silent fallback to `project_root` — which
/// dropped the sub-agent into the parent's unisolated working
/// tree and let parallel sub-agents race on the same files.
///
/// Same `[ERROR:` prefix convention as other structural-failure
/// markers so the model treats it as failure metadata rather
/// than a sub-agent answer. Includes the failure reason so the
/// parent can adapt (e.g. switch to a read-only sub-agent if
/// the underlying issue is no git repo).
fn workspace_provision_failure_marker(agent_name: &str, reason: &str) -> String {
    format!(
        "[ERROR: sub-agent '{agent_name}' could not provision an isolated workspace and was not \
         dispatched, to avoid corrupting the parent project tree (reason: {reason}). Either \
         resolve the workspace setup issue, retry without write tools, or attempt the work \
         directly without delegating.]"
    )
}

/// Walk `messages` backward and return the most recent assistant
/// message's non-empty trimmed text content, if any.
///
/// **#1369**: used as a fallback when the final LLM response in a
/// sub-agent loop comes back with neither tool calls nor text
/// content (a documented Gemini behavior, but reachable from any
/// provider per `providers/{anthropic,gemini,openai_compat}.rs`
/// which all normalize empty content to `None`). Pre-fix the
/// dispatch loop returned the literal sentinel `"(no output)"`
/// here, which the parent then mis-treated as the sub-agent's
/// actual answer.
///
/// Mirrors the pattern from `claude_code_src/src/tools/AgentTool/
/// agentToolUtils.ts::finalizeAgentTool` (~line 297, also vendored
/// at `peers/claude_code_src/`):
///
/// > *"If the final assistant message is a pure tool_use block
/// > (loop exited mid-turn), fall back to the most recent
/// > assistant message that has text content."*
///
/// Filters out empty / whitespace-only content so a `Some("")`
/// row from the DB doesn't shadow earlier real text. Stops at
/// the first hit (most recent, which is what the model is most
/// likely treating as its conclusion).
///
/// Returns `None` only if the entire run produced no assistant
/// text — which means something is genuinely wrong (e.g. the
/// model never said anything, only called tools). Caller surfaces
/// a structured error in that case rather than an empty string.
fn recover_last_assistant_text(messages: &[ChatMessage]) -> Option<String> {
    messages
        .iter()
        .rev()
        .filter(|m| m.role == "assistant")
        .find_map(|m| {
            m.content
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(|t| t.to_string())
        })
}

#[cfg(test)]
mod recover_last_assistant_text_tests {
    //! **#1369** regression tests for [`super::recover_last_assistant_text`].
    //!
    //! The helper guards the post-#1366-phase-1 sub-agent dispatch
    //! path: when the final LLM response in a sub-agent loop comes
    //! back with neither tool calls nor text content, the dispatcher
    //! falls back to scanning prior assistant turns for the most
    //! recent useful text. Pre-#1369 this branch returned the bare
    //! sentinel `"(no output)"`, which the parent (post-sync-dispatch)
    //! treated as the sub-agent's actual answer — producing the
    //! parent-loops-on-junk behavior reported in the #1366 phase-1
    //! validation. Pinning the helper directly is much cheaper and
    //! more legible than an end-to-end dispatch test, since the
    //! actual integration sits inside the 1300-line
    //! `execute_sub_agent` loop and would need a mock provider that
    //! returns `LlmResponse { content: None, tool_calls: vec![] }`.

    use super::recover_last_assistant_text;
    use crate::providers::ChatMessage;

    fn assistant(text: &str) -> ChatMessage {
        ChatMessage::text("assistant", text)
    }

    fn user(text: &str) -> ChatMessage {
        ChatMessage::text("user", text)
    }

    fn system(text: &str) -> ChatMessage {
        ChatMessage::text("system", text)
    }

    fn assistant_no_content() -> ChatMessage {
        // Mirrors what the DB-load path produces for a tool-call-only
        // assistant turn: role is "assistant" but `content` is `None`.
        ChatMessage {
            role: "assistant".to_string(),
            content: None,
            tool_calls: None,
            tool_call_id: None,
            images: None,
            reasoning: None,
            thinking_blocks: None,
        }
    }

    /// Happy path: most recent assistant turn has text — use it.
    /// This shouldn't actually be hit by the dispatcher (the dispatcher
    /// only calls the helper when the LATEST response is empty), but
    /// pinning it keeps the helper's contract self-consistent: "return
    /// the most recent assistant text" with no special edge for
    /// position.
    #[test]
    fn returns_most_recent_assistant_text_when_present() {
        let messages = vec![
            system("you are an explorer agent"),
            user("explore the repo"),
            assistant("I'll start by listing files."),
            assistant("I found three Rust crates."),
        ];
        assert_eq!(
            recover_last_assistant_text(&messages),
            Some("I found three Rust crates.".to_string())
        );
    }

    /// The bug we're actually fixing: latest assistant turn is
    /// content-less (a pure tool-call row from the DB load path),
    /// so we walk further back and surface the prior turn's text.
    /// Mirrors claude_code_src's framing: "If the final assistant
    /// message is a pure tool_use block (loop exited mid-turn),
    /// fall back to the most recent assistant message that has
    /// text content."
    #[test]
    fn skips_content_less_assistant_turns_and_finds_earlier_text() {
        let messages = vec![
            system("you are an explorer agent"),
            user("explore the repo"),
            assistant("I see three crates: koda-core, koda-cli, koda-sandbox."),
            assistant_no_content(), // pure tool-call turn
            assistant_no_content(), // another pure tool-call turn
        ];
        assert_eq!(
            recover_last_assistant_text(&messages),
            Some("I see three crates: koda-core, koda-cli, koda-sandbox.".to_string())
        );
    }

    /// Whitespace-only content must NOT shadow earlier real text.
    /// Without the `.trim().is_empty()` guard, a `Some("   \n")` row
    /// from a quirky provider would short-circuit the scan and
    /// surface a useless string — same UX failure as the original
    /// `(no output)` bug, just in disguise.
    #[test]
    fn whitespace_only_content_does_not_shadow_earlier_real_text() {
        let messages = vec![
            user("explore"),
            assistant("Found Cargo.toml and src/lib.rs."),
            assistant("   \n  \t  "), // whitespace-only — must be skipped
        ];
        assert_eq!(
            recover_last_assistant_text(&messages),
            Some("Found Cargo.toml and src/lib.rs.".to_string())
        );
    }

    /// Returned text must come back trimmed. The dispatcher uses the
    /// return value verbatim as the sub-agent's tool-result string;
    /// leading/trailing whitespace would render oddly in the parent's
    /// transcript and burn tokens for nothing.
    #[test]
    fn returned_text_is_trimmed() {
        let messages = vec![
            user(""),
            assistant("   final answer with surrounding whitespace.   "),
        ];
        assert_eq!(
            recover_last_assistant_text(&messages),
            Some("final answer with surrounding whitespace.".to_string())
        );
    }

    /// User and system turns with text are NOT recovery candidates.
    /// Surfacing the user's prompt back to the parent as the sub-agent's
    /// "answer" would be actively misleading — the parent might think
    /// the sub-agent agreed with the prompt verbatim. System turns are
    /// scaffolding, not content. Both must be filtered.
    #[test]
    fn ignores_user_and_system_messages() {
        let messages = vec![
            system("You are explorer. You MUST summarize at the end."),
            user("please explore the repo and tell me what you find"),
            // No assistant text at all.
        ];
        assert_eq!(recover_last_assistant_text(&messages), None);
    }

    /// No assistant text anywhere → caller surfaces a structured
    /// marker. The helper signals this case with `None`, NOT with an
    /// empty `Some("")` (which would re-introduce the bug).
    #[test]
    fn returns_none_when_no_assistant_text_anywhere() {
        let messages = vec![
            system("explorer prompt"),
            user("explore"),
            assistant_no_content(),
            assistant_no_content(),
        ];
        assert_eq!(recover_last_assistant_text(&messages), None);
    }

    /// Empty input slice → `None`. Pin the obvious edge so a future
    /// refactor that swaps `iter().rev()` for some indexed access
    /// doesn't panic in the wild.
    #[test]
    fn returns_none_for_empty_messages() {
        assert_eq!(recover_last_assistant_text(&[]), None);
    }
}

#[cfg(test)]
mod release_workspace_tests {
    use super::{
        WORKTREE_NOTE, edits_files, instructions_for, needs_worktree, release_workspace,
        with_workspace_hint,
    };
    use crate::config::AgentIsolation;
    use crate::engine::EngineEvent;
    use crate::engine::sink::TestSink;
    use crate::trust::TrustMode;
    use koda_sandbox::WorkspaceProvider;
    use std::path::{Path, PathBuf};

    struct Saying(&'static str);

    #[async_trait::async_trait]
    impl WorkspaceProvider for Saying {
        async fn provision(&self, _slot_id: &str) -> anyhow::Result<PathBuf> {
            unimplemented!("only release runs here")
        }

        async fn release(&self, _slot_id: &str, _path: &Path) -> anyhow::Result<Option<String>> {
            match self.0 {
                "nothing" => Ok(None),
                "fails" => anyhow::bail!("no"),
                hint => Ok(Some(hint.to_string())),
            }
        }
    }

    async fn messages(provider: Saying) -> Vec<String> {
        let sink = TestSink::new();
        release_workspace(&provider, "slot", Path::new("/w"), "task", &sink).await;
        sink.events()
            .into_iter()
            .filter_map(|event| match event {
                EngineEvent::Info { message } => Some(message),
                _ => None,
            })
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_hint_reaches_the_user() {
        let said = messages(Saying("still in /w")).await;
        assert_eq!(said.len(), 1);
        assert!(said[0].contains("task: still in /w"), "{said:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_hint_says_nothing() {
        assert!(messages(Saying("nothing")).await.is_empty());
        assert!(messages(Saying("fails")).await.is_empty());
    }

    #[test]
    fn only_a_worktree_adds_to_the_instructions() {
        assert_eq!(instructions_for("Do it.", false), "Do it.");
        let told = instructions_for("Do it.", true);
        assert!(told.starts_with("Do it."), "{told}");
        assert!(told.ends_with(WORKTREE_NOTE), "{told}");
    }

    #[test]
    fn a_hint_follows_the_answer() {
        let told = with_workspace_hint("Done.".into(), Some("On branch x".into()));
        assert_eq!(told, "Done.\n\nOn branch x");
        assert_eq!(with_workspace_hint("Done.".into(), None), "Done.");
    }

    #[test]
    fn writers_stay_in_the_project_unless_safe_or_asked() {
        use TrustMode::{Auto, Plan, Safe};
        let none = AgentIsolation::None;
        let worktree = AgentIsolation::Worktree;
        assert!(!needs_worktree(Safe, true, none, Auto));
        assert!(!needs_worktree(Plan, true, none, Plan));
        assert!(needs_worktree(Safe, true, none, Safe));
        assert!(needs_worktree(Safe, true, worktree, Auto));
    }

    #[test]
    fn safe_leaves_an_agent_without_file_tools_in_the_project() {
        let none = AgentIsolation::None;
        assert!(!needs_worktree(
            TrustMode::Safe,
            false,
            none,
            TrustMode::Safe
        ));
    }

    #[test]
    fn asking_for_a_worktree_holds_without_file_tools_but_not_in_plan() {
        let worktree = AgentIsolation::Worktree;
        assert!(needs_worktree(
            TrustMode::Safe,
            false,
            worktree,
            TrustMode::Auto
        ));
        assert!(!needs_worktree(
            TrustMode::Plan,
            false,
            worktree,
            TrustMode::Auto
        ));
    }

    fn names(tools: &[&str]) -> Vec<String> {
        tools.iter().map(|t| t.to_string()).collect()
    }

    #[test]
    fn denying_one_file_tool_leaves_the_others() {
        assert!(edits_files(&[], &names(&["Edit"])));
        assert!(edits_files(&[], &names(&["Write"])));
        assert!(!edits_files(&[], &names(&["Write", "Edit"])));
    }

    #[test]
    fn an_allow_list_decides_what_is_left() {
        assert!(edits_files(&[], &[]));
        assert!(edits_files(&names(&["Read", "Write"]), &[]));
        assert!(!edits_files(&names(&["Read", "Grep"]), &[]));
        assert!(!edits_files(&names(&["Write"]), &names(&["Write"])));
    }
}

#[cfg(test)]
mod unless_cancelled_tests {
    use super::unless_cancelled;
    use tokio_util::sync::CancellationToken;

    #[tokio::test(flavor = "multi_thread")]
    async fn a_cancel_abandons_the_work() {
        let cancel = CancellationToken::new();
        let waiting = unless_cancelled(&cancel, std::future::pending::<()>());
        cancel.cancel();
        assert_eq!(waiting.await, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_cancelled_token_never_starts_the_work() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(unless_cancelled(&cancel, async { 1 }).await, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn finished_work_keeps_its_result() {
        let cancel = CancellationToken::new();
        assert_eq!(unless_cancelled(&cancel, async { 1 }).await, Some(1));
    }
}

#[cfg(test)]
mod b21_tests {
    //! **#1022 B21** regression tests for the workspace-provision-
    //! failure marker.
    //!
    //! Pre-fix, an `Err` from `workspace.provision()` silently fell
    //! back to `project_root.to_path_buf()` — dropping the sub-agent
    //! into the parent's unisolated working tree and letting parallel
    //! sub-agents race on the same files. The marker is the
    //! short-circuit that replaces that fallback for write-tool sub-
    //! agents.
    //!
    //! These tests pin the *contract* of the marker (not its exact
    //! wording):
    //!
    //! - `[ERROR:` prefix so the model treats it as structural
    //!   failure metadata, not a sub-agent answer.
    //! - The agent name appears so multi-agent flows can identify
    //!   which sub-agent's workspace failed.
    //! - The failure reason is included so the parent / user can
    //!   diagnose (non-APFS volume, no git repo, etc.).
    //! - A re-strategize hint so the model doesn't just retry the
    //!   same prompt.
    //! - Single-line so it formats cleanly as a tool result.
    //!
    //! End-to-end coverage of the dispatch short-circuit itself
    //! requires injecting a `WorkspaceProvider` into the sub-agent
    //! dispatch path, which is not currently a public seam. Until
    //! that refactor lands, the marker contract here plus a manual
    //! check on a non-APFS macOS volume is the regression net.

    use super::workspace_provision_failure_marker;

    #[test]
    fn marker_has_error_prefix() {
        let m = workspace_provision_failure_marker("writer", "git worktree add failed");
        assert!(
            m.starts_with("[ERROR:"),
            "marker must start with `[ERROR:` so the model treats it as \
             structural failure, not a sub-agent answer; got: {m}"
        );
    }

    #[test]
    fn marker_includes_agent_name() {
        let m = workspace_provision_failure_marker("writer", "git worktree add failed");
        assert!(
            m.contains("'writer'"),
            "marker must name the sub-agent that failed so multi-agent \
             flows can disambiguate; got: {m}"
        );
    }

    #[test]
    fn marker_includes_failure_reason() {
        // The reason is the actionable bit — if it's missing, the
        // user has no way to know whether it's a misconfigured
        // volume, missing git repo, or something else. This
        // guarantees the underlying `e.to_string()` reaches the
        // parent agent intact.
        let m = workspace_provision_failure_marker("writer", "git worktree add failed");
        assert!(
            m.contains("git worktree add failed"),
            "marker must include the failure reason verbatim so it's \
             diagnosable; got: {m}"
        );
    }

    #[test]
    fn marker_does_not_silently_dispatch() {
        // The whole point of B21: the marker explicitly states the
        // sub-agent was *not* dispatched. If a future refactor
        // re-introduces the silent fallback and forgets this
        // wording, the model loses the signal that nothing ran.
        let m = workspace_provision_failure_marker("writer", "x");
        let lower = m.to_lowercase();
        assert!(
            lower.contains("not dispatched") || lower.contains("was not dispatched"),
            "marker must state the sub-agent was not dispatched, so the \
             parent doesn't assume the work happened; got: {m}"
        );
    }

    #[test]
    fn marker_includes_restrategize_hint() {
        // Without a hint, the model would tend to retry the same
        // prompt and hit the same provision failure. Same rationale
        // as the iteration-cap marker.
        let m = workspace_provision_failure_marker("writer", "x");
        let lower = m.to_lowercase();
        assert!(
            lower.contains("directly") || lower.contains("resolve") || lower.contains("retry"),
            "marker must hint at re-strategizing (e.g. resolve setup, \
             retry without write tools, do directly); got: {m}"
        );
    }

    #[test]
    fn marker_is_single_line() {
        let m = workspace_provision_failure_marker("writer", "git worktree add failed");
        assert!(
            !m.contains('\n'),
            "marker must be single-line for clean tool-result formatting; got:\n{m}"
        );
    }
}

#[cfg(test)]
mod parse_agent_name_required_tests {
    //! #1232 §5: schema declares `agent_name` required, and this
    //! module's runtime backstop holds the line for models that
    //! ignore the schema. The tests pin every branch so a future
    //! refactor that re-introduces an `unwrap_or("task")` fallback
    //! fails loudly here.

    use super::{available_agents_hint, parse_agent_name_required};
    use serde_json::json;
    use std::path::Path;

    /// Tests use `Path::new(".")` for the project root. Discovery
    /// will return only built-in agents (`explore`, `task`) plus any
    /// in `<cwd>/agents/` if it exists. The
    /// hint-suffix assertions only check for built-in names that
    /// must always be present.
    fn root() -> &'static Path {
        Path::new(".")
    }

    #[test]
    fn accepts_well_formed_string() {
        let name = parse_agent_name_required(&json!({"agent_name": "explore"}), root())
            .expect("explore must parse");
        assert_eq!(name, "explore");
    }

    #[test]
    fn trims_surrounding_whitespace() {
        // Models occasionally emit `"  explore  "` from sloppy
        // template interpolation. Accept it rather than reject.
        let name = parse_agent_name_required(&json!({"agent_name": "  explore  "}), root())
            .expect("trimmed string must parse");
        assert_eq!(name, "explore");
    }

    #[test]
    fn rejects_missing_field_with_actionable_hint() {
        let err = parse_agent_name_required(&json!({"prompt": "x"}), root())
            .expect_err("missing agent_name must fail")
            .to_string();
        assert!(
            err.contains("'agent_name' is required"),
            "missing-field error must name the field: {err}"
        );
        assert!(
            err.contains("Available agent"),
            "error must list available agents: {err}"
        );
        // Built-ins must be discoverable from any project root.
        assert!(err.contains("task"), "hint must list `task`: {err}");
    }

    #[test]
    fn rejects_explicit_null() {
        let err = parse_agent_name_required(&json!({"agent_name": null}), root())
            .expect_err("null agent_name must fail")
            .to_string();
        // null falls into the same arm as missing — same error message.
        assert!(err.contains("'agent_name' is required"), "got: {err}");
    }

    #[test]
    fn rejects_empty_string() {
        // Empty / whitespace-only strings are useless and silently-
        // routing them anywhere would resurrect the original bug.
        for empty in ["", "   ", "\t\n"] {
            let err = parse_agent_name_required(&json!({"agent_name": empty}), root())
                .expect_err("empty agent_name must fail")
                .to_string();
            assert!(
                err.contains("non-empty"),
                "empty {empty:?} must produce 'non-empty' error: {err}"
            );
        }
    }

    #[test]
    fn rejects_wrong_types() {
        for (value, expected_kind) in [
            (json!({"agent_name": true}), "a boolean"),
            (json!({"agent_name": 42}), "a number"),
            (json!({"agent_name": ["explore"]}), "an array"),
            (json!({"agent_name": {"name": "explore"}}), "an object"),
        ] {
            let err = parse_agent_name_required(&value, root())
                .expect_err("wrong-type agent_name must fail")
                .to_string();
            assert!(
                err.contains(expected_kind),
                "got: {err} — expected to mention {expected_kind:?}"
            );
            assert!(
                err.contains("Available"),
                "wrong-type error must also surface the available list: {err}"
            );
        }
    }

    #[test]
    fn hint_lists_builtins_and_fork() {
        let hint = available_agents_hint(root());
        // The built-in agents must always be present.
        for name in ["explore", "task", "fork"] {
            assert!(hint.contains(name), "hint must list `{name}`: {hint}");
        }
        // Names are sorted + deduplicated; `fork` should appear exactly once
        // even though it's pushed in addition to discovery.
        let fork_count = hint.matches("fork").count();
        assert_eq!(fork_count, 1, "`fork` must not be duplicated: {hint}");
    }
}

#[cfg(test)]
mod error_chain_format_tests {
    //! #1232 §4: pin the contract that sub-agent dispatch error
    //! strings include the **entire** anyhow context chain, not just
    //! the topmost label.
    //!
    //! The bug-review session that opened #1232 logged:
    //!   * msg #8–11: `Error invoking sub-agent: LLM API returned 400
    //!     Bad Request: {"error":"Context size has been exceeded."}`
    //!     — useful, the upstream HTTP body is a single `anyhow::bail!`
    //!     string so default `{e}` already shows everything.
    //!   * msg #19–20: `Error invoking sub-agent: Failed to call LLM
    //!     API` — useless, the underlying `reqwest::send()` cause
    //!     (network error, timeout, connection refused, ...) was added
    //!     by `.context("Failed to call LLM API")` in the provider but
    //!     `format!("{e}")` strips every layer except the top.
    //!
    //! Fix: switch every error-stringification site to `{e:#}` (anyhow's
    //! "alternate" Display flag) which walks the chain joined with ": ".
    //! These tests pin the format so a future "let's clean up the
    //! formatting" refactor can't silently regress to `{e}`.
    use anyhow::{Context, Error};
    #[test]
    fn alt_display_walks_chain_in_order_root_first_topmost_last() {
        // Build the exact shape `openai_compat.rs::chat` produces on
        // a network failure: an inner reqwest-style cause wrapped by
        // `.context("Failed to call LLM API")`.
        let e: Error = Err::<(), _>(anyhow::anyhow!("connection refused"))
            .context("error sending request for url")
            .context("Failed to call LLM API")
            .unwrap_err();
        // Default `{}` is the regression case the bug report flagged:
        // only the topmost label survives. Pin this so anyone reading
        // the test sees WHY we use `{:#}` everywhere.
        assert_eq!(format!("{e}"), "Failed to call LLM API");
        // `{:#}` walks the chain top-to-bottom, joined with ": ".
        // This is the contract every sub-agent error site relies on.
        assert_eq!(
            format!("{e:#}"),
            "Failed to call LLM API: error sending request for url: connection refused"
        );
    }
    #[test]
    fn fg_dispatch_error_string_format_contract() {
        // Mirror the literal `format!` in `tool_dispatch.rs`'s
        // foreground branch. If this assertion ever fails it means
        // someone reverted `{e:#}` → `{e}` and re-broke msg #19/20.
        let e: Error = Err::<(), _>(anyhow::anyhow!("timed out"))
            .context("Failed to call LLM API")
            .unwrap_err();
        let formatted = format!("Error invoking sub-agent: {e:#}");
        assert_eq!(
            formatted,
            "Error invoking sub-agent: Failed to call LLM API: timed out"
        );
    }
    #[test]
    fn bg_dispatch_error_string_format_contract() {
        // Mirror the literal `format!` in this module's bg-task branch
        // (the `Err(e) => tx.send(Err((format!("Error: {e:#}"), ...)))`
        // arm above). Same regression protection as the foreground test.
        let e: Error = Err::<(), _>(anyhow::anyhow!("dns lookup failed"))
            .context("error sending request for url")
            .context("Failed to call LLM API (stream)")
            .unwrap_err();
        let formatted = format!("Error: {e:#}");
        assert_eq!(
            formatted,
            "Error: Failed to call LLM API (stream): error sending request for url: dns lookup failed"
        );
    }
    #[test]
    fn single_layer_error_unchanged_by_alt_format() {
        // Negative regression: for the working sibling case (`anyhow::bail!`
        // already produces a rich message with no chain), `{:#}` and
        // `{}` must produce IDENTICAL output. This prevents a future
        // refactor that switches to `{:#}` from accidentally adding
        // a trailing colon or any other formatting noise to errors
        // that were already fine.
        let e: Error = anyhow::anyhow!(
            "LLM API returned 400 Bad Request: {{\"error\":\"Context size has been exceeded.\"}}"
        );
        assert_eq!(format!("{e}"), format!("{e:#}"));
    }
}
