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
/// This is a standalone async fn so the future is `Send + 'static`,
/// which `tokio::spawn` requires.
async fn run_bg_agent(
    project_root: std::path::PathBuf,
    parent_config: KodaConfig,
    db: Database,
    arguments: String,
    sub_agent_cache: SubAgentCache,
    parent_session: String,
    tx: tokio::sync::oneshot::Sender<Result<String, String>>,
) {
    let cancel = CancellationToken::new();
    let (_, mut cmd_rx) = mpsc::channel(1);
    let null_sink = crate::engine::sink::NullSink;
    let nested_bg = crate::bg_agent::new_shared();

    // Override background=false to prevent infinite spawn
    let mut sync_args: serde_json::Value = serde_json::from_str(&arguments).unwrap_or_default();
    sync_args["background"] = serde_json::Value::Bool(false);
    let sync_arguments = serde_json::to_string(&sync_args).unwrap();

    let result = execute_sub_agent(
        &project_root,
        &parent_config,
        &db,
        &sync_arguments,
        TrustMode::Auto,
        &null_sink,
        cancel,
        &mut cmd_rx,
        None,
        &sub_agent_cache,
        &parent_session,
        &nested_bg,
    )
    .await;

    let _ = match result {
        Ok(output) => tx.send(Ok(output)),
        Err(e) => tx.send(Err(format!("Error: {e}"))),
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
pub(crate) async fn execute_sub_agent(
    project_root: &Path,
    parent_config: &KodaConfig,
    db: &Database,
    arguments: &str,
    mode: TrustMode,
    sink: &dyn crate::engine::EngineSink,
    cancel: CancellationToken,
    cmd_rx: &mut mpsc::Receiver<EngineCommand>,
    parent_cache: Option<crate::tools::FileReadCache>,
    sub_agent_cache: &SubAgentCache,
    parent_session_id: &str,
    bg_agents: &std::sync::Arc<crate::bg_agent::BgAgentRegistry>,
) -> Result<String> {
    let args: serde_json::Value = serde_json::from_str(arguments)?;
    let agent_name = args["agent_name"].as_str().unwrap_or("task");
    tracing::Span::current().record("agent_name", agent_name);
    let prompt = args["prompt"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'prompt'"))?;
    let session_id = args["session_id"].as_str().map(|s| s.to_string());
    let is_fork = agent_name == "fork";
    let background = args["background"].as_bool().unwrap_or(false);

    // Background mode: spawn and return immediately
    if background {
        let (task_id, tx) = bg_agents.register(agent_name, prompt);
        let project_root = project_root.to_path_buf();
        let parent_config = parent_config.clone();
        let agent_name_owned = agent_name.to_string();
        let arguments = arguments.to_string();
        let sub_agent_cache = sub_agent_cache.clone();
        let parent_session = parent_session_id.to_string();
        let bg_db = db.clone();

        sink.emit(EngineEvent::Info {
            message: format!("  \u{1f680} {agent_name} launched in background (task {task_id})"),
        });

        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("bg agent runtime");
            rt.block_on(run_bg_agent(
                project_root,
                parent_config,
                bg_db,
                arguments,
                sub_agent_cache,
                parent_session,
                tx,
            ));
        });

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
    let workspace: Box<dyn WorkspaceProvider> = if has_write_tools {
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
        match parent_cache {
            Some(cache) => registry.with_shared_cache(cache),
            None => registry,
        }
    };
    let tool_defs = {
        let mut denied = sub_config.disallowed_tools.clone();
        // Anti-recursion: fork children cannot spawn sub-agents
        if is_fork && !denied.contains(&"InvokeAgent".to_string()) {
            denied.push("InvokeAgent".to_string());
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
            let approval = trust::check_tool(
                &tc.function_name,
                &parsed_args,
                mode,
                Some(effective_root_ref),
            );

            let output = match approval {
                ToolApproval::AutoApprove => {
                    tools
                        .execute(&tc.function_name, &tc.arguments, None)
                        .await
                        .output
                }
                ToolApproval::Blocked => {
                    let detail = tools::describe_action(&tc.function_name, &parsed_args);
                    let diff_preview =
                        preview::compute(&tc.function_name, &parsed_args, effective_root_ref).await;
                    sink.emit(EngineEvent::ActionBlocked {
                        tool_name: tc.function_name.clone(),
                        detail,
                        preview: diff_preview,
                    });
                    "[safe mode] Action blocked.".to_string()
                }
                ToolApproval::NeedsConfirmation => {
                    let detail = tools::describe_action(&tc.function_name, &parsed_args);
                    let diff_preview =
                        preview::compute(&tc.function_name, &parsed_args, effective_root_ref).await;
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
                            tools
                                .execute(&tc.function_name, &tc.arguments, None)
                                .await
                                .output
                        }
                        Some(ApprovalDecision::Reject) => "[rejected by user]".to_string(),
                        Some(ApprovalDecision::RejectWithFeedback { feedback }) => {
                            format!("[rejected: {feedback}]")
                        }
                        None => "[cancelled]".to_string(),
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
) -> Box<dyn WorkspaceProvider> {
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
) -> Box<dyn WorkspaceProvider> {
    // Linux + others: GitWorktreeProvider. The Linux CoW equivalent
    // (4e in #934) is parked until production telemetry shows it's
    // worth building — see #934 deferral comments.
    Box::new(GitWorktreeProvider::new(project_root, agent_name))
}
