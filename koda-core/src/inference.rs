//! LLM inference loop with streaming, tool execution, and sub-agent delegation.
//!
//! Runs the streaming inference → tool execution → re-inference loop
//! until the LLM produces a final text response.

use crate::approval::ApprovalMode;
use crate::config::KodaConfig;
use crate::db::{Database, Role};
use crate::engine::{EngineCommand, EngineEvent};
use crate::inference_helpers::{
    PREFLIGHT_COMPACT_THRESHOLD, RATE_LIMIT_MAX_RETRIES, assemble_messages, estimate_tokens,
    is_context_overflow_error, is_rate_limit_error, rate_limit_backoff,
};
use crate::loop_guard::LoopDetector;
use crate::persistence::Persistence;
use crate::providers::{ChatMessage, ImageData, LlmProvider, StreamChunk, ToolCall};
use crate::settings::Settings;
use crate::task_phase::{PhaseInfo, PhaseTracker, ToolType, TurnSignal};
use crate::tool_dispatch::{
    can_parallelize, execute_tools_parallel, execute_tools_sequential, execute_tools_split_batch,
};
use crate::tools::{self, ToolRegistry};

use anyhow::{Context, Result};
use std::path::Path;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// All parameters for the inference loop, bundled into a single struct.
pub struct InferenceContext<'a> {
    pub project_root: &'a Path,
    pub config: &'a KodaConfig,
    pub db: &'a Database,
    pub session_id: &'a str,
    pub system_prompt: &'a str,
    pub provider: &'a dyn LlmProvider,
    pub tools: &'a ToolRegistry,
    pub tool_defs: &'a [crate::providers::ToolDefinition],
    pub pending_images: Option<Vec<ImageData>>,
    pub mode: ApprovalMode,
    pub settings: &'a mut Settings,
    pub sink: &'a dyn crate::engine::EngineSink,
    pub cancel: CancellationToken,
    pub cmd_rx: &'a mut mpsc::Receiver<EngineCommand>,
}

