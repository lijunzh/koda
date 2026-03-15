//! LLM inference loop with streaming, tool execution, and sub-agent delegation.
//!
//! Runs the streaming inference → tool execution → re-inference loop
//! until the LLM produces a final text response.

use crate::approval::ApprovalMode;
use crate::config::KodaConfig;
use crate::db::{Database, Role};
use crate::engine::{EngineCommand, EngineEvent};
use crate::inference_helpers::{
    PREFLIGHT_COMPACT_THRESHOLD, assemble_context, collect_stream,
    is_context_overflow_error, try_overflow_recovery, try_with_rate_limit,
};
use crate::loop_guard::LoopDetector;
use crate::persistence::Persistence;
use crate::providers::{ChatMessage, ImageData, LlmProvider};
use crate::settings::Settings;
use crate::tool_dispatch::{
    can_parallelize, execute_tools_parallel, execute_tools_sequential, execute_tools_split_batch,
};
use crate::tools::ToolRegistry;

use anyhow::{Context, Result};
use std::path::Path;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// All parameters for the inference loop, bundled into a single struct.
pub struct InferenceContext<'a> {
    /// Project root directory.
    pub project_root: &'a Path,
    /// Global configuration.
    pub config: &'a KodaConfig,
    /// Database handle for message persistence.
    pub db: &'a Database,
    /// Current session identifier.
    pub session_id: &'a str,
    /// System prompt for this session.
    pub system_prompt: &'a str,
    /// LLM provider to use.
    pub provider: &'a dyn LlmProvider,
    /// Tool registry with all available tools.
    pub tools: &'a ToolRegistry,
    /// Pre-computed tool definitions sent to the LLM.
    pub tool_defs: &'a [crate::providers::ToolDefinition],
    /// Images attached to the current prompt (consumed on first turn).
    pub pending_images: Option<Vec<ImageData>>,
    /// Current approval mode.
    pub mode: ApprovalMode,
    /// User settings (may be mutated for auto-compact).
    pub settings: &'a mut Settings,
    /// Event sink for streaming output to the client.
    pub sink: &'a dyn crate::engine::EngineSink,
    /// Cancellation token for graceful interruption.
    pub cancel: CancellationToken,
    /// Channel for receiving client commands (approval responses, etc.).
    pub cmd_rx: &'a mut mpsc::Receiver<EngineCommand>,
}

