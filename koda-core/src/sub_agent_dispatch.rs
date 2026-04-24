//! Sub-agent invocation and lifecycle management.
//!
//! Extracted from `tool_dispatch.rs` — handles `InvokeAgent` execution,
//! background agent spawning, worktree provisioning, and sub-agent caching.
//! Each sub-agent gets its own session, provider, and (optionally) worktree
//! for isolation. Results are cached by `(agent_name, prompt_hash)`.

use crate::approval_flow::request_approval;
use crate::config::KodaConfig;
use crate::db::{Database, Role};
use crate::engine::{ApprovalDecision, EngineCommand, EngineEvent};
use crate::loop_guard;
use crate::memory;
use crate::persistence::Persistence;
use crate::preview;
use crate::prompt::build_system_prompt;
use crate::providers::{ChatMessage, ToolCall};
use crate::sub_agent_cache::SubAgentCache;
use crate::tool_dispatch::execute_one_tool;
use crate::tools::{self, ToolRegistry};
use crate::trust::{self, ToolApproval, TrustMode};

use anyhow::{Context, Result};
use koda_sandbox::{CwdProvider, GitWorktreeProvider, WorkspaceProvider};

#[cfg(target_os = "macos")]
use koda_sandbox::ClonefileProvider;
use std::path::Path;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Run a sub-agent in the background. Owns all data (no borrows).
///
/// **Phase 2 of #1022 (B5 complete):** uses the multi-thread runtime
/// via `tokio::spawn`. This requires `execute_sub_agent`'s future to
/// be `Send`, which we enforce explicitly via the `+ Send` bound on
/// its return type — see the function's signature for the bound and
/// `koda-sandbox/src/ipc.rs` for the matching `Send` bounds on the
/// generic IPC helpers that previously hid a non-Send transitive.
#[allow(clippy::too_many_arguments)]
async fn run_bg_agent(
    project_root: std::path::PathBuf,
    parent_config: KodaConfig,
    db: Database,
    arguments: String,
    sub_agent_cache: SubAgentCache,
    parent_session: String,
    tx: tokio::sync::oneshot::Sender<
        Result<crate::bg_agent::BgPayload, crate::bg_agent::BgPayload>,
    >,
    // B2 of #1022: parent's cancel token, threaded as a `child_token()`
    // so a Ctrl-C in the parent loop cancels the bg agent.
    cancel: CancellationToken,
    // B1 of #1022: parent's trust mode — used both as the approval
    // mode for tool calls inside the bg agent and (via the recursive
    // `execute_sub_agent` call) as the clamp ceiling for the
    // sub-agent's own declared trust.
    parent_trust: TrustMode,
    // B4 of #1022: parent's effective sandbox policy at spawn time.
    // The recursive `execute_sub_agent` composes the child policy
    // onto this so the bg agent inherits any parent narrowing.
    parent_sandbox_policy: koda_sandbox::SandboxPolicy,
) {
    let (_, mut cmd_rx) = mpsc::channel(1);
    // #1022 B9: bg agents used to run with `NullSink`, so every
    // event inside them was silently dropped — the user only saw
    // the spawn line and the eventual completion line. Now we use
    // `BufferingSink` to capture a narrative trace (tool calls,
    // info, auto-rejected approvals) that ships back over the
    // result oneshot and gets surfaced to the user at
    // result-injection time. See `engine::sink::BufferingSink` for
    // the capture rules.
    let buffering_sink = crate::engine::sink::BufferingSink::new();
    let nested_bg = crate::bg_agent::new_shared();

    // Override background=false to prevent infinite spawn — a bg agent
    // that itself emitted `InvokeAgent { background: true }` would
    // never see its child's result (no inference loop is running
    // *inside* a bg agent to drain results).
    let mut sync_args: serde_json::Value = serde_json::from_str(&arguments).unwrap_or_default();
    sync_args["background"] = serde_json::Value::Bool(false);
    let sync_arguments = serde_json::to_string(&sync_args).unwrap();

    let result = execute_sub_agent(
        &project_root,
        &parent_config,
        &db,
        &sync_arguments,
        parent_trust,
        &buffering_sink,
        cancel,
        &mut cmd_rx,
        None,
        &sub_agent_cache,
        &parent_session,
        &nested_bg,
        &parent_sandbox_policy,
    )
    .await;

    // Drain the buffered trace exactly once. The events ship back
    // alongside the output (for the success case) or alongside the
    // error message (for the failure case) so the user can see
    // *what the bg agent attempted* even when it failed.
    let events = buffering_sink.take_lines();
    let _ = match result {
        Ok(output) => tx.send(Ok((output, events))),
        Err(e) => tx.send(Err((format!("Error: {e}"), events))),
    };
}