/// Run inference, executing tool calls until the LLM produces a text response.
pub async fn inference_loop(ctx: InferenceContext<'_>) -> Result<()> {
    let InferenceContext {
        project_root,
        config,
        db,
        session_id,
        system_prompt,
        provider,
        tools,
        tool_defs,
        pending_images,
        mode,
        settings,
        sink,
        cancel,
        cmd_rx,
    } = ctx;
    // Use the same formula as estimate_tokens (chars/CHARS_PER_TOKEN + overhead)
    // to keep the budget calculation consistent with re-estimation later.
    let system_tokens = (system_prompt.len() as f64 / crate::inference_helpers::CHARS_PER_TOKEN)
        as usize
        + crate::inference_helpers::SYSTEM_PROMPT_OVERHEAD;
    let available = config.max_context_tokens.saturating_sub(system_tokens);
    // Hard cap is configurable per-agent; user can extend it interactively.
    let mut hard_cap = config.max_iterations;
    let mut iteration = 0u32;
    let mut made_tool_calls = false;
    let mut loop_detector = LoopDetector::new();
    let sub_agent_cache = crate::sub_agent_cache::SubAgentCache::new();
    let mut tier_observer = crate::tier_observer::TierObserver::new(
        config.model_tier,
        // Tier is explicitly set if it came from agent config (not auto-detected)
        false,
    );
    // Intervention observer: learns human override patterns at phase gates.
    // Auto-saves on drop (ObserverGuard), so all exit paths are covered.
    let mut intervention_observer =
        crate::intervention_observer::InterventionObserver::load_auto_save();
    let mut total_prompt_tokens: i64 = 0;
    let mut total_completion_tokens: i64 = 0;
    let mut total_cache_read_tokens: i64 = 0;
    let mut total_thinking_tokens: i64 = 0;
    let mut total_char_count: usize = 0;
    let loop_start = Instant::now();

    // Pre-build the base system message (avoids re-cloning 4-8KB per iteration)
    let base_system_prompt = system_prompt.to_string();
    let mut recent_tool_names: Vec<String> = Vec::new();

    // Phase tracker: structural detection of task progression.
    // Classify intent from the last user message for phase expectations.
    let intent = {
        let history = db.load_context(session_id, available).await?;
        history
            .iter()
            .rev()
            .find(|m| m.role == crate::db::Role::User)
            .and_then(|m| m.content.as_deref())
            .map(crate::intent::classify_intent)
            .map(|s| s.intent)
            .unwrap_or(crate::intent::TaskIntent::Modify)
    };

    // Task signature: fingerprint for per-task-type learning.
    let _task_signature = {
        let history = db.load_context(session_id, available).await?;
        let prompt = history
            .iter()
            .rev()
            .find(|m| m.role == crate::db::Role::User)
            .and_then(|m| m.content.as_deref())
            .unwrap_or("");
        crate::task_signature::TaskSignature::from_prompt(prompt)
    };
    let mut phase_tracker = PhaseTracker::new(&intent);

    loop {
        if iteration >= hard_cap {
            let recent = loop_detector.recent_names();
            sink.emit(EngineEvent::LoopCapReached {
                cap: hard_cap,
                recent_tools: recent,
            });

            // Wait for client decision via EngineCommand::LoopDecision
            let extra = loop {
                tokio::select! {
                    cmd = cmd_rx.recv() => match cmd {
                        Some(EngineCommand::LoopDecision { action }) => {
                            break action.extra_iterations();
                        }
                        Some(EngineCommand::Interrupt) => {
                            cancel.cancel();
                            break 0;
                        }
                        None => break 0,
                        _ => continue,
                    },
                    _ = cancel.cancelled() => break 0,
                }
            };

            if extra == 0 {
                break Ok(());
            }
            hard_cap += extra;
        }

        // Inject task phase hint + progress + git context into system prompt
        let phase = phase_tracker.current();
        let progress = crate::progress::get_progress_summary(db, session_id)
            .await
            .unwrap_or_default();
        let flow_summary = db.phase_flow_summary(session_id).await.unwrap_or_default();
        let flow_line = if flow_summary.is_empty() {
            String::new()
        } else {
            format!("\n[Flow: {flow_summary}]")
        };
        let git_line = crate::git::git_context(project_root)
            .map(|ctx| format!("\n{ctx}"))
            .unwrap_or_default();
        let phase_hint = if phase == crate::task_phase::TaskPhase::Reviewing {
            let depth = phase_tracker.select_review_depth(&intervention_observer);
            phase.review_hint(config.model_tier, depth)
        } else {
            phase.prompt_hint(config.model_tier)
        };
        let phase_prompt =
            format!("{base_system_prompt}\n\n{phase_hint}{progress}{flow_line}{git_line}",);
        let system_message = ChatMessage::text("system", &phase_prompt);

        // Assemble context with sliding window
        let history = db.load_context(session_id, available).await?;
        let mut messages = assemble_messages(&system_message, &history);

        // Attach pending images to the last user message (first iteration only)
        if iteration == 0
            && let Some(ref imgs) = pending_images
            && !imgs.is_empty()
            && let Some(last_user) = messages.iter_mut().rev().find(|m| m.role == "user")
        {
            last_user.images = Some(imgs.clone());
        }

        // Track context window usage
        let context_used = estimate_tokens(&messages);
        crate::context::update(context_used, config.max_context_tokens);

        // Pre-flight budget check: if context is critically high, compact first
        let ctx_pct = crate::context::percentage();
        if ctx_pct >= PREFLIGHT_COMPACT_THRESHOLD {
            tracing::warn!("Pre-flight: context at {ctx_pct}%, attempting auto-compact");
            sink.emit(EngineEvent::Info {
                message: format!(
                    "\u{1f4e6} Context at {ctx_pct}% \u{2014} compacting before sending..."
                ),
            });
            match crate::compact::compact_session_with_provider(
                db,
                session_id,
                config.max_context_tokens,
                &config.model_settings,
                provider,
            )
            .await
            {
                Ok(Ok(result)) => {
                    sink.emit(EngineEvent::Info {
                        message: format!(
                            "\u{2705} Compacted {} messages (~{} token summary)",
                            result.deleted, result.summary_tokens
                        ),
                    });
                    // Re-assemble with compacted history
                    let history = db.load_context(session_id, available).await?;
                    messages = assemble_messages(&system_message, &history);
                    // Re-attach images if first iteration
                    if iteration == 0
                        && let Some(ref imgs) = pending_images
                        && !imgs.is_empty()
                        && let Some(last_user) =
                            messages.iter_mut().rev().find(|m| m.role == "user")
                    {
                        last_user.images = Some(imgs.clone());
                    }
                    let new_used = estimate_tokens(&messages);
                    crate::context::update(new_used, config.max_context_tokens);
                }
                Ok(Err(skip)) => {
                    tracing::info!("Pre-flight compact skipped: {skip:?}");
                }
                Err(e) => {
                    tracing::warn!("Pre-flight compact failed: {e:#}");
                    sink.emit(EngineEvent::Warn {
                        message: format!("Compact failed: {e:#}. Continuing anyway..."),
                    });
                }
            }
        }

        // Stream the response (with rate limit retry)
        sink.emit(EngineEvent::SpinnerStart {
            message: "Thinking...".into(),
        });

        let stream_result = 'retry: {
            let mut last_err = None;
            for attempt in 0..RATE_LIMIT_MAX_RETRIES {
                let result = tokio::select! {
                    result = provider.chat_stream(&messages, tool_defs, &config.model_settings) => result,
                    _ = cancel.cancelled() => {
                        sink.emit(EngineEvent::SpinnerStop);
                        sink.emit(EngineEvent::Warn {
                            message: "Interrupted".into(),
                        });
                        return Ok(());
                    }
                };
                match result {
                    Ok(rx) => break 'retry Ok(rx),
                    Err(e) if is_rate_limit_error(&e) && attempt + 1 < RATE_LIMIT_MAX_RETRIES => {
                        let delay = rate_limit_backoff(attempt);
                        sink.emit(EngineEvent::SpinnerStop);
                        sink.emit(EngineEvent::Warn {
                            message: format!(
                                "\u{23f3} Rate limited. Retrying in {}s...",
                                delay.as_secs()
                            ),
                        });
                        tracing::warn!(
                            "Rate limit (attempt {}/{}): {e:#}",
                            attempt + 1,
                            RATE_LIMIT_MAX_RETRIES
                        );
                        tokio::time::sleep(delay).await;
                        sink.emit(EngineEvent::SpinnerStart {
                            message: format!("Retrying (attempt {})...", attempt + 2),
                        });
                        last_err = Some(e);
                    }
                    Err(e) => break 'retry Err(e),
                }
            }
            Err(last_err.unwrap_or_else(|| anyhow::anyhow!("Rate limit retries exhausted")))
        };

        // Graceful recovery: if the provider returns a context-overflow error,
        // compact and retry once before giving up.
        let mut rx = match stream_result {
            Ok(rx) => rx,
            Err(e) if is_context_overflow_error(&e) => {
                sink.emit(EngineEvent::SpinnerStop);
                sink.emit(EngineEvent::Warn {
                    message:
                        "\u{26a0}\u{fe0f} Provider rejected request (context overflow). Compacting and retrying..."
                            .to_string(),
                });
                tracing::warn!("Context overflow from provider: {e:#}");

                // Try to compact
                match crate::compact::compact_session_with_provider(
                    db,
                    session_id,
                    config.max_context_tokens,
                    &config.model_settings,
                    provider,
                )
                .await
                {
                    Ok(Ok(result)) => {
                        sink.emit(EngineEvent::Info {
                            message: format!(
                                "\u{2705} Compacted {} messages. Retrying...",
                                result.deleted
                            ),
                        });
                    }
                    _ => {
                        // Compaction failed or was skipped — can't recover
                        return Err(e).context(
                            "LLM inference failed (context overflow, compaction unsuccessful)",
                        );
                    }
                }

                // Re-assemble messages with compacted history
                let history = db.load_context(session_id, available).await?;
                messages = assemble_messages(&system_message, &history);
                if iteration == 0
                    && let Some(ref imgs) = pending_images
                    && !imgs.is_empty()
                    && let Some(last_user) = messages.iter_mut().rev().find(|m| m.role == "user")
                {
                    last_user.images = Some(imgs.clone());
                }
                let new_used = estimate_tokens(&messages);
                crate::context::update(new_used, config.max_context_tokens);

                // Retry once
                sink.emit(EngineEvent::SpinnerStart {
                    message: "Retrying...".into(),
                });
                tokio::select! {
                    result = provider.chat_stream(&messages, tool_defs, &config.model_settings) => {
                        result.context("LLM inference failed after compaction retry")?
                    }
                    _ = cancel.cancelled() => {
                        sink.emit(EngineEvent::SpinnerStop);
                        sink.emit(EngineEvent::Warn {
                            message: "Interrupted".into(),
                        });
                        return Ok(());
                    }
                }
            }
            Err(e) => {
                return Err(e).context("LLM inference failed");
            }
        };

        // Collect the streamed response
        let mut full_text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut usage = crate::providers::TokenUsage::default();
        let mut first_token = true;
        let mut char_count: usize = 0;
        let mut native_think_buf = String::new();
        let mut response_banner_shown = false;
        let mut thinking_banner_shown = false;
        let mut interrupted = false;

        loop {
            let chunk = tokio::select! {
                c = rx.recv() => c,
                _ = cancel.cancelled() => {
                    interrupted = true;
                    None
                }
            };

            if interrupted || cancel.is_cancelled() {
                sink.emit(EngineEvent::SpinnerStop);
                if !full_text.is_empty() {
                    sink.emit(EngineEvent::TextDone);
                }
                sink.emit(EngineEvent::Warn {
                    message: "Interrupted".into(),
                });
                if !full_text.is_empty() {
                    db.insert_message(
                        session_id,
                        &Role::Assistant,
                        Some(&full_text),
                        None,
                        None,
                        None,
                    )
                    .await?;
                }

                return Ok(());
            }

            let Some(chunk) = chunk else { break };

            match chunk {
                StreamChunk::TextDelta(delta) => {
                    if first_token {
                        // Close any open thinking block (content already streamed)
                        if !native_think_buf.is_empty() {
                            sink.emit(EngineEvent::SpinnerStop);
                            sink.emit(EngineEvent::ThinkingDone);
                            native_think_buf.clear();
                            thinking_banner_shown = true;
                        }
                        sink.emit(EngineEvent::SpinnerStop);
                        first_token = false;
                    }

                    // Show response banner if coming from thinking
                    if thinking_banner_shown && !response_banner_shown && !delta.trim().is_empty() {
                        sink.emit(EngineEvent::ResponseStart);
                        response_banner_shown = true;
                    }

                    // Show response banner on first non-empty text
                    if !response_banner_shown && !delta.trim().is_empty() {
                        sink.emit(EngineEvent::ResponseStart);
                        response_banner_shown = true;
                    }

                    full_text.push_str(&delta);
                    char_count += delta.len();
                    sink.emit(EngineEvent::TextDelta {
                        text: delta.clone(),
                    });
                }
                StreamChunk::ThinkingDelta(delta) => {
                    // Buffer thinking — emit as a block when text or tool calls start
                    if !thinking_banner_shown {
                        sink.emit(EngineEvent::SpinnerStop);
                        sink.emit(EngineEvent::ThinkingStart);
                        thinking_banner_shown = true;
                    }
                    sink.emit(EngineEvent::ThinkingDelta {
                        text: delta.clone(),
                    });
                    native_think_buf.push_str(&delta);
                }
                StreamChunk::ToolCalls(tcs) => {
                    if !native_think_buf.is_empty() {
                        sink.emit(EngineEvent::SpinnerStop);
                        sink.emit(EngineEvent::ThinkingDone);
                        native_think_buf.clear();
                    }
                    sink.emit(EngineEvent::SpinnerStop);
                    // Normalize tool names (small models send lowercase)
                    tool_calls = tcs
                        .into_iter()
                        .map(|mut tc| {
                            tc.function_name = tools::normalize_tool_name(&tc.function_name);
                            tc
                        })
                        .collect();
                }
                StreamChunk::Done(u) => {
                    // Close any open thinking block (content already streamed)
                    if !native_think_buf.is_empty() {
                        sink.emit(EngineEvent::SpinnerStop);
                        sink.emit(EngineEvent::ThinkingDone);
                        native_think_buf.clear();
                    }
                    usage = u;
                    break;
                }
            }
        }

        // Flush remaining text
        sink.emit(EngineEvent::TextDone);

        // If we never showed the AGENT RESPONSE banner (no text or only thinking),
        // and there's non-thinking text, show it now
        // (This is handled inline during streaming above)

        if first_token {
            sink.emit(EngineEvent::SpinnerStop);
        }

        // Log the assistant response
        let content = if full_text.is_empty() {
            None
        } else {
            Some(full_text.as_str())
        };
        let tool_calls_json = if tool_calls.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&tool_calls)?)
        };

        db.insert_message(
            session_id,
            &Role::Assistant,
            content,
            tool_calls_json.as_deref(),
            None,
            Some(&usage),
        )
        .await?;

        // Advance phase tracker based on structural turn signal
        {
            let tool_names: Vec<&str> = tool_calls
                .iter()
                .map(|tc| tc.function_name.as_str())
                .collect();
            let after_bash = recent_tool_names.iter().rev().take(3).any(|n| n == "Bash");
            let signal = TurnSignal {
                has_tool_calls: !tool_calls.is_empty(),
                tool_type: ToolType::classify(&tool_names),
                after_bash,
            };
            if let Some(transition) = phase_tracker.advance(&signal) {
                // Record auto transition (no human intervention at this gate)
                // TODO(#320 Phase 6): record_override when plan approval (#217)
                // gates are wired — requires approval results to flow back.
                intervention_observer.record_auto(transition.to);

                // Persist to flow log (survives compaction)
                let _ = db
                    .insert_phase_transition(
                        session_id,
                        iteration,
                        &transition.from.to_string(),
                        &transition.to.to_string(),
                        Some(transition.trigger),
                    )
                    .await;

                // Log phase transition as a Role::Phase message
                let _ = db
                    .insert_message(
                        session_id,
                        &crate::db::Role::Phase,
                        Some(&transition.as_message_content()),
                        None,
                        None,
                        None,
                    )
                    .await;
            }

            // Escalation check: if we're in Executing and tool output
            // suggests scope changed, demote to Understanding.
            if matches!(
                phase_tracker.current(),
                crate::task_phase::TaskPhase::Executing | crate::task_phase::TaskPhase::Verifying
            ) {
                // Check recent tool results for escalation signals
                let recent = db.load_context(session_id, 2000).await.unwrap_or_default();
                let last_tool_output = recent
                    .iter()
                    .rev()
                    .find(|m| m.role == crate::persistence::Role::Tool)
                    .and_then(|m| m.content.as_deref())
                    .unwrap_or("");

                if let crate::escalation::EscalationSignal::Escalate { reason, .. } =
                    crate::escalation::classify_error("tool", last_tool_output)
                    && let Some(transition) = phase_tracker.demote_to_understanding("escalation")
                {
                    intervention_observer.record_auto(transition.to);
                    let _ = db
                        .insert_phase_transition(
                            session_id,
                            iteration,
                            &transition.from.to_string(),
                            &transition.to.to_string(),
                            Some("error_escalation"),
                        )
                        .await;

                    // Inject reflection prompt
                    let prompt = crate::escalation::escalation_prompt("tool", &reason);
                    let _ = db
                        .insert_message(
                            session_id,
                            &crate::persistence::Role::Phase,
                            Some(&prompt),
                            None,
                            None,
                            None,
                        )
                        .await;
                }
            }
        }

        // If no tool calls, we already streamed the response — done
        if tool_calls.is_empty() {
            if made_tool_calls && full_text.trim().is_empty() {
                sink.emit(EngineEvent::Warn {
                    message: "Model produced an empty response after tool use — it may have given up mid-task. Try rephrasing or switching to a more capable model.".into(),
                });
            }
            total_prompt_tokens += usage.prompt_tokens;
            total_completion_tokens += usage.completion_tokens;
            total_cache_read_tokens += usage.cache_read_tokens;
            total_thinking_tokens += usage.thinking_tokens;
            total_char_count += char_count;

            // Use provider token count, or estimate from char count
            let display_tokens = if total_completion_tokens > 0 {
                total_completion_tokens
            } else {
                (total_char_count / 4) as i64
            };

            let total_elapsed = loop_start.elapsed();
            let total_secs = total_elapsed.as_secs_f64();
            let rate = if total_secs > 0.0 && display_tokens > 0 {
                display_tokens as f64 / total_secs
            } else {
                0.0
            };

            let context = crate::context::format_footer();

            sink.emit(EngineEvent::Footer {
                prompt_tokens: total_prompt_tokens,
                completion_tokens: total_completion_tokens,
                cache_read_tokens: total_cache_read_tokens,
                thinking_tokens: total_thinking_tokens,
                total_chars: total_char_count,
                elapsed_ms: total_elapsed.as_millis() as u64,
                rate,
                context,
            });

            return Ok(());
        }

        // Accumulate token usage across iterations
        total_prompt_tokens += usage.prompt_tokens;
        total_completion_tokens += usage.completion_tokens;
        total_cache_read_tokens += usage.cache_read_tokens;
        total_thinking_tokens += usage.thinking_tokens;
        total_char_count += char_count;

        made_tool_calls = true;

        // Git checkpoint before tool execution (crash-safe undo)
        let _checkpoint_sha = crate::git::checkpoint(project_root);

        // Execute tool calls — parallelize when possible
        // (Lite tier models must use sequential to avoid confusion)
        let pi = PhaseInfo::from(&phase_tracker);
        if tool_calls.len() > 1
            && config.model_tier.allows_parallel_tools()
            && can_parallelize(&tool_calls, mode, pi, project_root)
        {
            execute_tools_parallel(
                &tool_calls,
                project_root,
                config,
                db,
                session_id,
                tools,
                mode,
                sink,
                cancel.clone(),
                &sub_agent_cache,
                pi,
            )
            .await?;
        } else if tool_calls.len() > 1 && config.model_tier.allows_parallel_tools() {
            // Mixed batch: some tools need confirmation, but parallelizable
            // ones (like InvokeAgent) can still run concurrently.
            execute_tools_split_batch(
                &tool_calls,
                project_root,
                config,
                db,
                session_id,
                tools,
                mode,
                settings,
                sink,
                cancel.clone(),
                cmd_rx,
                &sub_agent_cache,
                pi,
            )
            .await?;
        } else {
            execute_tools_sequential(
                &tool_calls,
                project_root,
                config,
                db,
                session_id,
                tools,
                mode,
                settings,
                sink,
                cancel.clone(),
                cmd_rx,
                &sub_agent_cache,
                pi,
            )
            .await?;
        }

        // Track tool names for task phase detection
        // and observe tool call quality for tier adaptation.
        for tc in &tool_calls {
            recent_tool_names.push(tc.function_name.clone());

            // Observe: is the tool name known?
            let name_valid = tools.has_tool(&tc.function_name);
            // Observe: do the arguments parse as valid JSON?
            let args_valid = serde_json::from_str::<serde_json::Value>(&tc.arguments).is_ok();

            let outcome = if !name_valid {
                crate::tier_observer::ToolCallOutcome::UnknownTool
            } else if !args_valid {
                crate::tier_observer::ToolCallOutcome::MalformedArgs
            } else {
                crate::tier_observer::ToolCallOutcome::Valid
            };
            tier_observer.record_tool_call(outcome);
        }
        tier_observer.end_turn();

        // Loop detection: same tool+args repeated REPEAT_THRESHOLD times → stop immediately.
        if let Some(fp) = loop_detector.record(&tool_calls) {
            let culprit = fp.split(':').next().unwrap_or("unknown");
            sink.emit(EngineEvent::Warn {
                message: format!(
                    "Loop detected: '{culprit}' is repeating with identical arguments. \
                     Stopping to avoid wasted work. Rephrase the task or check for ambiguity."
                ),
            });
            break Ok(());
        }

        iteration += 1;
    }
}