/// Run the inference loop: send messages, stream responses, dispatch tool calls.
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

    // Hard cap is configurable per-agent; user can extend it interactively.
    let mut hard_cap = config.max_iterations;
    let mut iteration = 0u32;
    let mut made_tool_calls = false;
    let mut retried_empty = false;
    let mut loop_detector = LoopDetector::new();
    let sub_agent_cache = crate::sub_agent_cache::SubAgentCache::new();
    let mut total_prompt_tokens: i64 = 0;
    let mut total_completion_tokens: i64 = 0;
    let mut total_cache_read_tokens: i64 = 0;
    let mut total_thinking_tokens: i64 = 0;
    let mut total_char_count: usize = 0;
    let loop_start = Instant::now();

    // Pre-build the base system message (avoids re-cloning 4-8KB per iteration)
    let base_system_prompt = system_prompt.to_string();

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

        // Build system prompt with progress + git context
        let progress = crate::progress::get_progress_summary(db, session_id)
            .await
            .unwrap_or_default();
        let git_line = crate::git::git_context(project_root)
            .map(|ctx| format!("\n{ctx}"))
            .unwrap_or_default();
        let system_prompt_full = format!("{base_system_prompt}{progress}{git_line}");
        let system_message = ChatMessage::text("system", &system_prompt_full);

        // Assemble context (load history, attach images, track usage)
        let mut messages = assemble_context(
            db,
            session_id,
            &system_message,
            pending_images.as_deref(),
            iteration,
            config.max_context_tokens,
        )
        .await?;

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
                    messages = assemble_context(
                        db,
                        session_id,
                        &system_message,
                        pending_images.as_deref(),
                        iteration,
                        config.max_context_tokens,
                    )
                    .await?;
                }
                Ok(Err(skip)) => {
                    tracing::info!("Pre-flight compact skipped: {skip:?}");
                    if matches!(skip, crate::compact::CompactSkip::HistoryTooLarge) {
                        sink.emit(EngineEvent::Warn {
                            message: "\u{26a0}\u{fe0f} Context is full but history is too large for this model to summarize. \
                                      Start a new session (/session) or switch to a model with a larger context window."
                                .to_string(),
                        });
                    }
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

        let stream_result = try_with_rate_limit(
            provider,
            &messages,
            tool_defs,
            &config.model_settings,
            &cancel,
            sink,
        )
        .await;

        // Handle cancellation during rate limit retries
        let stream_result = match stream_result {
            Ok(Some(rx)) => Ok(rx),
            Ok(None) => {
                // Cancelled during retry
                sink.emit(EngineEvent::SpinnerStop);
                sink.emit(EngineEvent::Warn {
                    message: "Interrupted".into(),
                });
                return Ok(());
            }
            Err(e) => Err(e),
        };

        // Graceful recovery: if the provider returns a context-overflow error,
        // compact and retry once before giving up.
        let mut rx = match stream_result {
            Ok(rx) => rx,
            Err(e) if is_context_overflow_error(&e) => {
                match try_overflow_recovery(
                    e,
                    db,
                    session_id,
                    &system_message,
                    pending_images.as_deref(),
                    iteration,
                    config,
                    provider,
                    tool_defs,
                    &cancel,
                    sink,
                )
                .await?
                {
                    Some((rx, updated)) => {
                        messages = updated;
                        rx
                    }
                    None => {
                        // Cancelled during overflow recovery retry
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
        let stream_result = collect_stream(&mut rx, sink, &cancel).await;

        if stream_result.interrupted {
            // Persist partial text if any, then exit
            if !stream_result.text.is_empty() {
                db.insert_message(
                    session_id,
                    &Role::Assistant,
                    Some(&stream_result.text),
                    None,
                    None,
                    None,
                )
                .await?;
            }
            return Ok(());
        }

        let full_text = stream_result.text;
        let tool_calls = stream_result.tool_calls;
        let usage = stream_result.usage;
        let char_count = stream_result.char_count;

        // Empty response after tool use — retry once before giving up.
        // Don't save the empty message so the model sees the same context on retry.
        if tool_calls.is_empty()
            && made_tool_calls
            && full_text.trim().is_empty()
            && usage.stop_reason != "max_tokens"
            && !retried_empty
        {
            retried_empty = true;
            sink.emit(EngineEvent::SpinnerStart {
                message: "Empty response — retrying...".into(),
            });
            continue;
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

        // If no tool calls, we already streamed the response — done
        if tool_calls.is_empty() {
            // Detect why the model stopped
            if usage.stop_reason == "max_tokens" {
                // Model was truncated — it didn't finish, it ran out of output tokens
                sink.emit(EngineEvent::Warn {
                    message: format!(
                        "Model {} hit max_tokens limit — response was truncated. \
                         The context may be too large. Try /compact or start a new session.",
                        config.model,
                    ),
                });
                // Don't end the turn — continue so the model can try again
                // with the truncated response in context
                continue;
            } else if made_tool_calls && full_text.trim().is_empty() {
                sink.emit(EngineEvent::Warn {
                    message: format!(
                        "Model {} produced an empty response after tool use. \
                         Try rephrasing, run /compact, or switch models with /model.",
                        config.model,
                    ),
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

        // Execute tool calls — parallelize when possible
        if tool_calls.len() > 1 && can_parallelize(&tool_calls, mode, project_root) {
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
            )
            .await?;
        } else if tool_calls.len() > 1 {
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
            )
            .await?;
        }

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