/// Execute a sub-agent in its own isolated event loop.
///
/// When `parent_cache` is provided, the sub-agent shares the parent's
/// file-read cache so reads by one agent benefit all others.
///
/// Results are cached in `sub_agent_cache` keyed by `(agent_name, prompt_hash)`.
/// On cache hit, returns immediately without any LLM calls.
#[tracing::instrument(skip_all, fields(agent_name, cached = false))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_sub_agent<'a>(
    project_root: &'a Path,
    parent_config: &'a KodaConfig,
    db: &'a Database,
    arguments: &'a str,
    mode: TrustMode,
    sink: &'a dyn crate::engine::EngineSink,
    cancel: CancellationToken,
    cmd_rx: &'a mut mpsc::Receiver<EngineCommand>,
    parent_cache: Option<crate::tools::FileReadCache>,
    sub_agent_cache: &'a SubAgentCache,
    parent_session_id: &'a str,
    bg_agents: &'a std::sync::Arc<crate::bg_agent::BgAgentRegistry>,
    // Phase 5 PR-4 of #934: parent's effective sandbox policy. The
    // child policy is composed onto this so the child can only narrow,
    // never widen — see [`koda_sandbox::SandboxPolicy::compose`] for
    // the per-field rules. Pass `&SandboxPolicy::strict_default()`
    // when there is no meaningful parent (top-level invocation).
    parent_sandbox_policy: &'a koda_sandbox::SandboxPolicy,
) -> impl std::future::Future<Output = Result<String>> + Send + 'a {
    async move {
        let args: serde_json::Value = serde_json::from_str(arguments)?;
        let agent_name = args["agent_name"].as_str().unwrap_or("task");
        tracing::Span::current().record("agent_name", agent_name);
        let prompt = args["prompt"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing 'prompt'"))?;
        let session_id = args["session_id"].as_str().map(|s| s.to_string());
        let is_fork = agent_name == "fork";
        let background = args["background"].as_bool().unwrap_or(false);

        // Background mode: spawn and return immediately.
        //
        // Phase 1 of #1022 fixes B1–B4 here:
        //  * **B1 trust:** the recursive `execute_sub_agent` call below
        //    receives `mode` (the parent's trust mode) instead of
        //    hard-coded `TrustMode::Auto`. The clamp inside that call
        //    then guarantees the bg agent can only narrow, never widen.
        //  * **B2 cancellation:** the bg task receives a `child_token()`
        //    of the parent's `cancel`. Ctrl-C in the parent loop now
        //    cascades into every in-flight bg agent.
        //  * **B3 lifecycle:** the spawned `JoinHandle` is held by the
        //    registry as an `AbortOnDropHandle`, so a registry drop
        //    aborts the task and releases its worktree.
        //  * **B4 sandbox:** `parent_sandbox_policy.clone()` is captured
        //    at spawn time, so the recursive call composes the bg
        //    agent's policy onto the parent's effective policy instead
        //    of regressing to `strict_default()`.
        //  * **B5 (Phase 2):** the bg agent now runs on the multi-thread
        //    runtime via `tokio::spawn`. We enforced `Send` on
        //    `execute_sub_agent`'s future by switching its signature to
        //    `fn(...) -> impl Future<Output = ...> + Send + 'a`, which
        //    forces the compiler to *prove* Send (vs. silently degrading
        //    when an `async fn` happens to capture a non-Send temporary).
        //    The transitive offender was `koda-sandbox::ipc::{read,write}_message`
        //    — those generic helpers had no `Send` bound on `R`/`W`/`T`,
        //    so MutexGuards held across their awaits weren't Send. Bounds
        //    have been added there as well.
        if background {
            let reservation = bg_agents.reserve(&cancel);
            let task_id = reservation.task_id;
            let bg_cancel = reservation.cancel.clone();
            let bg_tx = reservation.tx;
            let bg_rx = reservation.rx;
            let entry_cancel = reservation.cancel;

            let project_root_owned = project_root.to_path_buf();
            let parent_config_owned = parent_config.clone();
            let agent_name_owned = agent_name.to_string();
            let prompt_owned = prompt.to_string();
            let arguments_owned = arguments.to_string();
            let sub_agent_cache_owned = sub_agent_cache.clone();
            let parent_session_owned = parent_session_id.to_string();
            let bg_db = db.clone();
            let bg_policy = parent_sandbox_policy.clone();
            let bg_trust = mode;

            sink.emit(EngineEvent::Info {
                message: format!(
                    "  \u{1f680} {agent_name} launched in background (task {task_id})"
                ),
            });

            let handle = tokio::spawn(run_bg_agent(
                project_root_owned,
                parent_config_owned,
                bg_db,
                arguments_owned,
                sub_agent_cache_owned,
                parent_session_owned,
                bg_tx,
                bg_cancel,
                bg_trust,
                bg_policy,
            ));

            bg_agents.attach(
                task_id,
                &agent_name_owned,
                &prompt_owned,
                bg_rx,
                entry_cancel,
                handle,
            );

            return Ok(format!(
                "Background agent '{agent_name_owned}' started (task {task_id}). \
             Results will be injected when complete."
            ));
        }
        // Check result cache (only for stateless calls without a session_id,
        // since session continuations need fresh execution).
        if session_id.is_none()
            && let Some(cached) = sub_agent_cache.get(agent_name, prompt)
        {
            sink.emit(EngineEvent::Info {
                message: format!("  \u{26a1} {agent_name}: cache hit, skipping LLM call"),
            });
            tracing::Span::current().record("cached", true);
            return Ok(cached);
        }

        sink.emit(EngineEvent::SubAgentStart {
            agent_name: agent_name.to_string(),
        });

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
        let sub_config = if is_fork {
            // Fork inherits the parent config verbatim — including trust mode.
            // The clone preserves trust; no clamping needed since
            // fork == exact copy.  Assertion guards against future changes
            // that might add config overrides to the fork path.
            let cfg = parent_config.clone();
            debug_assert!(
                cfg.trust == parent_config.trust,
                "fork must inherit parent trust exactly"
            );
            cfg
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

            // Inherit trust: child can never exceed parent's trust (#845).
            // Same pattern as Codex's `apply_spawn_agent_runtime_overrides()`
            // which copies the parent's runtime sandbox_policy onto the child.
            let child_trust = cfg.trust;
            cfg.trust = TrustMode::clamp(parent_config.trust, cfg.trust);
            if cfg.trust != child_trust {
                tracing::info!(
                    agent = agent_name,
                    parent = %parent_config.trust,
                    child = %child_trust,
                    effective = %cfg.trust,
                    "sub-agent trust clamped to match parent",
                );
            }

            cfg
        };

        let sub_session = match session_id {
            Some(id) => id,
            None => {
                let sid = db
                    .create_session(&sub_config.agent_name, project_root)
                    .await?;
                // Fork: copy parent conversation history into the new session
                if is_fork {
                    let parent_history = db.load_context(parent_session_id).await?;
                    for msg in &parent_history {
                        let mid = db
                            .insert_message(
                                &sid,
                                &msg.role,
                                msg.content.as_deref(),
                                msg.tool_calls.as_deref(),
                                msg.tool_call_id.as_deref(),
                                None, // don't duplicate usage stats
                            )
                            .await?;
                        // Copied assistant messages are already complete in the
                        // parent — mark them complete in the child session so
                        // load_context includes them (#875).
                        if msg.role == Role::Assistant {
                            db.mark_message_complete(mid).await?;
                        }
                    }
                }
                sid
            }
        };

        db.insert_message(&sub_session, &Role::User, Some(prompt), None, None, None)
            .await?;

        let provider = crate::providers::create_provider(&sub_config);
        // Select workspace provider. Write-capable agents get an isolated
        // workspace; read-only agents share the parent root for free.
        //
        // Per-platform write-isolation choice:
        //
        // - **macOS:** `ClonefileProvider` (APFS clonefile(2)) is
        //   preferred for its ~3-4× provision speedup over git worktree
        //   (Phase 4d / #934). Falls back to git worktree if construction
        //   fails (e.g. `$HOME` unset, project path can't canonicalize).
        // - **Linux + others:** `GitWorktreeProvider`. The Linux CoW
        //   equivalent (4e in #934) is deferred until production
        //   telemetry shows it's worth building.
        //
        // **Documented platform divergence** — see `docs/src/sandbox.md`
        // → "Workspace providers". Both backends provide the same
        // isolation guarantees; only provision speed differs.
        let has_write_tools = !sub_config
            .disallowed_tools
            .iter()
            .any(|t| t == "Write" || t == "Edit");
        // Phase 1 of #1022: explicit `+ Send + Sync` on the trait object.
        // The supertrait bound `WorkspaceProvider: Send + Sync` constrains
        // *implementors*, but Rust trait objects don't auto-inherit those
        // bounds — `Box<dyn WorkspaceProvider>` is `!Send` without the
        // explicit annotation, which makes the whole `execute_sub_agent`
        // future `!Send` and unspawnable.
        let workspace: Box<dyn WorkspaceProvider + Send + Sync> = if has_write_tools {
            pick_write_provider(project_root, agent_name)
        } else {
            Box::new(CwdProvider::new(project_root))
        };
        let effective_root = match workspace.provision(&sub_session).await {
            Ok(path) => {
                if path != project_root {
                    sink.emit(EngineEvent::Info {
                        message: format!("  \u{1f333} {agent_name}: isolated in worktree"),
                    });
                }
                path
            }
            Err(e) => {
                tracing::warn!("Workspace provision failed: {e}");
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
            // scripted model emits `InvokeAgent` regardless.
            if !denied.contains(&"InvokeAgent".to_string()) {
                denied.push("InvokeAgent".to_string());
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
            tools.get_definitions(&sub_config.allowed_tools, &denied)
        };
        let semantic_memory = if sub_config.skip_memory {
            String::new()
        } else {
            memory::load(project_root)?
        };
        let env = crate::prompt::EnvironmentInfo {
            project_root: effective_root_ref,
            model: &sub_config.model,
            platform: std::env::consts::OS,
        };
        let system_prompt = build_system_prompt(
            &sub_config.system_prompt,
            &semantic_memory,
            &sub_config.agents_dir,
            &env,
            &[], // sub-agents have no REPL commands
            &tools.skill_registry,
        );

        for _ in 0..loop_guard::MAX_SUB_AGENT_ITERATIONS {
            // Respect parent cancellation (#286)
            if cancel.is_cancelled() {
                // Release workspace on cancellation (best-effort, no user hint).
                let _ = workspace.release(&sub_session, &effective_root).await;
                return Ok("[cancelled by parent]".to_string());
            }
            let history = db.load_context(&sub_session).await?;
            let mut messages = vec![ChatMessage::text("system", &system_prompt)];
            for msg in &history {
                let tool_calls: Option<Vec<ToolCall>> = msg
                    .tool_calls
                    .as_deref()
                    .and_then(|tc| serde_json::from_str(tc).ok());
                messages.push(ChatMessage {
                    role: msg.role.as_str().to_string(),
                    content: msg.content.clone(),
                    tool_calls,
                    tool_call_id: msg.tool_call_id.clone(),
                    images: None,
                });
            }

            sink.emit(EngineEvent::SpinnerStart {
                message: format!("  🦥 {agent_name} thinking..."),
            });
            let response = provider
                .chat(&messages, &tool_defs, &sub_config.model_settings)
                .await?;
            sink.emit(EngineEvent::SpinnerStop);

            let tool_calls_json = if response.tool_calls.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&response.tool_calls)?)
            };

            db.insert_message(
                &sub_session,
                &Role::Assistant,
                response.content.as_deref(),
                tool_calls_json.as_deref(),
                None,
                Some(&response.usage),
            )
            .await?;

            if response.tool_calls.is_empty() {
                let result = response
                    .content
                    .unwrap_or_else(|| "(no output)".to_string());
                // Cache the result for future identical calls
                sub_agent_cache.put(agent_name, prompt, &result);
                // Release workspace; surface branch hint if agent left changes.
                if let Ok(Some(hint)) = workspace.release(&sub_session, &effective_root).await {
                    sink.emit(EngineEvent::Info {
                        message: format!("  \u{1f335} {agent_name}: {hint}"),
                    });
                }
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
                let validation_error = {
                    let cache = tools.file_read_cache();
                    let last_writer = tools.last_writer_cache();
                    let last_bash = tools.last_bash_cache();
                    tools::validate::validate_tool_call(
                        &tc.function_name,
                        &parsed_args,
                        effective_root_ref,
                        Some(&cache),
                        Some(&last_writer),
                        Some(&last_bash),
                    )
                    .await
                };

                let output = if let Some(error) = validation_error {
                    format!("Validation error: {error}")
                } else {
                    let approval = trust::check_tool(
                        &tc.function_name,
                        &parsed_args,
                        mode,
                        Some(effective_root_ref),
                    );

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
                            let (_id, result, _success, _full) = execute_one_tool(
                                tc,
                                project_root,
                                &sub_config,
                                db,
                                &sub_session,
                                &tools,
                                mode,
                                sink,
                                cancel.clone(),
                                sub_agent_cache,
                                bg_agents,
                            )
                            .await;
                            result
                        }
                        ToolApproval::Blocked => {
                            let detail = tools::describe_action(&tc.function_name, &parsed_args);
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
                            let detail = tools::describe_action(&tc.function_name, &parsed_args);
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
                            match request_approval(
                                sink,
                                cmd_rx,
                                &cancel,
                                &tc.function_name,
                                &detail,
                                diff_preview,
                                effect,
                            )
                            .await
                            {
                                Some(ApprovalDecision::Approve) => {
                                    let (_id, result, _success, _full) = execute_one_tool(
                                        tc,
                                        project_root,
                                        &sub_config,
                                        db,
                                        &sub_session,
                                        &tools,
                                        mode,
                                        sink,
                                        cancel.clone(),
                                        sub_agent_cache,
                                        bg_agents,
                                    )
                                    .await;
                                    result
                                }
                                Some(ApprovalDecision::Reject) => "[rejected by user]".to_string(),
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

        sink.emit(EngineEvent::Warn {
            message: format!(
                "Sub-agent '{agent_name}' hit its iteration limit ({}). Returning partial result.",
                loop_guard::MAX_SUB_AGENT_ITERATIONS
            ),
        });
        // Release workspace on iteration limit exit.
        if let Ok(Some(hint)) = workspace.release(&sub_session, &effective_root).await {
            sink.emit(EngineEvent::Info {
                message: format!("  \u{1f335} {agent_name}: {hint}"),
            });
        }
        Ok("(sub-agent reached maximum iterations)".to_string())
    }
}

// ── Workspace provider selection ────────────────────────────────────────────
//
// Two cfg-gated definitions of `pick_write_provider` rather than
// inline `cfg!()` branches because:
//
//  * `ClonefileProvider` is itself `cfg(target_os = "macos")` in
//    koda-sandbox; inline branches would still need cfg gating to
//    avoid "unresolved import" on Linux.
//  * Each platform's selection logic is small but distinct (macOS
//    has a fallback path, Linux doesn't), and side-by-side cfg
//    bodies read more honestly than a tangled inline form.
//
// Behavior is documented for users in `docs/src/sandbox.md`
// → "Workspace providers".

#[cfg(target_os = "macos")]
fn pick_write_provider(
    project_root: &std::path::Path,
    agent_name: &str,
) -> Box<dyn WorkspaceProvider + Send + Sync> {
    // Try `ClonefileProvider` first — its 3-4× provision speedup
    // (Phase 4d / #934 bench) is durable and the implementation is
    // a thin wrapper over the OS primitive designed for this.
    //
    // Fall back to `GitWorktreeProvider` only if construction itself
    // fails (e.g. `$HOME` unset, project path can't canonicalize).
    // Runtime `clonefile(2)` failures (non-APFS volume etc.) surface
    // through the existing `provision()` error path which already
    // falls back to the unisolated project root with a warning.
    match ClonefileProvider::new(project_root) {
        Ok(p) => Box::new(p),
        Err(e) => {
            tracing::warn!("ClonefileProvider unavailable, falling back to git worktree: {e}");
            Box::new(GitWorktreeProvider::new(project_root, agent_name))
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn pick_write_provider(
    project_root: &std::path::Path,
    agent_name: &str,
) -> Box<dyn WorkspaceProvider + Send + Sync> {
    // Linux + others: GitWorktreeProvider. The Linux CoW equivalent
    // (4e in #934) is parked until production telemetry shows it's
    // worth building — see #934 deferral comments.
    Box::new(GitWorktreeProvider::new(project_root, agent_name))
}
