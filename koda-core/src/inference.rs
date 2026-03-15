//! LLM inference loop with streaming, tool execution, and sub-agent delegation.
//!
//! Runs the streaming inference → tool execution → re-inference loop
//! until the LLM produces a final text response.

use crate::approval::ApprovalMode;
use crate::config::KodaConfig;
use crate::db::{Database, Role};
use crate::engine::{EngineCommand, EngineEvent, EngineSink};
use crate::inference_helpers::{
    PREFLIGHT_COMPACT_THRESHOLD, RATE_LIMIT_MAX_RETRIES, assemble_messages, estimate_tokens,
    is_context_overflow_error, is_rate_limit_error, rate_limit_backoff,
};
use crate::loop_guard::LoopDetector;
use crate::persistence::Persistence;
use crate::providers::{
    ChatMessage, ImageData, LlmProvider, StreamChunk, TokenUsage, ToolCall, ToolDefinition,
};
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

// ---------------------------------------------------------------------------
// Inference loop helpers (tightly coupled to inference_loop — live here)
// ---------------------------------------------------------------------------

/// Result of collecting a streamed LLM response.
struct StreamResult {
    /// Accumulated text content from the response.
    text: String,
    /// Tool calls requested by the model.
    tool_calls: Vec<ToolCall>,
    /// Token usage statistics.
    usage: TokenUsage,
    /// Total character count of text deltas.
    char_count: usize,
    /// Whether the stream was interrupted by cancellation.
    interrupted: bool,
}

/// Load conversation history, assemble messages with the system prompt,
/// attach pending images (first iteration only), and update context tracking.
///
/// This is the single source of truth for context assembly — called on initial
/// build, after pre-flight compaction, and after overflow recovery.
async fn assemble_context(
    db: &Database,
    session_id: &str,
    system_message: &ChatMessage,
    pending_images: Option<&[ImageData]>,
    iteration: u32,
    max_context_tokens: usize,
) -> Result<Vec<ChatMessage>> {
    let history = db.load_context(session_id).await?;
    let mut messages = assemble_messages(system_message, &history);

    // Attach pending images to the last user message (first iteration only)
    if iteration == 0 {
        if let Some(imgs) = pending_images {
            if !imgs.is_empty() {
                if let Some(last_user) = messages.iter_mut().rev().find(|m| m.role == "user") {
                    last_user.images = Some(imgs.to_vec());
                }
            }
        }
    }

    let context_used = estimate_tokens(&messages);
    crate::context::update(context_used, max_context_tokens);

    Ok(messages)
}

/// Pre-flight budget check: if context usage exceeds the threshold, compact
/// before sending to the provider. Re-assembles context after successful compaction.
///
/// Returns the (possibly updated) message vec.
async fn preflight_compact_if_needed(
    messages: Vec<ChatMessage>,
    db: &Database,
    session_id: &str,
    system_message: &ChatMessage,
    pending_images: Option<&[ImageData]>,
    iteration: u32,
    config: &KodaConfig,
    provider: &dyn LlmProvider,
    sink: &dyn EngineSink,
) -> Result<Vec<ChatMessage>> {
    let ctx_pct = crate::context::percentage();
    if ctx_pct < PREFLIGHT_COMPACT_THRESHOLD {
        return Ok(messages);
    }

    tracing::warn!("Pre-flight: context at {ctx_pct}%, attempting auto-compact");
    sink.emit(EngineEvent::Info {
        message: format!("\u{1f4e6} Context at {ctx_pct}% \u{2014} compacting before sending..."),
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
            assemble_context(
                db,
                session_id,
                system_message,
                pending_images,
                iteration,
                config.max_context_tokens,
            )
            .await
        }
        Ok(Err(skip)) => {
            tracing::info!("Pre-flight compact skipped: {skip:?}");
            if matches!(skip, crate::compact::CompactSkip::HistoryTooLarge) {
                sink.emit(EngineEvent::Warn {
                    message: "\u{26a0}\u{fe0f} Context is full but history is too large for \
                              this model to summarize. Start a new session (/session) or \
                              switch to a model with a larger context window."
                        .to_string(),
                });
            }
            Ok(messages)
        }
        Err(e) => {
            tracing::warn!("Pre-flight compact failed: {e:#}");
            sink.emit(EngineEvent::Warn {
                message: format!("Compact failed: {e:#}. Continuing anyway..."),
            });
            Ok(messages)
        }
    }
}

