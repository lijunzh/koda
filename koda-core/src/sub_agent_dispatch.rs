//! Sub-agent invocation and lifecycle management.
//!
//! Extracted from `tool_dispatch.rs` — handles `InvokeAgent` execution,
//! background agent spawning, worktree provisioning, and sub-agent caching.
//! Each sub-agent gets its own session, provider, and (optionally) worktree
//! for isolation. Results are cached by `(agent_name, prompt_hash)`.

use crate::approval::{self, ApprovalMode, ToolApproval};
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

use anyhow::{Context, Result};
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
        ApprovalMode::Auto,
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
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_sub_agent(
    project_root: &Path,
    parent_config: &KodaConfig,
    db: &Database,
    arguments: &str,
    mode: ApprovalMode,
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
        return Ok(cached);
    }

    sink.emit(EngineEvent::SubAgentStart {
        agent_name: agent_name.to_string(),
    });

    // Fork inherits parent config; named agents load their own persona
    // but fall back to the parent's provider and model for anything not
    // explicitly set in the agent JSON.
    //
    // Why always inherit the provider:
    //   Sub-agents have no independent API credentials or routing config.
    //   If the user is on Gemini, every sub-agent should use Gemini.
    //
    // Why fall back to parent's model:
    //   Agent JSON can set a preferred model (e.g. a cheaper/faster one).
    //   If it doesn't, the parent's active model is the only guaranteed
    //   working choice — defaulting to a provider's "default model" can
    //   silently route to a local endpoint with nothing loaded (this bug).
    let sub_config = if is_fork {
        parent_config.clone()
    } else {
        // Load the raw JSON first to see what the agent explicitly set.
        let raw = crate::config::KodaConfig::load_agent_json(project_root, agent_name)
            .with_context(|| format!("Failed to load sub-agent: {agent_name}"))?;

        let mut cfg = crate::config::KodaConfig::load(project_root, agent_name)
            .with_context(|| format!("Failed to load sub-agent: {agent_name}"))?;

        // Always inherit parent's provider + base_url.
        cfg = cfg.with_overrides(
            Some(parent_config.base_url.clone()),
            None,
            Some(parent_config.provider_type.to_string()),
        );

        // Inherit parent's model only when the agent JSON left it unset.
        if raw.model.is_none() {
            cfg = cfg.with_overrides(None, Some(parent_config.model.clone()), None);
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
                    db.insert_message(
                        &sid,
                        &msg.role,
                        msg.content.as_deref(),
                        msg.tool_calls.as_deref(),
                        msg.tool_call_id.as_deref(),
                        None, // don't duplicate usage stats
                    )
                    .await?;
                }
            }
            sid
        }
    };

    db.insert_message(&sub_session, &Role::User, Some(prompt), None, None, None)
        .await?;

    let provider = crate::providers::create_provider(&sub_config);
    // Provision a git worktree for agents that can write files.
    // Read-only agents (explore, plan, verify) skip this.
    let has_write_tools = !sub_config
        .disallowed_tools
        .iter()
        .any(|t| t == "Write" || t == "Edit");
    let (effective_root, worktree_path) = if has_write_tools {
        match crate::worktree::provision(project_root, &sub_session).await {
            Ok(crate::worktree::WorktreeResult::Created(wt)) => {
                sink.emit(EngineEvent::Info {
                    message: format!("  \u{1f333} {agent_name}: isolated in worktree"),
                });
                (wt.clone(), Some(wt))
            }
            Ok(_) => (project_root.to_path_buf(), None), // not a git repo / no git
            Err(e) => {
                tracing::warn!("Worktree provision failed: {e}");
                (project_root.to_path_buf(), None)
            }
        }
    } else {
        (project_root.to_path_buf(), None)
    };
    let effective_root_ref = effective_root.as_path();

    let tools = {
        let registry = ToolRegistry::new(effective_root.clone(), sub_config.max_context_tokens);
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
    let semantic_memory = memory::load(project_root)?;
    let env = crate::prompt::EnvironmentInfo {
        project_root: effective_root_ref,
        model: &sub_config.model,
        platform: std::env::consts::OS,
    };
    let system_prompt = build_system_prompt(
        &sub_config.system_prompt,
        &semantic_memory,
        &sub_config.agents_dir,
        &tool_defs,
        &env,
        &[], // sub-agents have no REPL commands
    );

    for _ in 0..loop_guard::MAX_SUB_AGENT_ITERATIONS {
        // Respect parent cancellation (#286)
        if cancel.is_cancelled() {
            // Clean up worktree on cancellation
            if let Some(ref wt) = worktree_path {
                let _ = crate::worktree::cleanup(project_root, wt).await;
            }
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
            // Clean up worktree
            if let Some(ref wt) = worktree_path
                && let Ok(Some(changes)) = crate::worktree::cleanup(project_root, wt).await
            {
                sink.emit(EngineEvent::Info {
                    message: format!("  \u{1f333} {agent_name}: {changes}"),
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
            let approval = approval::check_tool(
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
                    let effect =
                        crate::approval::resolve_tool_effect(&tc.function_name, &parsed_args);
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
    // Clean up worktree on exit
    if let Some(ref wt) = worktree_path
        && let Ok(Some(changes)) = crate::worktree::cleanup(project_root, wt).await
    {
        sink.emit(EngineEvent::Info {
            message: format!("  \u{1f333} {agent_name}: {changes}"),
        });
    }
    Ok("(sub-agent reached maximum iterations)".to_string())
}