/// Attempt to start a chat stream with exponential backoff on rate limits.
///
/// Returns `Ok(Some(rx))` on success, `Ok(None)` if cancelled during retries,
/// or `Err` for non-retriable failures.
async fn try_with_rate_limit(
    provider: &dyn LlmProvider,
    messages: &[ChatMessage],
    tool_defs: &[ToolDefinition],
    model_settings: &crate::config::ModelSettings,
    cancel: &CancellationToken,
    sink: &dyn EngineSink,
) -> Result<Option<mpsc::Receiver<StreamChunk>>> {
    let mut last_err = None;
    for attempt in 0..RATE_LIMIT_MAX_RETRIES {
        let result = tokio::select! {
            result = provider.chat_stream(messages, tool_defs, model_settings) => result,
            _ = cancel.cancelled() => return Ok(None),
        };
        match result {
            Ok(rx) => return Ok(Some(rx)),
            Err(e) if is_rate_limit_error(&e) && attempt + 1 < RATE_LIMIT_MAX_RETRIES => {
                let delay = rate_limit_backoff(attempt);
                sink.emit(EngineEvent::SpinnerStop);
                sink.emit(EngineEvent::Warn {
                    message: format!("\u{23f3} Rate limited. Retrying in {}s...", delay.as_secs()),
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
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("Rate limit retries exhausted")))
}

/// Recover from a context overflow error: compact the session, re-assemble
/// context, and retry the provider call once.
///
/// Returns `Ok(Some((rx, messages)))` on success (receiver + updated messages),
/// `Ok(None)` if cancelled during retry, or `Err` if compaction/retry fails.
async fn try_overflow_recovery(
    original_err: anyhow::Error,
    db: &Database,
    session_id: &str,
    system_message: &ChatMessage,
    pending_images: Option<&[ImageData]>,
    iteration: u32,
    config: &KodaConfig,
    provider: &dyn LlmProvider,
    tool_defs: &[ToolDefinition],
    cancel: &CancellationToken,
    sink: &dyn EngineSink,
) -> Result<Option<(mpsc::Receiver<StreamChunk>, Vec<ChatMessage>)>> {
    sink.emit(EngineEvent::SpinnerStop);
    sink.emit(EngineEvent::Warn {
        message: "\u{26a0}\u{fe0f} Provider rejected request (context overflow). \
             Compacting and retrying..."
            .to_string(),
    });
    tracing::warn!("Context overflow from provider: {original_err:#}");

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
            return Err(original_err)
                .context("LLM inference failed (context overflow, compaction unsuccessful)");
        }
    }

    let messages = assemble_context(
        db,
        session_id,
        system_message,
        pending_images,
        iteration,
        config.max_context_tokens,
    )
    .await?;

    sink.emit(EngineEvent::SpinnerStart {
        message: "Retrying...".into(),
    });
    let rx = tokio::select! {
        result = provider.chat_stream(&messages, tool_defs, &config.model_settings) => {
            result.context("LLM inference failed after compaction retry")?
        }
        _ = cancel.cancelled() => return Ok(None),
    };
    Ok(Some((rx, messages)))
}

/// Collect a streamed LLM response, emitting engine events for thinking/text/tool calls.
///
/// Handles thinking ↔ response state transitions, cancellation via `CancellationToken`,
/// and spinner lifecycle. Returns a `StreamResult` — the caller is responsible for
/// persistence and early-return on interruption.
async fn collect_stream(
    rx: &mut mpsc::Receiver<StreamChunk>,
    sink: &dyn EngineSink,
    cancel: &CancellationToken,
) -> StreamResult {
    let mut full_text = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut usage = TokenUsage::default();
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
            return StreamResult {
                text: full_text,
                tool_calls,
                usage,
                char_count,
                interrupted: true,
            };
        }

        let Some(chunk) = chunk else { break };

        match chunk {
            StreamChunk::TextDelta(delta) => {
                if first_token {
                    if !native_think_buf.is_empty() {
                        sink.emit(EngineEvent::SpinnerStop);
                        sink.emit(EngineEvent::ThinkingDone);
                        native_think_buf.clear();
                        thinking_banner_shown = true;
                    }
                    sink.emit(EngineEvent::SpinnerStop);
                    first_token = false;
                }

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
                tool_calls = tcs;
            }
            StreamChunk::Done(u) => {
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

    sink.emit(EngineEvent::TextDone);

    if first_token {
        sink.emit(EngineEvent::SpinnerStop);
    }

    StreamResult {
        text: full_text,
        tool_calls,
        usage,
        char_count,
        interrupted: false,
    }
}

// ---------------------------------------------------------------------------
// Inference loop
// ---------------------------------------------------------------------------

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
    pub tool_defs: &'a [ToolDefinition],
    /// Images attached to the current prompt (consumed on first turn).
    pub pending_images: Option<Vec<ImageData>>,
    /// Current approval mode.
    pub mode: ApprovalMode,
    /// User settings (may be mutated for auto-compact).
    pub settings: &'a mut Settings,
    /// Event sink for streaming output to the client.
    pub sink: &'a dyn EngineSink,
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
        let messages = assemble_context(
            db,
            session_id,
            &system_message,
            pending_images.as_deref(),
            iteration,
            config.max_context_tokens,
        )
        .await?;

        // Pre-flight budget check: if context is critically high, compact first
        let mut messages = preflight_compact_if_needed(
            messages,
            db,
            session_id,
            &system_message,
            pending_images.as_deref(),
            iteration,
            config,
            provider,
            sink,
        )
        .await?;

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

        // Persist the assistant response
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
            if usage.stop_reason == "max_tokens" {
                sink.emit(EngineEvent::Warn {
                    message: format!(
                        "Model {} hit max_tokens limit — response was truncated. \
                         The context may be too large. Try /compact or start a new session.",
                        config.model,
                    ),
                });
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
